//! L1 layer: OKF Markdown/YAML bundles (T-040).
//!
//! A bundle is the source of truth for the code-knowledge stack: one
//! `*.md` file with YAML frontmatter per concept (kind-grouped
//! directories plus `index.md` navigation pages), in okf-generator's
//! canonical layout. `okf-rs` never stamps wall-clock times into
//! bundles (analysis leaves `generated.at` `None`), so writing the same
//! concepts twice produces byte-identical output — the determinism the
//! rest of the stack relies on.

use memento_domain::DomainError;
use okf_parser::Concept;
use std::path::Path;

/// Write the L1 bundle for `concepts` under `dir` (creating directories).
///
/// Errors (filesystem, duplicate concept ids) map to `IO` with the
/// underlying reason in the message; the bundle is written per-file, so a
/// failure never leaves a half-usable bundle behind.
pub fn write_bundle(concepts: &[Concept], dir: &Path) -> Result<(), DomainError> {
    okf_generator::write_bundle(concepts, dir).map_err(|err| DomainError::Io {
        source: std::io::Error::other(format!("bundle write failed: {err:#}")),
    })
}

/// Read a bundle back into concepts (id-sorted; malformed files skipped
/// per okf-rs's reader contract).
pub fn load_bundle(dir: &Path) -> Result<Vec<Concept>, DomainError> {
    okf_parser::read_bundle(dir).map_err(|err| DomainError::Io {
        source: std::io::Error::other(format!("bundle read failed: {err:#}")),
    })
}

/// Recursively list every `.md` file in a bundle as a relative,
/// `/`-separated path (navigation pages + concept files) — used by tests
/// to assert the bundle materialized in a platform-independent way.
pub fn bundle_md_files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                let relative = path
                    .strip_prefix(dir)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push(relative);
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layers::test_util::sample_concepts;

    #[test]
    fn bundle_round_trip_preserves_concepts() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        write_bundle(&sample_concepts(), &bundle).unwrap();

        let files = bundle_md_files(&bundle);
        assert!(files.iter().any(|f| f.ends_with("index.md")));
        assert!(files.iter().any(|f| f.ends_with("functions/alpha.md")));
        assert!(files.iter().any(|f| f.ends_with("modules/lib.md")));

        let loaded = load_bundle(&bundle).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, "functions/alpha");
        assert_eq!(loaded[1].id, "modules/lib");
        assert_eq!(
            loaded[0].signature.as_deref(),
            Some("pub fn alpha() -> u32")
        );
    }

    #[test]
    fn write_is_byte_deterministic() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let concepts = sample_concepts();
        write_bundle(&concepts, &a.path().join("bundle")).unwrap();
        write_bundle(&concepts, &b.path().join("bundle")).unwrap();

        let fa = bundle_md_files(&a.path().join("bundle"));
        let fb = bundle_md_files(&b.path().join("bundle"));
        assert_eq!(fa, fb, "same file set");
        for rel in &fa {
            let ca = std::fs::read_to_string(a.path().join("bundle").join(rel)).unwrap();
            let cb = std::fs::read_to_string(b.path().join("bundle").join(rel)).unwrap();
            assert_eq!(ca, cb, "byte-identical {rel} (no timestamps)");
        }
    }
}
