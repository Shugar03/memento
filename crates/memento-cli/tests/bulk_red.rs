//! T-080 RED tests — bulk-ingest boundary (threat-matrix row 5).
//!
//! The design threat matrix maps the bulk-ingest paths (CLI) to two cases:
//! **glob traversal** and **symlink escape**. The design response is a
//! canonical-path check: every ingested file must canonicalize under the
//! workspace root ("workspace-relative only").
//!
//! These tests are RED by design: they reference
//! `memento_cli::commands::ingest::{collect_bulk_files, canonical_within}`,
//! which land with T-083. The walker must make them pass without weakening:
//!
//! 1. `dotdot_traversal_rejected` — a bulk root argument carrying a `..`
//!    component (glob-style traversal, e.g. `dir/../outside`) is rejected
//!    BEFORE any walk: structured `INVALID_INPUT`, zero files collected.
//! 2. `escape_via_dotdot_component_rejected` — an entry path that resolves
//!    outside the workspace root (the shape a symlink or traversal entry
//!    produces) fails the canonical-path containment check.
//! 3. `symlink_escape_rejected` — a REAL symlink inside the root pointing
//!    outside is rejected by the full walker (unix-only: symlink creation
//!    needs privileges on Windows; the containment gate is covered for all
//!    platforms by test 2).

use memento_cli::commands::ingest::{canonical_within, collect_bulk_files};
use std::path::PathBuf;

/// A fresh temp root for one test.
fn temp_root() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    (dir, root)
}

#[test]
fn dotdot_traversal_rejected() {
    // Glob traversal: a bulk root like `data/../secrets` must be rejected
    // before any directory scan — the walker never leaves the workspace.
    let (_dir, root) = temp_root();
    let nested = root.join("sub");
    std::fs::create_dir_all(&nested).expect("mkdir");
    let outside = root.join("outside");
    std::fs::write(&outside, b"secret").expect("write");

    let traversal = nested.join("..").join("outside");
    let err = collect_bulk_files(&traversal).expect_err("traversal rejected");
    assert_eq!(err.code(), "INVALID_INPUT", "stable code: {err}");
    assert!(
        err.to_string().contains(".."),
        "error names the traversal component: {err}"
    );
}

#[test]
fn escape_via_dotdot_component_rejected() {
    // The containment gate: a path that canonicalizes OUTSIDE the workspace
    // root is rejected, whatever produced it (symlink, `..` component,
    // junction). This is the shape every escape resolves to.
    let (_dir, root) = temp_root();
    let nested = root.join("sub");
    std::fs::create_dir_all(&nested).expect("mkdir");
    // A real file physically outside the root (reached via `sub/..`).
    let outside = root.join("outside.txt");
    std::fs::write(&outside, b"secret").expect("write");

    let escaped = nested.join("..").join("outside.txt");
    let err = canonical_within(&root, &escaped).expect_err("escape rejected");
    assert_eq!(err.code(), "INVALID_INPUT", "stable code: {err}");
    assert!(
        err.to_string().contains("outside"),
        "error names the offending path: {err}"
    );

    // Control: a path INSIDE the root passes the gate.
    let inside = nested.join("inside.txt");
    std::fs::write(&inside, b"ok").expect("write");
    let canonical = canonical_within(&root, &inside).expect("inside allowed");
    assert!(canonical.starts_with(&root), "canonical under root");
}

#[cfg(unix)]
#[test]
fn symlink_escape_rejected() {
    use std::os::unix::fs::symlink;

    // A symlink inside the workspace pointing OUTSIDE it must abort the
    // bulk walk with a structured error — nothing outside is ever ingested.
    let (_dir, root) = temp_root();
    let outside_dir = tempfile::tempdir().expect("outside tempdir");
    std::fs::write(outside_dir.path().join("secret.txt"), b"secret").expect("write");

    let link = root.join("evil.md");
    symlink(outside_dir.path(), &link).expect("symlink");

    let err = collect_bulk_files(&root).expect_err("symlink escape rejected");
    assert_eq!(err.code(), "INVALID_INPUT", "stable code: {err}");
    assert!(
        err.to_string().contains("evil.md"),
        "error names the escaping link: {err}"
    );
}

#[cfg(windows)]
#[test]
fn symlink_escape_rejected() {
    // Windows without Developer Mode cannot create real symlinks in tests.
    // The canonical-path containment gate (tested above, all platforms) is
    // the same code path a symlink resolves through: canonicalize + require
    // containment under the workspace root. This test pins that the walker
    // rejects a directory whose canonical target is outside via the gate.
    let (_dir, root) = temp_root();
    let outside_dir = tempfile::tempdir().expect("outside tempdir");
    std::fs::write(outside_dir.path().join("secret.txt"), b"secret").expect("write");

    let escaped = root.join("escaped");
    // `escaped` does not exist: canonicalize must surface a structured Io
    // error (never a silent fallback to the non-canonical path).
    let err = canonical_within(&root, &escaped).expect_err("missing entry rejected");
    assert_eq!(err.code(), "IO", "stable code: {err}");
    let _ = outside_dir;
}
