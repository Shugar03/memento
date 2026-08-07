//! Project identity for the code-knowledge layer (T-040).
//!
//! `project_id = hex(sha256(canonical root path))[0..16]` (design D8): stable
//! across runs on the same machine+path, deterministic, and safe to use as a
//! directory name inside a tenant's `okf-bundles/` namespace. Queries receive
//! the id back, so [`is_valid_project_id`] doubles as the guard that keeps an
//! attacker-supplied id from escaping the bundles directory (path traversal).

use sha2::{Digest, Sha256};
use std::path::Path;

/// Derive the stable project id from a repository root path.
///
/// The canonical form is hashed so two spellings of the same directory
/// (`./repo` vs `repo/../repo`) converge; the first 16 hex chars (64 bits)
/// are plenty for a per-tenant namespace of indexed projects.
pub fn project_id_from_path(path: &Path) -> String {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Whether `id` is safe to interpolate into a filesystem path.
///
/// The default ids (16 lowercase hex chars) always pass. The guard exists
/// for any future overridable ids (design: "overridable"): no separators,
/// no `.`/`..`, no control characters, bounded length.
pub fn is_valid_project_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id != "."
        && id != ".."
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_16_hex_chars() {
        let dir = tempfile::tempdir().unwrap();
        let id = project_id_from_path(dir.path());
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn id_is_stable_per_path() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            project_id_from_path(dir.path()),
            project_id_from_path(dir.path())
        );
    }

    #[test]
    fn different_paths_differ() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        assert_ne!(
            project_id_from_path(a.path()),
            project_id_from_path(b.path())
        );
    }

    #[test]
    fn default_ids_are_valid() {
        let dir = tempfile::tempdir().unwrap();
        let id = project_id_from_path(dir.path());
        assert!(is_valid_project_id(&id));
    }

    #[test]
    fn traversal_ids_are_rejected() {
        for bad in ["..", ".", "../x", "a/b", "a\\b", "a:b", "a b", "", "a/../b"] {
            assert!(!is_valid_project_id(bad), "must reject {bad:?}");
        }
    }

    #[test]
    fn overridable_shape_is_accepted() {
        for ok in ["my-project", "my_project", "proj.1", "A-B_c.d"] {
            assert!(is_valid_project_id(ok), "must accept {ok:?}");
        }
    }
}
