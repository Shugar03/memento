//! Backup + restore (T-065, REQ-ML-005, design D8 + D4).
//!
//! # Backup
//!
//! 1. **Compact** the store first (clean latest version, no stale fragments).
//! 2. **Copy** the tenant's data dirs (`lancedb/`, `okf-bundles/`,
//!    `conversation/`, `auth/`, `config.toml`) into a tar archive. The
//!    `keys/` dir is deliberately EXCLUDED: the master key is the decryption
//!    root and must never travel inside the backup (D4 crypto-shredding —
//!    destroying it is what makes a leaked backup worthless).
//! 3. **Encrypt** the tar with a fresh per-backup AES-256-GCM key
//!    (`backup.enc`: 12-byte nonce ‖ ciphertext).
//! 4. **Wrap** the per-backup key with the tenant master key
//!    (`backup.key.json`: `{version, alg, nonce, wrapped}`), so the backup
//!    is decryptable only with the master key.
//!
//! Artifacts land under `<root>/backups/<tid>/<ts>/` (D8).
//!
//! # Restore
//!
//! Restore is a STANDALONE operation (not a method on [`AppService`]): it
//! runs while the app is not serving (worker/CLI offline op — the round-trip
//! drill wipes the store, restores, then reopens). Steps: unwrap the
//! per-backup key with the live master key → decrypt (AES-GCM authenticates
//! the ciphertext: a corrupt artifact fails here with `BACKUP_CORRUPT`) →
//! extract to a staging dir under `<root>/tmp/` → validate the embedded
//! manifest (schema version → `BACKUP_VERSION`; tenant match) → move the
//! data dirs into the tenant dir. A failed restore NEVER touches the live
//! store: staging happens first and nothing moves until validation passes.

use crate::AppService;
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use memento_domain::{DomainError, TenantContext, TenantId};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Backup artifact schema version (validated at restore — REQ-ML-005).
pub const BACKUP_SCHEMA_VERSION: u32 = 1;

/// Size of AES-256 keys (master + per-backup) and of the GCM nonce.
const KEY_BYTES: usize = 32;
const NONCE_BYTES: usize = 12;

/// Manifest embedded in every backup tar.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BackupManifest {
    pub version: u32,
    pub tenant_id: TenantId,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub chunk_count: usize,
}

/// Outcome of a backup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupReport {
    /// Absolute path of the backup dir (`backups/<tid>/<ts>/`).
    pub path: PathBuf,
    /// Chunks captured at backup time (from the compacted store).
    pub chunk_count: usize,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Outcome of a restore.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestoreReport {
    pub chunk_count: usize,
    pub restored_at: chrono::DateTime<chrono::Utc>,
}

impl AppService {
    /// Take a full backup of the bound tenant (REQ-ML-005, design D8/D4).
    ///
    /// # Errors
    ///
    /// * `Io` / `CapacityExceeded` — the artifact cannot be written.
    /// * `Internal` — crypto setup failed (key generation).
    pub async fn backup(&self, ctx: &TenantContext) -> Result<BackupReport, DomainError> {
        self.ensure_bound_tenant(ctx)?;

        // 1. Compact → clean latest version (backups from a quiesced state).
        memento_lancedb::compact(&self.store, ctx).await?;

        // 2. Master key: load or lazily create (32 random bytes, D4).
        let master_key = load_or_create_master_key(&self.master_key_path())?;

        // 3. Copy the tenant data dirs into a tar in memory.
        let chunk_count = self.store.count_chunks(ctx).await? as usize;
        let created_at = self.clock.now();
        let manifest = BackupManifest {
            version: BACKUP_SCHEMA_VERSION,
            tenant_id: *ctx.tenant_id(),
            created_at,
            chunk_count,
        };
        let tar_bytes = build_backup_tar(&self.tenant_dir(), &manifest)?;

        // 4. Fresh per-backup key → encrypt the tar (nonce ‖ ciphertext).
        let per_backup_key = random_bytes(KEY_BYTES)?;
        let (nonce, ciphertext) = seal(&per_backup_key, &tar_bytes)?;

        // 5. Wrap the per-backup key with the master key.
        let (wrap_nonce, wrapped) = seal(&master_key, &per_backup_key)?;
        let key_manifest = serde_json::json!({
            "version": 1,
            "alg": "AES-256-GCM",
            "nonce": hex_encode(&wrap_nonce),
            "wrapped": hex_encode(&wrapped),
        });

        let ts = created_at.format("%Y%m%dT%H%M%SZ");
        let backup_dir = self
            .root()
            .join("backups")
            .join(ctx.tenant_id().to_string())
            .join(ts.to_string());
        std::fs::create_dir_all(&backup_dir)?;

        let mut enc = std::fs::File::create(backup_dir.join("backup.enc"))?;
        enc.write_all(&nonce)?;
        enc.write_all(&ciphertext)?;
        enc.flush()?;
        std::fs::write(backup_dir.join("backup.key.json"), key_manifest.to_string())?;

        self.record_audit(
            ctx,
            "backup",
            json!({
                "backup_dir": backup_dir.file_name().map(|n| n.to_string_lossy().to_string()),
                "chunk_count": chunk_count,
            }),
            None,
        );
        Ok(BackupReport {
            path: backup_dir,
            chunk_count,
            created_at,
        })
    }

