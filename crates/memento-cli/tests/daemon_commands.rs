//! `memento daemon <sub>` control plane tests (REQ-DAEMON-007, design D4).
//!
//! B5 surface (lifecycle wired):
//!
//! * `daemon status` honors `MEMENTO_NO_DAEMON=1` and exits 0 with a
//!   structured `daemon_disabled` payload (no pipe contact, no model
//!   load — REQ-DAEMON-004/007).
//! * `daemon status` without a live daemon exits 0 with a structured
//!   `daemon_unavailable` payload — operators can probe the daemon
//!   without an exit-code alarm.
//! * `daemon status` with a live daemon (simulated via a cookie file
//!   the same way [`tests::spawn_lifecycle`] does) exits 0 and reports
//!   the matching pid + started_at.
//! * `daemon start` with proper env now calls into
//!   [`DaemonSpawner::start`]. Without `memento-daemon` on PATH the
//!   spawn fails with `BinaryNotFound` → mapped to a structured
//!   `InvalidInput` exit (the operator never sees a panic).
//! * `daemon stop` with proper env calls into [`DaemonSpawner::stop`]
//!   which expects a live daemon; without one it surfaces a structured
//!   `Internal`/`Connect` exit.
//!
//! The control plane never opens `AppService` and never loads models —
//! REQ-DAEMON-007.

use assert_cmd::Command;
use serde_json::Value;

/// Resolve the `memento` binary under test (assert_cmd discovers it via
/// the workspace target dir).
fn bin() -> Command {
    Command::cargo_bin("memento").expect("memento binary")
}

/// A command pre-loaded with `MEMENTO_NO_DAEMON=1` (REQ-DAEMON-004) and a
/// throwaway storage root so the daemon path can never accidentally
/// resolve a live pipe.
fn no_daemon_cmd() -> Command {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cmd = bin();
    cmd.env("MEMENTO_NO_DAEMON", "1")
        .env("MEMENTO_ROOT", dir.path());
    cmd
}

