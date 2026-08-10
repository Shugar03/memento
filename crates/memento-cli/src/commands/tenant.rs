//! Tenant administration commands (T-082): create, rotate-token, delete
//! (confirmation ceremony), retention override, export, backup, restore,
//! sweep (REQ-CL-002, REQ-TA-006/007, REQ-ML-003/005, REQ-CG-001/005).
//!
//! Auth rules:
//! * `tenant create` is the BOOTSTRAP path — it runs unauthenticated and
//!   prints the generated token exactly once (REQ-TA-006: hash-only at
//!   rest).
//! * `tenant restore` is a STANDALONE offline op: it resolves the bound
//!   context (tenant id) but must NOT open the store — `AppService::open`
//!   creates `lancedb/`, which would trip the restore quiesce check
//!   (backup.rs: non-empty `lancedb/` → live store).
//! * Everything else opens the full application layer bound to the
//!   `MEMENTO_TOKEN` + `MEMENTO_AGENT_ID` env credentials.

use std::path::Path;

use clap::ArgMatches;
use memento_domain::DomainError;
use memento_i18n::{I18n, StringKey};
use memento_ports::SweepReport;
use memento_tenant::{CredentialStore, TenantResolverImpl};
use serde_json::json;

use crate::commands::confirm_ceremony;
use crate::output::{emit_json, emit_json_value};
use crate::startup::{CliApp, open};

/// `--json` (global flag) is propagated into subcommand matches.
fn is_json(m: &ArgMatches) -> bool {
    m.get_flag("json")
}

/// Dispatch the `tenant` subtree.
pub async fn run(
    sub: &ArgMatches,
    root: &Path,
    no_embeddings: bool,
    i18n: &I18n,
) -> Result<(), DomainError> {
    match sub.subcommand() {
        // Bootstrap: no credentials required (REQ-TA-006).
        Some(("create", m)) => create(m, root, i18n),
        // Standalone offline op: context only, no store open (see module
        // docs — the quiesce check must find an empty lancedb/).
        Some(("restore", m)) => {
            let ctx = TenantResolverImpl::open(root).resolve_from_env()?;
            restore(m, root, &ctx).await
        }
        _ => {
            let app = open(root, no_embeddings).await?;
            match sub.subcommand() {
                Some(("rotate-token", m)) => rotate(m, &app).await,
                Some(("delete", m)) => delete(m, &app, i18n).await,
                Some(("retention", m)) => retention(m, &app).await,
                Some(("export", m)) => export(m, &app).await,
                Some(("backup", m)) => backup(m, &app).await,
                Some(("sweep", m)) => sweep(m, &app).await,
                _ => Err(DomainError::InvalidInput {
                    message: "unknown tenant subcommand; run 'memento tenant --help'".into(),
                }),
            }
        }
    }
}

/// `tenant create --name <name>`: generate the token, persist only its
/// Argon2id hash (REQ-TA-006), print the plaintext exactly once.
fn create(m: &ArgMatches, root: &Path, i18n: &I18n) -> Result<(), DomainError> {
    let name = m.get_one::<String>("name").expect("clap: required");
    let store = CredentialStore::new(root);
    let (tenant_id, key) = store.create_tenant(name)?;

    if is_json(m) {
        emit_json(&json!({
            "tenant_id": tenant_id,
            "token": key.as_str(),
            "name": name,
        }))
    } else {
        println!("{}", i18n.t(StringKey::CliMsgTokenCreated));
        println!("tenant_id: {tenant_id}");
        println!("token:     {key}");
        Ok(())
    }
}

/// `tenant rotate-token`: replace the stored hash; the old token dies
/// immediately (restart required, risk R9 — documented).
async fn rotate(m: &ArgMatches, app: &CliApp) -> Result<(), DomainError> {
    let store = CredentialStore::new(&app.root);
    let key = store.rotate(app.ctx.tenant_id())?;

    if is_json(m) {
        emit_json(&json!({
            "tenant_id": app.ctx.tenant_id(),
            "token": key.as_str(),
        }))
    } else {
        println!("token: {}", key.as_str());
        Ok(())
    }
}

