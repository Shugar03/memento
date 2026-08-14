//! `memento daemon <sub>` control plane tests (REQ-DAEMON-007, design D4).
//!
//! B4 only wires the surface — the lazy-spawn (`start`) and cooperative
//! shutdown (`stop`) bodies land in B5. These tests prove the surface is
//! honest about that contract:
//!
//! * `daemon status` honors `MEMENTO_NO_DAEMON=1` and exits 0 with a
//!   structured `daemon_disabled` payload (no pipe contact, no model
//!   load — REQ-DAEMON-004/007).
//! * `daemon status` without a live daemon exits 0 with a structured
//!   `daemon_unavailable` payload — operators can probe the daemon
//!   without an exit-code alarm.
//! * `daemon start` / `daemon stop` are explicit B4 stubs: they print a
//!   `pending` marker with the `b4_skeleton` phase so automation wiring
//!   fails loudly if it depends on the not-yet-wired behavior.
//!
//! The test fixture never provisions a real tenant — the control plane
//! never opens `AppService` (REQ-DAEMON-007).

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
fn start_returns_b4_skeleton_marker_and_exits_zero() {
    // B4 stub: lazy-spawn lives in B5 (REQ-DAEMON-003). The surface
    // exposes the future contract so operators can wire automation today.
    let out = no_daemon_cmd()
        .args(["daemon", "start"])
        .output()
        .expect("run daemon start");
    assert_eq!(out.status.code(), Some(0), "stub exits 0");
    let payload = json_of(&out);
    assert_eq!(payload["status"], "pending", "start pending: {payload}");
    assert_eq!(payload["command"], "daemon.start");
    assert_eq!(payload["phase"], "b4_skeleton");
    assert!(
        payload["note"].as_str().unwrap_or_default().contains("B5"),
        "note mentions B5: {payload}"
    );
}

#[test]
fn stop_returns_b4_skeleton_marker_and_exits_zero() {
    // B4 stub: cooperative shutdown lives in B5 (REQ-DAEMON-013).
    let out = no_daemon_cmd()
        .args(["daemon", "stop"])
        .output()
        .expect("run daemon stop");
    assert_eq!(out.status.code(), Some(0), "stub exits 0");
    let payload = json_of(&out);
    assert_eq!(payload["status"], "pending", "stop pending: {payload}");
    assert_eq!(payload["command"], "daemon.stop");
    assert_eq!(payload["phase"], "b4_skeleton");
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