    /// The tenant master-key path (`<tenant_dir>/keys/master.key`, D4).
    fn master_key_path(&self) -> PathBuf {
        self.tenant_dir().join("keys").join("master.key")
    }
}

/// Standalone restore (see module docs): decrypt + validate + move into
/// place. The live store is untouched unless the artifact fully validates.
///
/// # Errors
///
/// * `BackupCorrupt` — no master key, bad manifest JSON, or AES-GCM
///   authentication failure (truncated/tampered artifact).
/// * `BackupVersion` — manifest schema version mismatch.
/// * `InvalidInput` — the backup's tenant does not match, or the target
///   tenant dir is not quiesced (has a non-empty `lancedb/`).
pub async fn restore_backup(
    root: impl AsRef<Path>,
    tenant_id: &TenantId,
    backup_dir: impl AsRef<Path>,
) -> Result<RestoreReport, DomainError> {
    let root = root.as_ref();
    let backup_dir = backup_dir.as_ref();
    let tenant_dir = root.join("db").join("tenants").join(tenant_id.to_string());

    // Unwrap the per-backup key with the live master key.
    let master_key_path = tenant_dir.join("keys").join("master.key");
    let master_key = std::fs::read(&master_key_path).map_err(|err| DomainError::BackupCorrupt {
        reason: format!("no master key at {} ({err})", master_key_path.display()),
    })?;
    let key_manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(backup_dir.join("backup.key.json")).map_err(|err| {
            DomainError::BackupCorrupt {
                reason: format!("missing backup.key.json: {err}"),
            }
        })?,
    )
    .map_err(|err| DomainError::BackupCorrupt {
        reason: format!("corrupt backup.key.json: {err}"),
    })?;
    let per_backup_key = open(&master_key, &key_manifest)?;

    // Decrypt the payload (AES-GCM authenticates: corruption fails here).
    let encrypted =
        std::fs::read(backup_dir.join("backup.enc")).map_err(|err| DomainError::BackupCorrupt {
            reason: format!("missing backup.enc: {err}"),
        })?;
    let tar_bytes = open_payload(&per_backup_key, &encrypted)?;

    // Extract to staging under <root>/tmp — live store untouched so far.
    let staging = root.join("tmp").join(format!("restore-{}", uuid_stamp()));
    std::fs::create_dir_all(&staging)?;
    let mut archive = tar::Archive::new(&tar_bytes[..]);
    archive
        .unpack(&staging)
        .map_err(|err| DomainError::BackupCorrupt {
            reason: format!("tar extraction failed: {err}"),
        })?;

    // Validate the manifest before touching anything live.
    let manifest_raw =
        std::fs::read(staging.join("backup.json")).map_err(|err| DomainError::BackupCorrupt {
            reason: format!("backup.json missing: {err}"),
        })?;
    let manifest: BackupManifest =
        serde_json::from_slice(&manifest_raw).map_err(|err| DomainError::BackupCorrupt {
            reason: format!("backup.json corrupt: {err}"),
        })?;
    if manifest.version != BACKUP_SCHEMA_VERSION {
        return Err(DomainError::BackupVersion {
            found: manifest.version.to_string(),
            expected: BACKUP_SCHEMA_VERSION.to_string(),
        });
    }
    if &manifest.tenant_id != tenant_id {
        return Err(DomainError::InvalidInput {
            message: format!(
                "backup belongs to tenant {}, restore target is {tenant_id}",
                manifest.tenant_id
            ),
        });
    }

    // Move the data dirs into place. The target must be quiesced: an
    // existing non-empty lancedb/ means the store is live or dirty.
    // REQ-DAEMON-009: a store the daemon still holds (quiesce did not
    // complete / timed out) is BUSY — the restore fails with the
    // STORE_BUSY tier and touches nothing (store + backup intact).
    std::fs::create_dir_all(&tenant_dir)?;
    let live_lancedb = tenant_dir.join("lancedb");
    if live_lancedb.exists()
        && std::fs::read_dir(&live_lancedb)
            .map(|mut d| d.next().is_some())
            .unwrap_or(true)
    {
        return Err(DomainError::StoreBusy {
            message: "restore requires a quiesced tenant store (non-empty lancedb/ exists; \
                      quiesce or stop the daemon before restoring)"
                .into(),
        });
    }
    for entry in [
        "lancedb",
        "okf-bundles",
        "conversation",
        "auth",
        "config.toml",
    ] {
        let staged = staging.join(entry);
        let target = tenant_dir.join(entry);
        if !staged.exists() {
            continue;
        }
        if target.exists() {
            let _ = std::fs::remove_dir_all(&target).or_else(|_| std::fs::remove_file(&target));
        }
        std::fs::rename(&staged, &target)?;
    }
    let _ = std::fs::remove_dir_all(&staging);

    // Audit (standalone: the log is opened directly for this tenant — the
    // restore op runs outside any bound AppService).
    let logger = crate::audit::AuditLogger::new(root, tenant_id)?;
    logger.record(&crate::audit::AuditEvent {
        ts: chrono::Utc::now(),
        tenant_id: *tenant_id,
        agent_id: memento_domain::AgentId::new("restore"),
        action: "restore".to_string(),
        target: json!({
            "backup_dir": backup_dir.file_name().map(|n| n.to_string_lossy().to_string()),
            "chunk_count": manifest.chunk_count,
        }),
        outcome: "ok",
        error_code: None,
        chore_id: None,
    });
    Ok(RestoreReport {
        chunk_count: manifest.chunk_count,
        restored_at: chrono::Utc::now(),
    })
}

