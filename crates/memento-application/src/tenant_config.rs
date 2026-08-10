//! Per-tenant configuration (T-063, REQ-TA-007, REQ-ML-003).
//!
//! The per-tenant config file is `<root>/db/tenants/<tid>/config.toml`
//! (design D8). Batch 6 seeds it with provisioning metadata
//! (`[tenant] name = "..."`); this module adds the retention override:
//!
//! ```toml
//! [tenant]
//! name = "Mi tenant"
//!
//! [retention]
//! days = 90        # 0 = retention disabled (opt-out); missing = default 30
//! ```
//!
//! * Missing file / missing `[retention]` table → [`DEFAULT_RETENTION_DAYS`]
//!   (30). Existing tenants get the default on their next read — no
//!   migration step (REQ-ML-003 scenario 1).
//! * `days = 0` → retention disabled (opt-out, REQ-ML-003 scenario 3).
//! * The file is written ONLY by provisioning (batch 6) and by this module
//!   (the CLI override lands in T-082), so the hand-rolled TOML subset is
//!   safe; `name` is preserved verbatim (round-trip via the escaping rules
//!   of the tenant crate's `toml_basic_string`).

use memento_domain::{DomainError, TenantId};
use std::path::{Path, PathBuf};

/// Privacy-forward default horizon (REQ-CG-002: 30 days, locked decision).
pub const DEFAULT_RETENTION_DAYS: u64 = 30;
/// `days = 0` means retention disabled (explicit opt-out, REQ-ML-003).
pub const RETENTION_DISABLED: u64 = 0;

/// The per-tenant configuration as read from disk. `retention_days` is the
/// effective horizon: `0` = disabled, otherwise days (default 30).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TenantConfig {
    pub retention_days: u64,
}

impl Default for TenantConfig {
    fn default() -> Self {
        Self {
            retention_days: DEFAULT_RETENTION_DAYS,
        }
    }
}

/// The config path for a tenant: `<root>/db/tenants/<tid>/config.toml`.
pub fn tenant_config_path(root: &Path, tenant_id: &TenantId) -> PathBuf {
    root.join("db")
        .join("tenants")
        .join(tenant_id.to_string())
        .join("config.toml")
}

/// Read the effective tenant configuration. A missing file, a missing
/// `[retention]` table, or a corrupt entry resolves to the 30-day default —
/// never an error (REQ-ML-003 "existing tenants get 30d default on next
/// read").
pub fn read_tenant_config(root: &Path, tenant_id: &TenantId) -> TenantConfig {
    let raw = match std::fs::read_to_string(tenant_config_path(root, tenant_id)) {
        Ok(raw) => raw,
        Err(_) => return TenantConfig::default(),
    };
    TenantConfig {
        retention_days: parse_retention_days(&raw).unwrap_or(DEFAULT_RETENTION_DAYS),
    }
}

/// Persist the retention override, preserving the `[tenant] name` line
/// verbatim (the file's only other content; written by provisioning).
///
/// # Errors
///
/// * `Io` — the file cannot be written.
pub fn write_tenant_config(
    root: &Path,
    tenant_id: &TenantId,
    config: &TenantConfig,
) -> Result<(), DomainError> {
    let path = tenant_config_path(root, tenant_id);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let name_line = existing
        .lines()
        .find(|line| line.trim_start().starts_with("name ="))
        .map(str::to_string)
        .unwrap_or_else(|| "name = \"\"".to_string());

    let content = format!(
        "[tenant]\n{name_line}\n\n[retention]\ndays = {}\n",
        config.retention_days
    );
    // Temp file + rename: the config is never observed half-written
    // (same pattern as the credential store's atomic writes).
    let dir = path.parent().ok_or_else(|| DomainError::Internal {
        message: "config path has no parent".into(),
    })?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".config-{}.tmp", std::process::id()));
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Minimal `[retention] days = N` parser over the hand-written file subset.
fn parse_retention_days(raw: &str) -> Option<u64> {
    let mut in_retention = false;
    for line in raw.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_retention = line == "[retention]";
            continue;
        }
        if in_retention {
            let Some(rest) = line.strip_prefix("days") else {
                continue;
            };
            let Some(value) = rest.trim_start().strip_prefix('=') else {
                return None; // corrupt entry → default
            };
            return value.trim().parse::<u64>().ok(); // corrupt → default
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use memento_testkit::TempStore;

    fn config_path(ts: &TempStore) -> PathBuf {
        tenant_config_path(ts.root(), ts.tenant_id())
    }

    #[test]
    fn missing_file_defaults_to_30() {
        // REQ-ML-003 scenario 1: fresh tenant, no config → 30-day default.
        let ts = TempStore::new();
        assert_eq!(
            read_tenant_config(ts.root(), ts.tenant_id()).retention_days,
            DEFAULT_RETENTION_DAYS
        );
    }

    #[test]
    fn override_round_trip_preserves_name() {
        let ts = TempStore::new();
        let tid = *ts.tenant_id();
        let dir = config_path(&ts).parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        // Simulate provisioning's write (batch 6 format).
        std::fs::write(config_path(&ts), "[tenant]\nname = \"Mi tenant\"\n").unwrap();

        write_tenant_config(ts.root(), &tid, &TenantConfig { retention_days: 90 }).unwrap();
        let raw = std::fs::read_to_string(config_path(&ts)).unwrap();
        assert!(
            raw.contains("name = \"Mi tenant\""),
            "name preserved: {raw}"
        );
        assert!(raw.contains("[retention]"), "retention table: {raw}");
        assert_eq!(
            read_tenant_config(ts.root(), &tid).retention_days,
            90,
            "override read back"
        );

        // Relaxing back to the default works too.
        write_tenant_config(ts.root(), &tid, &TenantConfig::default()).unwrap();
        assert_eq!(read_tenant_config(ts.root(), &tid).retention_days, 30);
    }

    #[test]
    fn zero_disables_retention() {
        let ts = TempStore::new();
        let tid = *ts.tenant_id();
        std::fs::create_dir_all(config_path(&ts).parent().unwrap()).unwrap();
        std::fs::write(config_path(&ts), "[tenant]\nname = \"x\"\n").unwrap();
        write_tenant_config(ts.root(), &tid, &TenantConfig { retention_days: 0 }).unwrap();
        assert_eq!(
            read_tenant_config(ts.root(), &tid).retention_days,
            RETENTION_DISABLED
        );
    }

    #[test]
    fn corrupt_entry_falls_back_to_default() {
        let ts = TempStore::new();
        let tid = *ts.tenant_id();
        std::fs::create_dir_all(config_path(&ts).parent().unwrap()).unwrap();
        std::fs::write(
            config_path(&ts),
            "[tenant]\nname = \"x\"\n\n[retention]\ndays = later\n",
        )
        .unwrap();
        assert_eq!(
            read_tenant_config(ts.root(), &tid).retention_days,
            DEFAULT_RETENTION_DAYS,
            "corrupt → privacy-forward default, never an error"
        );
    }

    #[test]
    fn other_sections_do_not_leak() {
        let ts = TempStore::new();
        let tid = *ts.tenant_id();
        std::fs::create_dir_all(config_path(&ts).parent().unwrap()).unwrap();
        std::fs::write(
            config_path(&ts),
            "[retention]\ndays = 7\n[other]\ndays = 999\n",
        )
        .unwrap();
        assert_eq!(read_tenant_config(ts.root(), &tid).retention_days, 7);
    }
}
