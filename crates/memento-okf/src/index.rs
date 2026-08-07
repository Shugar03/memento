//! L1 indexing pipeline over okf-rs (T-040).
//!
//! [`index_project`] scans a repository root with `okf_core::Project::load`
//! (`.gitignore`-aware), keeps only the MVP languages (Rust + Python —
//! REQ-CK-001), analyzes it with `okf_analyzer::analyze` (a deterministic
//! concept model with resolved `Calls`/`CalledBy` + `Imports` relationships),
//! and writes the L1 bundle under `okf-bundles/<project_id>/bundle/`.
//!
//! Files in other languages are *reported*, never silently dropped
//! (REQ-CK-001 scenario "Mixed repo": the [`IndexReport`] lists skipped
//! files and languages).
//!
//! The pipeline is synchronous work (tree-sitter parse runs inside
//! okf-rs's rayon pool); the `async` surface exists so later batches can
//! extend it with the LanceDB symbols mirror (T-041) and embedding
//! (T-043) without breaking callers.

use crate::layers::l1;
use crate::project_id::project_id_from_path;
use memento_domain::DomainError;
use okf_core::{Project, SourceFile};
use okf_parser::Language;
use std::path::Path;
use std::time::Instant;

/// Languages indexed at MVP quality (REQ-CK-001: Rust + Python only).
pub const SUPPORTED_LANGUAGES: [Language; 2] = [Language::Rust, Language::Python];

/// One skipped file: a recognized language outside the MVP scope
/// (REQ-CK-001 — no silent partial indexing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkipEntry {
    /// Path relative to the project root, `/`-separated.
    pub file: String,
    /// Human-readable language name (e.g. "JavaScript").
    pub language: String,
}

/// Honest result of one indexing run (REQ-CK-001).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexReport {
    /// Stable project identity: `sha256(path)[0..16]` (design D8).
    pub project_id: String,
    /// Every recognized source file found by the scan.
    pub files_scanned: usize,
    /// Files actually indexed (Rust + Python).
    pub files_indexed: usize,
    /// Recognized-but-unsupported files, per language.
    pub files_skipped: Vec<SkipEntry>,
    /// Extracted OKF concepts written to the L1 bundle.
    pub concept_count: usize,
    /// Wall-clock duration of the whole run.
    pub duration_ms: u64,
}

/// Split a scanned project into MVP-supported and skipped files
/// (REQ-CK-001). Order is deterministic (the scan is sorted by path).
pub fn filter_unsupported(project: &Project) -> (Vec<SourceFile>, Vec<SkipEntry>) {
    let mut supported = Vec::new();
    let mut skipped = Vec::new();
    for file in &project.files {
        if SUPPORTED_LANGUAGES.contains(&file.language) {
            supported.push(file.clone());
        } else {
            skipped.push(SkipEntry {
                file: file.relative_path.clone(),
                language: file.language.display_name().to_string(),
            });
        }
    }
    (supported, skipped)
}