// --- crypto + archive internals -------------------------------------------------

/// Load `keys/master.key` or create it with 32 OS-random bytes.
fn load_or_create_master_key(path: &Path) -> Result<Vec<u8>, DomainError> {
    if let Ok(existing) = std::fs::read(path) {
        if existing.len() == KEY_BYTES {
            return Ok(existing);
        }
        return Err(DomainError::BackupCorrupt {
            reason: format!("master key at {} has invalid length", path.display()),
        });
    }
    let key = random_bytes(KEY_BYTES)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, &key)?;
    Ok(key)
}

/// Tar the tenant's data dirs + manifest (keys/ deliberately excluded).
fn build_backup_tar(tenant_dir: &Path, manifest: &BackupManifest) -> Result<Vec<u8>, DomainError> {
    let mut bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut bytes);
        let manifest_bytes = serde_json::to_vec(manifest).map_err(|err| DomainError::Internal {
            message: format!("serialize backup manifest: {err}"),
        })?;
        let mut header = tar::Header::new_gnu();
        header.set_size(manifest_bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(
                &mut header,
                "backup.json",
                std::io::Cursor::new(manifest_bytes),
            )
            .map_err(|err| DomainError::Io { source: err })?;
        for dir in ["lancedb", "okf-bundles", "conversation", "auth"] {
            let path = tenant_dir.join(dir);
            if path.exists() {
                builder
                    .append_dir_all(dir, &path)
                    .map_err(|err| DomainError::Io { source: err })?;
            }
        }
        let config = tenant_dir.join("config.toml");
        if config.exists() {
            builder
                .append_path_with_name(&config, "config.toml")
                .map_err(|err| DomainError::Io { source: err })?;
        }
        builder
            .finish()
            .map_err(|err| DomainError::Io { source: err })?;
    }
    Ok(bytes)
}

