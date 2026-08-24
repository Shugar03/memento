//! Platform-agnostic pipe naming for the daemon transport (REQ-DAEMON-006/D5).
//!
//! Windows named-pipe binding lives in [`crate::daemon`]; the name derivation
//! is shared so CLI, spawner, and daemon agree on the same path.

use std::path::{Path, PathBuf};
use std::time::Duration;

use memento_domain::TenantId;
use sha2::{Digest, Sha256};

/// Default bound on daemon writes to a stalled client (S2.5).
pub const DEFAULT_PIPE_TIMEOUT: Duration = Duration::from_secs(5);

/// The deterministic pipe name for a (root, tenant) pair (D5):
/// `\\.\pipe\memento-<sha256(canonical root)[0..16]>-<tenant_id>`. The token
/// never appears in the name (D4). Root is canonicalized when possible so
/// two spellings of the same path resolve to the same daemon.
pub fn pipe_name(root: &Path, tenant_id: &TenantId) -> String {
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let mut hex16 = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        hex16.push_str(&format!("{byte:02x}"));
    }
    format!(r"\\.\pipe\memento-{hex16}-{tenant_id}")
}

/// Canonical root hashing helper (used by tests on all targets).
#[allow(dead_code)]
pub fn canonical_root(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}
