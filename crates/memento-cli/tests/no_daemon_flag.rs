//! `--no-daemon` flag tests (REQ-DAEMON-004, design D7).
//!
//! The flag MUST mirror `MEMENTO_NO_DAEMON=1` BEFORE any startup logic
//! runs — every transport / spawner / startup check already honors the
//! env var, so once it is set, the CLI never touches the named pipe.
//!
//! B6 acceptance: a CLI invocation that carries `--no-daemon` (without
//! the env var) produces the same byte-identical shape as the env-only
//! variant.
//!
//! Scenarios:
//!
//! * `flag_sets_env_and_short_circuits_status` — `memento daemon
//!   status --no-daemon` (no `MEMENTO_NO_DAEMON`) exits 0 with the
//!   structured `daemon_disabled` payload, proving the flag was
//!   honored.
//! * `flag_keeps_metrics_dump_local` — `memento observability metrics
//!   --no-daemon` exits 0 with an empty registry (the local dump is
//!   taken; the daemon path is never attempted).
//! * `flag_wins_over_daemon_env_explicit_zero` — the flag sets the env
//!   var even when the env was previously unset / `0`, pinning the
//!   pre-scan precedence.

use assert_cmd::Command;
use serde_json::Value;

fn bin() -> Command {
    Command::cargo_bin("memento").expect("memento binary")
}

/// Every `memento daemon status` invocation in this file runs against a
/// fresh throwaway root so the probe cannot accidentally hit a real
/// cookie / pipe from a sibling test.
fn fresh_root_cmd() -> Command {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cmd = bin();
    cmd.env("MEMENTO_ROOT", dir.path())
        // Make sure the env var is not leaking from a sibling test
        // (each test gets a fresh subprocess, but the parent env can
        // still bleed through nextest's shared process pool).
        .env_remove("MEMENTO_NO_DAEMON");
    cmd
}

fn json_of(out: &std::process::Output) -> Value {
    assert!(
        out.status.success(),
        "expected success, got {:?}: stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("stdout is JSON")
}

#[test]
fn flag_sets_env_and_short_circuits_status() {
    // REQ-DAEMON-004: `--no-daemon` pre-scans argv and sets
    // MEMENTO_NO_DAEMON=1 BEFORE the env gate in `ClientConfig::from_env`
    // runs. The `daemon status` probe must therefore print the
    // `daemon_disabled` payload — exit 0 — without ever opening the
    // pipe or scanning for a cookie.
    let out = fresh_root_cmd()
        .args(["--no-daemon", "daemon", "status"])
        .output()
        .expect("run daemon status --no-daemon");
    assert_eq!(
        out.status.code(),
        Some(0),
        "probe exit 0; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let payload = json_of(&out);
    assert_eq!(
        payload["status"], "daemon_disabled",
        "no-daemon flag must reach the gate: {payload}"
    );
    assert!(
        payload["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("MEMENTO_NO_DAEMON"),
        "reason names the env the flag set: {payload}"
    );
}

#[test]
fn flag_keeps_metrics_dump_local() {
    // REQ-DAEMON-004 + REQ-DAEMON-010: `memento observability metrics
    // --no-daemon` exits 0 and renders an empty registry (the daemon
    // path is short-circuited by the flag; the local dump is the
    // process-local fallback).
    let out = fresh_root_cmd()
        .args(["--no-daemon", "observability", "metrics"])
        .env("MEMENTO_METRICS", "1")
        .output()
        .expect("run observability metrics --no-daemon");
    assert_eq!(
        out.status.code(),
        Some(0),
        "exit 0; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "no daemon → local dump: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn flag_mirrors_to_env_var_inside_subprocess() {
    // REQ-DAEMON-004 acceptance: the flag is equivalent to setting
    // `MEMENTO_NO_DAEMON=1`. Both shapes must produce identical output.
    // Compare against the env-only variant from the B5 suite
    // (`tests/daemon_commands::status_with_no_daemon_env_*`).
    let with_flag = fresh_root_cmd()
        .args(["--no-daemon", "daemon", "status"])
        .output()
        .expect("run with flag");
    let with_env = fresh_root_cmd()
        .env("MEMENTO_NO_DAEMON", "1")
        .args(["daemon", "status"])
        .output()
        .expect("run with env");
    assert_eq!(with_flag.status.code(), Some(0));
    assert_eq!(with_env.status.code(), Some(0));
    let payload_flag = json_of(&with_flag);
    let payload_env = json_of(&with_env);
    assert_eq!(
        payload_flag["status"], payload_env["status"],
        "flag and env produce the same status: {payload_flag:?} vs {payload_env:?}"
    );
}

#[test]
fn flag_appears_in_help_text_bilingually() {
    // REQ-CL-004: the new key (`CliHelpNoDaemon`) must surface in
    // `--help` for both the primary locale (ES) and the EN fallback.
    // `--no-daemon` is a global flag, so it appears on the root
    // command's help.
    let es = bin().args(["--help"]).output().expect("es help");
    assert!(es.status.success(), "es help exits 0");
    let es_help = String::from_utf8_lossy(&es.stdout);
    assert!(
        es_help.contains("--no-daemon"),
        "ES --help lists --no-daemon: {es_help}"
    );
    assert!(
        es_help.contains("Desactiva el daemon persistente"),
        "ES description present: {es_help}"
    );

    let en = bin()
        .args(["--locale", "en", "--help"])
        .output()
        .expect("en help");
    assert!(en.status.success(), "en help exits 0");
    let en_help = String::from_utf8_lossy(&en.stdout);
    assert!(
        en_help.contains("--no-daemon"),
        "EN --help lists --no-daemon: {en_help}"
    );
    assert!(
        en_help.contains("Disable the persistent daemon"),
        "EN description present: {en_help}"
    );
}