/// Full L1 index: scan → filter → analyze → write bundle (T-040).
///
/// `root` is the repository root; `bundles_root` is the tenant's
/// `okf-bundles/` directory (the project id is derived and appended).
/// A project with zero supported files is reported honestly (indexed = 0,
/// skipped listed) without writing a bundle — an error would hide the
/// reason, and REQ-CK-001's report *is* the mechanism.
pub async fn index_project(root: &Path, bundles_root: &Path) -> Result<IndexReport, DomainError> {
    let started = Instant::now();

    let root = root
        .canonicalize()
        .map_err(|source| DomainError::Io { source })?;
    let project = Project::load(&root).map_err(|err| DomainError::Io {
        source: std::io::Error::other(format!("project scan failed: {err:#}")),
    })?;

    let (supported, skipped) = filter_unsupported(&project);
    let mut report = IndexReport {
        project_id: project_id_from_path(&root),
        files_scanned: project.files.len(),
        files_indexed: supported.len(),
        files_skipped: skipped,
        concept_count: 0,
        duration_ms: 0,
    };

    if supported.is_empty() {
        report.duration_ms = started.elapsed().as_millis() as u64;
        return Ok(report);
    }

    let filtered = Project {
        root: project.root.clone(),
        files: supported,
        manifest: project.manifest,
        packages: project.packages.clone(),
    };
    let result = okf_analyzer::analyze(&filtered).map_err(|err| DomainError::Parse {
        message: format!("code analysis failed: {err:#}"),
    })?;

    let bundle_dir = bundles_root.join(&report.project_id).join("bundle");
    l1::write_bundle(&result.concepts, &bundle_dir)?;

    report.concept_count = result.concepts.len();
    report.duration_ms = started.elapsed().as_millis() as u64;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A mixed-language fixture: Rust + Python (indexed) and JS + TS
    /// (skipped) — the REQ-CK-001 "Mixed repo" scenario.
    fn write_mixed_fixture(root: &Path) {
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("py")).unwrap();
        fs::create_dir_all(root.join("web")).unwrap();
        fs::write(
            root.join("src/main.rs"),
            "fn main() { println!(\"hi\"); }\nmod helpers;\n",
        )
        .unwrap();
        fs::write(
            root.join("src/helpers.rs"),
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
        )
        .unwrap();
        fs::write(
            root.join("py/app.py"),
            "def main():\n    print('hi')\n\nclass Greeter:\n    def greet(self):\n        return 'hello'\n",
        )
        .unwrap();
        fs::write(root.join("web/app.js"), "export function click() {}\n").unwrap();
        fs::write(root.join("web/app.ts"), "export const x = 1;\n").unwrap();
    }

    #[tokio::test]
    async fn mixed_repo_reports_skipped_languages() {
        let repo = tempfile::tempdir().unwrap();
        let bundles = tempfile::tempdir().unwrap();
        write_mixed_fixture(repo.path());

        let report = index_project(repo.path(), bundles.path()).await.unwrap();

        assert_eq!(report.project_id.len(), 16);
        assert_eq!(report.files_scanned, 5);
        assert_eq!(report.files_indexed, 3);
        assert_eq!(
            report.concept_count, 8,
            "modules (src, src/helpers, py/app) + fn main, fn add, fn main(py), class Greeter, method greet"
        );
        assert_eq!(
            report.files_skipped,
            vec![
                SkipEntry {
                    file: "web/app.js".into(),
                    language: "JavaScript".into(),
                },
                SkipEntry {
                    file: "web/app.ts".into(),
                    language: "TypeScript".into(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn bundle_materializes_under_project_id() {
        let repo = tempfile::tempdir().unwrap();
        let bundles = tempfile::tempdir().unwrap();
        write_mixed_fixture(repo.path());

        let report = index_project(repo.path(), bundles.path()).await.unwrap();

        let bundle = bundles.path().join(&report.project_id).join("bundle");
        let files = l1::bundle_md_files(&bundle);
        assert!(files.iter().any(|f| f == "index.md"), "root index");
        assert!(
            files.iter().any(|f| f == "functions/src/helpers/add.md"),
            "concept file for add: {files:?}"
        );
        assert!(
            files.iter().any(|f| f == "classes/py/app/Greeter.md"),
            "concept file for Greeter: {files:?}"
        );
    }

    #[tokio::test]
    async fn index_is_deterministic() {
        let repo = tempfile::tempdir().unwrap();
        let bundles_a = tempfile::tempdir().unwrap();
        let bundles_b = tempfile::tempdir().unwrap();
        write_mixed_fixture(repo.path());

        let first = index_project(repo.path(), bundles_a.path()).await.unwrap();
        let second = index_project(repo.path(), bundles_b.path()).await.unwrap();

        assert_eq!(first.project_id, second.project_id);
        assert_eq!(first.files_scanned, second.files_scanned);
        assert_eq!(first.files_indexed, second.files_indexed);
        assert_eq!(first.files_skipped, second.files_skipped);
        assert_eq!(first.concept_count, second.concept_count);

        // Byte-identical bundles across runs (no timestamps anywhere).
        let fa = l1::bundle_md_files(&bundles_a.path().join(&first.project_id).join("bundle"));
        let fb = l1::bundle_md_files(&bundles_b.path().join(&second.project_id).join("bundle"));
        assert_eq!(fa, fb);
        for rel in &fa {
            assert_eq!(
                fs::read_to_string(
                    bundles_a
                        .path()
                        .join(&first.project_id)
                        .join("bundle")
                        .join(rel)
                )
                .unwrap(),
                fs::read_to_string(
                    bundles_b
                        .path()
                        .join(&second.project_id)
                        .join("bundle")
                        .join(rel)
                )
                .unwrap(),
                "byte-identical {rel}"
            );
        }
    }

    #[tokio::test]
    async fn unsupported_only_repo_is_reported_not_failed() {
        let repo = tempfile::tempdir().unwrap();
        let bundles = tempfile::tempdir().unwrap();
        fs::write(repo.path().join("site.js"), "export const x = 1;\n").unwrap();

        let report = index_project(repo.path(), bundles.path()).await.unwrap();

        assert_eq!(report.files_indexed, 0);
        assert_eq!(report.concept_count, 0);
        assert_eq!(
            report.files_skipped,
            vec![SkipEntry {
                file: "site.js".into(),
                language: "JavaScript".into(),
            }]
        );
        // No bundle dir is written for an empty index.
        assert!(!bundles.path().join(&report.project_id).exists());
    }

    #[tokio::test]
    async fn empty_repo_indexes_to_zero() {
        let repo = tempfile::tempdir().unwrap();
        let bundles = tempfile::tempdir().unwrap();

        let report = index_project(repo.path(), bundles.path()).await.unwrap();

        assert_eq!(report.files_scanned, 0);
        assert_eq!(report.files_indexed, 0);
        assert!(report.files_skipped.is_empty());
        assert_eq!(report.concept_count, 0);
    }

    #[tokio::test]
    async fn missing_root_is_a_structured_io_error() {
        let bundles = tempfile::tempdir().unwrap();
        let err = index_project(Path::new("Z:/definitely/not/here"), bundles.path())
            .await
            .unwrap_err();
        assert_eq!(err.code(), "IO");
    }

    #[test]
    fn filter_classifies_by_language() {
        let repo = tempfile::tempdir().unwrap();
        write_mixed_fixture(repo.path());
        let project = Project::load(repo.path()).unwrap();
        let (supported, skipped) = filter_unsupported(&project);
        assert_eq!(supported.len(), 3);
        assert!(
            supported
                .iter()
                .all(|f| matches!(f.language, Language::Rust | Language::Python))
        );
        assert_eq!(skipped.len(), 2);
    }
}