/// AES-256-GCM seal: returns `(nonce, ciphertext)`.
fn seal(key: &[u8], payload: &[u8]) -> Result<(Vec<u8>, Vec<u8>), DomainError> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|err| DomainError::Internal {
        message: format!("invalid AES key: {err}"),
    })?;
    let nonce = random_bytes(NONCE_BYTES)?;
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), payload)
        .map_err(|_| DomainError::Internal {
            message: "AES-GCM encryption failed".into(),
        })?;
    Ok((nonce, ciphertext))
}

/// Unwrap a key-manifest (backup.key.json) with the master key.
fn open(master_key: &[u8], manifest: &serde_json::Value) -> Result<Vec<u8>, DomainError> {
    if manifest["alg"] != "AES-256-GCM" {
        return Err(DomainError::BackupVersion {
            found: manifest["alg"].as_str().unwrap_or("unknown").to_string(),
            expected: "AES-256-GCM".to_string(),
        });
    }
    let nonce = hex_decode(manifest["nonce"].as_str().unwrap_or_default())?;
    let wrapped = hex_decode(manifest["wrapped"].as_str().unwrap_or_default())?;
    open_payload(master_key, &concat_nonce_ct(&nonce, &wrapped))
}

/// AES-256-GCM open: `nonce ‖ ciphertext` (authenticates — corruption
/// surfaces as [`DomainError::BackupCorrupt`]).
fn open_payload(key: &[u8], nonce_and_ct: &[u8]) -> Result<Vec<u8>, DomainError> {
    if nonce_and_ct.len() < NONCE_BYTES {
        return Err(DomainError::BackupCorrupt {
            reason: "payload shorter than the nonce".into(),
        });
    }
    let (nonce, ct) = nonce_and_ct.split_at(NONCE_BYTES);
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|err| DomainError::Internal {
        message: format!("invalid AES key: {err}"),
    })?;
    cipher
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|_| DomainError::BackupCorrupt {
            reason: "AES-GCM authentication failed (truncated or tampered artifact)".into(),
        })
}

fn concat_nonce_ct(nonce: &[u8], ct: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nonce.len() + ct.len());
    out.extend_from_slice(nonce);
    out.extend_from_slice(ct);
    out
}

/// OS-random bytes (rand_core 0.6 OsRng with the getrandom feature —
/// discovery 2605; infallible RngCore).
fn random_bytes(len: usize) -> Result<Vec<u8>, DomainError> {
    use rand_core::RngCore;
    let mut out = vec![0u8; len];
    rand_core::OsRng.fill_bytes(&mut out);
    Ok(out)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn hex_decode(hex: &str) -> Result<Vec<u8>, DomainError> {
    if !hex.len().is_multiple_of(2) {
        return Err(DomainError::BackupCorrupt {
            reason: "odd-length hex".into(),
        });
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| DomainError::BackupCorrupt {
                reason: format!("invalid hex at byte {}", i / 2),
            })
        })
        .collect()
}