/// `tenant delete`: confirmation ceremony → data purge + crypto-shredding
/// (erase, REQ-CG-001/D4) → credential destruction (account lifecycle,
/// batch-7 design note). Without confirmation the command aborts and the
/// tenant is untouched.
async fn delete(m: &ArgMatches, app: &CliApp, i18n: &I18n) -> Result<(), DomainError> {
    confirm_ceremony(i18n, app.ctx.tenant_id(), is_json(m))?;

    let report = app.app.erase(&app.ctx).await?;
    // Account destruction: remove the credential hash + provisioning
    // config. (The tenant dir itself stays — on Windows the open LanceDB
    // store holds file locks; the store was already purged+pruned.)
    let store = CredentialStore::new(&app.root);
    let credentials_destroyed = remove_if_present(&store.credentials_path(app.ctx.tenant_id()))?;
    let _ = std::fs::remove_file(store.config_path(app.ctx.tenant_id()));

    let mut value = serde_json::to_value(&report).map_err(|err| DomainError::Internal {
        message: format!("serialize erasure report: {err}"),
    })?;
    value["credentials_destroyed"] = json!(credentials_destroyed);

    if is_json(m) {
        emit_json_value(&value);

        Ok(())
    } else {
        let tid = app.ctx.tenant_id();
        println!(
            "tenant {tid} erased: {} rows, {} backups, key destroyed: {}",
            report.deleted_count, report.backups_count, report.master_key_destroyed
        );
        Ok(())
    }
}

/// `tenant retention [--days N]`: show the effective horizon, or set it
/// (audited configuration change, REQ-CG-002; `0` disables, REQ-ML-003).
async fn retention(m: &ArgMatches, app: &CliApp) -> Result<(), DomainError> {
    match m.get_one::<String>("days") {
        Some(raw) => {
            let days: u64 = raw.parse().map_err(|_| DomainError::InvalidInput {
                message: format!("--days must be a non-negative integer, got: {raw}"),
            })?;
            app.app.set_retention_days(&app.ctx, days).await?;
            if is_json(m) {
                emit_json(&json!({ "retention_days": days, "updated": true }))
            } else {
                println!("retención: {days} días");
                Ok(())
            }
        }
        None => {
            let days = app.app.retention_days(&app.ctx)?;
            if is_json(m) {
                emit_json(&json!({ "retention_days": days }))
            } else {
                println!("retención: {days} días (0 = desactivada)");
                Ok(())
            }
        }
    }
}

/// `tenant export`: portability artifact (JSONL → tar.gz, REQ-CG-005).
async fn export(m: &ArgMatches, app: &CliApp) -> Result<(), DomainError> {
    let report = app.app.export_tenant(&app.ctx).await?;
    if is_json(m) {
        emit_json(&report)
    } else {
        println!(
            "exportado: {} ({} fragmentos)",
            report.path.display(),
            report.chunk_count
        );
        Ok(())
    }
}

/// `tenant backup`: compact → copy → encrypt (per-backup AES-256-GCM key
/// wrapped by the tenant master key, D4/REQ-ML-005).
async fn backup(m: &ArgMatches, app: &CliApp) -> Result<(), DomainError> {
    let report = app.app.backup(&app.ctx).await?;
    if is_json(m) {
        emit_json(&report)
    } else {
        println!(
            "respaldo: {} ({} fragmentos)",
            report.path.display(),
            report.chunk_count
        );
        Ok(())
    }
}

/// `tenant restore <backup-dir>`: standalone offline restore — decrypt,
/// validate (BACKUP_VERSION / tenant match), stage, then move into a
/// quiesced tenant dir (REQ-ML-005). A live store rejects the restore
/// with a structured error.
async fn restore(
    m: &ArgMatches,
    root: &Path,
    ctx: &memento_domain::TenantContext,
) -> Result<(), DomainError> {
    let dir = Path::new(m.get_one::<String>("backup-dir").expect("clap: required"));
    let report = memento_application::backup::restore_backup(root, ctx.tenant_id(), dir).await?;
    if is_json(m) {
        emit_json(&report)
    } else {
        println!("restaurado: {} fragmentos", report.chunk_count);
        Ok(())
    }
}

/// `tenant sweep`: run the retention sweep now (REQ-CL-002 trigger).
async fn sweep(m: &ArgMatches, app: &CliApp) -> Result<(), DomainError> {
    let report: SweepReport = app.app.retention_sweep(&app.ctx).await?;
    if is_json(m) {
        emit_json(&report)
    } else {
        println!("sweep: {} expirados", report.expired_count);
        Ok(())
    }
}

/// Confirmation ceremony for tenant-wide destruction lives in
/// [`crate::commands::confirm_ceremony`] (shared with `delete --tenant`).
fn remove_if_present(path: &Path) -> Result<bool, DomainError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}
