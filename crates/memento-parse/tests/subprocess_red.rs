//! RED tests — subprocess boundary (threat matrix, design 2585 rows 1-2).
//!
//! These tests assert the isolation guarantees of the anydoc subprocess
//! boundary BEFORE the implementation exists (T-030). They deliberately do
//! not compile until T-031 lands: that compile failure IS the red state.
//!
//! Attack surface in the MVP: the only user-controlled string that reaches
//! the subprocess argv is the file *extension* carried by
//! `SourceKind::Document(ext)` (the staging filename is server-generated,
//! the staging dir is server-owned). The guards tested here:
//!
//! 1. **Path traversal** — `../`-style or absolute escape inside the
//!    extension must be rejected before any staging write or exec.
//! 2. **Argument injection** — shell metacharacters (`; & | > < ` $ \ %` …)
//!    in the extension must be rejected. argv is passed positionally with no
//!    shell; this guard is defense-in-depth (Windows spawns `.cmd` shims via
//!    `cmd.exe`, so metacharacters must never reach the command line).
//! 3. **Output bomb** — stdout growing past the configured cap must abort
//!    the conversion with `SUBPROCESS_STDOUT_OVERFLOW` and kill the child.
//! 4. **Hang** — a child that does not exit within the timeout must be
//!    killed and surfaced as `SUBPROCESS_TIMEOUT` (kill-on-timeout).
//!
//! Error codes are asserted by stable string code (memento-domain taxonomy,
//! D7) so the tests document the external contract, not implementation.

use std::time::Duration;

use memento_domain::DomainError;
use memento_parse::anydoc::{AnydocClient, AnydocCommand, AnydocConfig};
use tempfile::TempDir;

/// Builds a client pointed at the crate's fake-anydoc binary, with a staging
/// dir and limits fully controlled by the test.
fn fake_client(
    mode: &str,
    timeout: Duration,
    stdout_limit: usize,
    staging: &TempDir,
) -> AnydocClient {
    AnydocClient::new(AnydocConfig {
        command: AnydocCommand {
            program: env!("CARGO_BIN_EXE_memento-parse-fake-anydoc").to_string(),
            args: Vec::new(),
            env: vec![("FAKE_ANYDOC_MODE".to_string(), mode.to_string())],
        },
        timeout,
        stdout_limit,
        staging_dir: staging.path().to_path_buf(),
    })
}

/// Shared assertion: the error is a subprocess-argv rejection with the
/// stable `SUBPROCESS_ARGV_INVALID` code (exit 32, REQ-CL-005 matrix).
fn assert_argv_rejected(err: &DomainError) {
    assert_eq!(
        err.code(),
        "SUBPROCESS_ARGV_INVALID",
        "traversal/injection must surface the stable argv-rejection code, got: {err}"
    );
    assert_eq!(err.exit_code(), 32);
}

/// Threat matrix row 1 (adapted): path traversal must be rejected before the
/// extension ever reaches a staging path or the subprocess command line.
#[tokio::test]
async fn path_traversal_rejected() {
    let staging = TempDir::new().expect("tempdir");
    let client = fake_client("echo", Duration::from_secs(5), 1024 * 1024, &staging);

    for evil_ext in [
        "../etc/passwd",
        "..\\..\\Windows\\system32\\config",
        "a/../../b",
        "/etc/shadow",
        "C:\\evil",
    ] {
        let err = client
            .convert(b"PK\x03\x04 fake docx payload", evil_ext)
            .await
            .expect_err("traversal extension must be rejected");
        assert_argv_rejected(&err);
    }

    // Zero writes: nothing may be staged for a rejected argv.
    let leftovers: Vec<_> = std::fs::read_dir(staging.path())
        .expect("read staging")
        .collect();
    assert!(
        leftovers.is_empty(),
        "rejected argv must leave no staging files, found: {leftovers:?}"
    );
}

/// Threat matrix row 1 (adapted): shell metacharacters in the extension must
/// be rejected even though argv is positional (no shell) — defense-in-depth
/// against Windows `.cmd` shim expansion (`%`, `!`, `^`) and any future
/// command-line change.
#[tokio::test]
async fn shell_metacharacters_rejected() {
    let staging = TempDir::new().expect("tempdir");
    let client = fake_client("echo", Duration::from_secs(5), 1024 * 1024, &staging);

    for evil_ext in [
        "docx;rm -rf /",
        "docx&calc",
        "docx|whoami",
        "docx>out",
        "docx<in",
        "docx`id`",
        "docx$HOME",
        "docx\\..\\evil",
        "docx%PATH%",
        "docx!CMD!",
        "docx\"quote",
        "docx'quote",
    ] {
        let err = client
            .convert(b"blob", evil_ext)
            .await
            .expect_err("metacharacter extension must be rejected");
        assert_argv_rejected(&err);
    }
}

/// Threat matrix row 2 (adapted): a subprocess that floods stdout past the
/// configured cap must be killed and surfaced as `SUBPROCESS_STDOUT_OVERFLOW`
/// — the 50MB production cap is injected as a small limit here.
#[tokio::test]
async fn stdout_bomb_capped() {
    let staging = TempDir::new().expect("tempdir");
    // Fake emits 8 MiB of zeros; the cap is 64 KiB → must abort mid-stream.
    let client = fake_client("bomb", Duration::from_secs(30), 64 * 1024, &staging);

    let err = client
        .convert(b"blob", "docx")
        .await
        .expect_err("stdout bomb must be capped");
    assert_eq!(
        err.code(),
        "SUBPROCESS_STDOUT_OVERFLOW",
        "expected stable overflow code, got: {err}"
    );
    assert_eq!(err.exit_code(), 31);
}

/// Threat matrix row 2 (adapted): a child that never exits must be killed at
/// the timeout (kill-on-timeout) and surfaced as `SUBPROCESS_TIMEOUT`.
#[tokio::test]
async fn hang_killed_on_timeout() {
    let staging = TempDir::new().expect("tempdir");
    let client = fake_client("hang", Duration::from_millis(500), 1024 * 1024, &staging);

    let started = std::time::Instant::now();
    let err = client
        .convert(b"blob", "docx")
        .await
        .expect_err("hang must be killed at the timeout");
    let elapsed = started.elapsed();

    assert_eq!(
        err.code(),
        "SUBPROCESS_TIMEOUT",
        "expected stable timeout code, got: {err}"
    );
    assert_eq!(err.exit_code(), 30);
    // The kill must be prompt: generous bound (the fake sleeps 300s).
    assert!(
        elapsed < Duration::from_secs(30),
        "kill-on-timeout must not wait for the child, elapsed: {elapsed:?}"
    );
}