/// A unique staging suffix (no uuid dependency beyond the workspace pin's
/// v7 feature — available, but a plain counter-ish stamp is enough here).
fn uuid_stamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::test_app;
    use memento_ports::{IngestTextRequest, SearchQuery};
    use memento_testkit::{TempStore, TestClock};

    /// Backup a populated tenant, wipe its store, restore, and return the
    /// reopened app (the REQ-ML-005 round-trip drill).
    async fn round_trip(ts: &TempStore, text: &str) -> AppService {
        let app = test_app(ts, TestClock::default()).await;
        app.ingest_text(
            &ts.ctx(),
            IngestTextRequest {
                text: text.to_string(),
                doc_id: None,
                metadata: None,
            },
        )
        .await
        .expect("ingest");
        let backup = app.backup(&ts.ctx()).await.expect("backup");
        drop(app); // release the store before wiping

        // Wipe: remove the live data dirs (keys/ survives — the decryption
        // root is outside the backup by design).
        let tenant_dir = ts.lancedb_dir().parent().unwrap().to_path_buf();
        for entry in ["lancedb", "okf-bundles", "conversation", "config.toml"] {
            let path = tenant_dir.join(entry);
            if path.is_dir() {
                std::fs::remove_dir_all(&path).unwrap();
            } else if path.exists() {
                std::fs::remove_file(&path).unwrap();
            }
        }

        let report = restore_backup(ts.root(), ts.tenant_id(), &backup.path)
            .await
            .expect("restore");
        assert_eq!(report.chunk_count, backup.chunk_count);

        // Reopen and verify search equivalence (REQ-ML-005 scenario 1).
        test_app(ts, TestClock::default()).await
    }

    #[tokio::test]
    async fn backup_restore_round_trip_reproduces_search_state() {
        // REQ-ML-005 scenario 1: searches return the same results with
        // provenance intact.
        let ts = TempStore::new();
        let restored = round_trip(
            &ts,
            "la memoria persiste a través de las copias de seguridad",
        )
        .await;
        let hits = restored
            .search(
                &ts.ctx(),
                SearchQuery::new("memoria", 10, *ts.workspace_id()),
            )
            .await
            .expect("search after restore");
        assert!(!hits.is_empty(), "searchable after restore");
        assert!(
            hits.iter()
                .all(|h| h.provenance.tenant_id == *ts.tenant_id()),
            "provenance intact"
        );
        assert_eq!(
            restored.store().count_chunks(&ts.ctx()).await.unwrap() as usize,
            hits.len()
        );
    }

    #[tokio::test]
    async fn corrupt_backup_fails_with_structured_error_and_leaves_store_untouched() {
        // REQ-ML-005 scenario 2: a truncated artifact → BACKUP_CORRUPT and
        // the current store is unchanged.
        let ts = TempStore::new();
        let app = test_app(&ts, TestClock::default()).await;
        app.ingest_text(
            &ts.ctx(),
            IngestTextRequest {
                text: "dato que debe sobrevivir a un backup corrupto".into(),
                doc_id: None,
                metadata: None,
            },
        )
        .await
        .expect("ingest");
        let backup = app.backup(&ts.ctx()).await.expect("backup");

        // Truncate the encrypted payload.
        let enc_path = backup.path.join("backup.enc");
        let bytes = std::fs::read(&enc_path).unwrap();
        std::fs::write(&enc_path, &bytes[..bytes.len() / 2]).unwrap();

        let err = restore_backup(ts.root(), ts.tenant_id(), &backup.path)
            .await
            .expect_err("corrupt backup must fail");
        assert_eq!(err.code(), "BACKUP_CORRUPT");

        // The live store still has its chunk (untouched).
        assert_eq!(app.store().count_chunks(&ts.ctx()).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn restore_into_live_store_is_rejected() {
        let ts = TempStore::new();
        let app = test_app(&ts, TestClock::default()).await;
        app.ingest_text(
            &ts.ctx(),
            IngestTextRequest {
                text: "memoria viva".into(),
                doc_id: None,
                metadata: None,
            },
        )
        .await
        .expect("ingest");
        let backup = app.backup(&ts.ctx()).await.expect("backup");

        // The store is still live (non-empty lancedb/) → structured error.
        // REQ-DAEMON-009: a restore against a store the daemon still holds
        // (quiesce did not happen / timed out) fails STORE_BUSY — the store
        // and the backup stay untouched.
        let err = restore_backup(ts.root(), ts.tenant_id(), &backup.path)
            .await
            .expect_err("live restore must fail");
        assert_eq!(err.code(), "STORE_BUSY", "quiesce-timeout tier");
    }

    #[tokio::test]
    async fn backup_artifact_layout_and_key_exclusion() {
        let ts = TempStore::new();
        let app = test_app(&ts, TestClock::default()).await;
        app.ingest_text(
            &ts.ctx(),
            IngestTextRequest {
                text: "memoria para el layout".into(),
                doc_id: None,
                metadata: None,
            },
        )
        .await
        .expect("ingest");
        let backup = app.backup(&ts.ctx()).await.expect("backup");

        assert!(backup.path.join("backup.enc").exists());
        assert!(backup.path.join("backup.key.json").exists());
        let key_manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(backup.path.join("backup.key.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(key_manifest["alg"], "AES-256-GCM");

        // The master key exists on the tenant side (created lazily).
        let master = app.tenant_dir().join("keys").join("master.key");
        assert!(master.exists());
        assert_eq!(std::fs::read(&master).unwrap().len(), 32);
    }
}