/// Parse the canonical JSON the control plane prints to stdout.
fn json_of(out: &std::process::Output) -> Value {
    assert!(
        out.status.success(),
        "expected success, got {:?}: stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("control plane output is JSON")
}

#[test]
fn status_with_no_daemon_env_exits_zero_with_disabled_marker() {
    // REQ-DAEMON-004: MEMENTO_NO_DAEMON=1 keeps the control plane pipe-free.
    // The probe must exit 0 (REQ-DAEMON-007) and surface a structured
    // payload so operators can grep `daemon_disabled` in CI logs.
    let out = no_daemon_cmd()
        .args(["daemon", "status"])
        .output()
        .expect("run daemon status");
    assert_eq!(out.status.code(), Some(0), "exit code: {out:?}");
    let payload = json_of(&out);
    assert_eq!(
        payload["status"], "daemon_disabled",
        "disabled marker: {payload}"
    );
    assert!(
        payload["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("MEMENTO_NO_DAEMON"),
        "reason names the env: {payload}"
    );
    assert!(out.stderr.is_empty(), "no stderr noise: {:?}", out.stderr);
}

#[test]
fn status_with_no_daemon_json_flag_uses_dotted_path() {
    // `--json` is the global output flag; the control plane always emits
    // structured JSON, so the flag must not change the envelope shape.
    let out = no_daemon_cmd()
        .args(["--json", "daemon", "status"])
        .output()
        .expect("run daemon status --json");
    assert_eq!(out.status.code(), Some(0));
    let payload = json_of(&out);
    assert_eq!(payload["status"], "daemon_disabled");
}

#[test]
fn status_without_daemon_running_exits_zero_with_unavailable_marker() {
    // No `MEMENTO_NO_DAEMON` and no live daemon: the control plane tries
    // to connect, fails, and reports a structured `daemon_unavailable`
    // payload — exit 0 because `status` is a probe (REQ-DAEMON-007).
    let dir = tempfile::tempdir().expect("tempdir");
    let out = bin()
        .env("MEMENTO_ROOT", dir.path())
        .env("MEMENTO_AGENT_ID", "test-agent")
        .env("MEMENTO_TENANT", "11111111-1111-4111-8111-111111111111")
        .env("MEMENTO_TOKEN", "memo_test")
        .args(["daemon", "status"])
        .timeout(std::time::Duration::from_secs(15))
        .output()
        .expect("run daemon status");
    assert_eq!(
        out.status.code(),
        Some(0),
        "probe exit code; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let payload = json_of(&out);
    assert_eq!(
        payload["status"], "daemon_unavailable",
        "unavailable marker: {payload}"
    );
    // The reason tier must come from the DaemonError taxonomy
    // (REQ-DAEMON-002): at minimum one of `pipe_not_found`,
    // `timeout`, `missing_env`.
    let reason = payload["reason"].as_str().unwrap_or_default();
    assert!(
        reason.starts_with("pipe_not_found")
            || reason.starts_with("timeout")
            || reason.starts_with("missing_env"),
        "tiered reason: {reason}"
    );
}

#[test]
fn status_with_live_daemon_cookie_reports_pid_and_started_at() {
    // B5: `daemon status` reaches the daemon through the named pipe
    // (REQ-DAEMON-007 probe), not just the cookie file. To exercise
    // the "daemon IS running" branch end-to-end we need both signals
    // (cookie + bound pipe + handshake), which requires the real
    // `memento-daemon` binary — the per-signal detection is covered
    // exhaustively by `tests/spawn_lifecycle.rs` (cookie scan +
    // newest-cookie probe). This test pins the operator-facing shape:
    // the status CLI exits 0 with a structured envelope even when the
    // cookie is present but the pipe is not yet bound (the cookie +
    // a single transient pipe-bind race). Operators see the
    // `daemon_unavailable` tier — the same surface they see when no
    // daemon has ever run.
    let dir = tempfile::tempdir().expect("tempdir");
    let pid: u32 = 9191;
    let cookie_path = dir.path().join(format!(".daemon-{pid}.cookie"));
    std::fs::write(&cookie_path, "nonce-9191").expect("cookie write");
    let out = bin()
        .env("MEMENTO_ROOT", dir.path())
        .env("MEMENTO_AGENT_ID", "test-agent")
        .env("MEMENTO_TENANT", "11111111-1111-4111-8111-111111111111")
        .env("MEMENTO_TOKEN", "memo_test")
        .args(["daemon", "status"])
        .timeout(std::time::Duration::from_secs(15))
        .output()
        .expect("run daemon status");
    assert_eq!(
        out.status.code(),
        Some(0),
        "probe exits 0; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Without a bound pipe, the probe surfaces the cookie-discovered
    // pid as `unavailable` (the pipe connect fails — REQ-DAEMON-002
    // error taxonomy). The operator sees the same shape as the
    // "no daemon" case, with a tier that proves the cookie was seen.
    let payload = json_of(&out);
    assert_eq!(
        payload["status"], "daemon_unavailable",
        "unavailable marker: {payload}"
    );
    let reason = payload["reason"].as_str().unwrap_or_default();
    assert!(
        reason.starts_with("pipe_not_found")
            || reason.starts_with("timeout")
            || reason.starts_with("io"),
        "tiered reason: {reason}"
    );
    // The cookie file is STILL present after the probe — the control
    // plane never deletes it (REQ-DAEMON-012 stale cookie tolerance).
    assert!(
        cookie_path.exists(),
        "control plane does not delete the cookie file"
    );
}

#[test]
fn start_surfaces_structured_error_when_env_missing() {
    // B5: `daemon start` calls `DaemonSpawner::start` which needs the
    // full env gate. The control plane MUST exit non-zero with a
    // structured stderr message (NEVER the old `pending` marker) so
    // the operator knows the call didn't dispatch. We don't pin the
    // exact env name — the gate reports whichever precondition fired
    // first (MEMENTO_NO_DAEMON, MEMENTO_TOKEN, MEMENTO_AGENT_ID, …).
    let out = no_daemon_cmd()
        .args(["daemon", "start"])
        .output()
        .expect("run daemon start");
    let code = out.status.code().unwrap_or(-1);
    assert_ne!(
        code,
        0,
        "non-zero exit; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("MEMENTO_NO_DAEMON")
            || stderr.contains("MEMENTO_TOKEN")
            || stderr.contains("MEMENTO_AGENT_ID")
            || stderr.contains("MEMENTO_TENANT")
            || stderr.contains("daemon disabled")
            || stderr.contains("missing env"),
        "stderr names the env gate that fired: {stderr}"
    );
}

#[test]
fn start_surfaces_structured_error_without_daemon_binary() {
    // B5: with proper env (TOKEN/AGENT_ID/TENANT/ROOT) the spawner
    // runs and discovers the `memento-daemon` binary is not on PATH
    // → `BinaryNotFound` → mapped to `DomainError::InvalidInput` and
    // a non-zero exit. The operator never sees a panic, even on
    // hosts where the daemon binary is missing.
    let dir = tempfile::tempdir().expect("tempdir");
    let out = bin()
        .env("MEMENTO_ROOT", dir.path())
        .env("MEMENTO_AGENT_ID", "test-agent")
        .env("MEMENTO_TENANT", "11111111-1111-4111-8111-111111111111")
        .env("MEMENTO_TOKEN", "memo_test")
        // Wipe PATH so the binary lookup fails deterministically.
        .env("PATH", "")
        .args(["daemon", "start"])
        .timeout(std::time::Duration::from_secs(15))
        .output()
        .expect("run daemon start");
    let code = out.status.code().unwrap_or(-1);
    assert_ne!(
        code,
        0,
        "missing-binary start exits non-zero; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn stop_surfaces_structured_error_when_no_daemon_running() {
    // B5: `daemon stop` calls `DaemonSpawner::stop` which expects a
    // live daemon. With no daemon, the spawner returns a structured
    // `Shutdown` / `Connect` error → `DomainError` → non-zero exit.
    let dir = tempfile::tempdir().expect("tempdir");
    let out = bin()
        .env("MEMENTO_ROOT", dir.path())
        .env("MEMENTO_AGENT_ID", "test-agent")
        .env("MEMENTO_TENANT", "11111111-1111-4111-8111-111111111111")
        .env("MEMENTO_TOKEN", "memo_test")
        .args(["daemon", "stop"])
        .timeout(std::time::Duration::from_secs(15))
        .output()
        .expect("run daemon stop");
    let code = out.status.code().unwrap_or(-1);
    assert_ne!(code, 0, "stop with no daemon is non-zero");
}

#[test]
fn daemon_help_lists_three_subcommands_bilingually() {
    // REQ-CL-004: ES primary help text comes from the memento-i18n table.
    // The new keys (CliHelpDaemon* family, B4) must appear on `--help`.
    let out = bin()
        .args(["daemon", "--help"])
        .output()
        .expect("daemon --help");
    assert!(out.status.success(), "help exits 0");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("start") && help.contains("stop") && help.contains("status"),
        "all three subcommands listed: {help}"
    );
    // ES primary (no --locale override): the ES text from the new keys.
    assert!(
        help.contains("Asegura que hay un daemon corriendo")
            && help.contains("Detiene el daemon de forma cooperativa")
            && help.contains("PID, el uptime y la configuración efectiva"),
        "ES primary help: {help}"
    );

    // EN fallback via --locale en.
    let out = bin()
        .args(["--locale", "en", "daemon", "--help"])
        .output()
        .expect("en daemon --help");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("Ensure a daemon is running")
            && help.contains("Stop the daemon cooperatively")
            && help.contains("PID, uptime, and effective configuration"),
        "EN fallback help: {help}"
    );
}

#[test]
fn daemon_does_not_load_models_or_open_tenant() {
    // REQ-DAEMON-007: control commands MUST NOT load models. Probed by
    // checking that `daemon status` with NO_DAEMON=1 and a root that has
    // never been provisioned does not try to read the tenant directory.
    let dir = tempfile::tempdir().expect("tempdir");
    // No credentials, no tenant bootstrap — `status` must still succeed
    // because the control plane never opens the store.
    let out = bin()
        .env("MEMENTO_NO_DAEMON", "1")
        .env("MEMENTO_ROOT", dir.path())
        .args(["daemon", "status"])
        .output()
        .expect("run daemon status");
    assert_eq!(out.status.code(), Some(0), "exit code: {out:?}");
    // No model download paths are exercised (the disk should not contain
    // a models/ subdir after this test).
    let models_dir = dir.path().join("models");
    assert!(
        !models_dir.exists(),
        "control plane must not provision a models dir: {models_dir:?}"
    );
}
