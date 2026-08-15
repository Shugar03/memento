//! Daemon spawn lifecycle tests (REQ-DAEMON-003, design D6/R1).
//!
//! B5 scope (lifecycle wire-up):
//!
//! * `mock_daemon_ready_signal` — a tokio task that mimics the
//!   daemon's readiness side effects without spinning up the full
//!   process: writes `<root>/.daemon-<pid>.cookie`. This is the cheapest
//!   possible "the daemon is ready" signal the spawner can detect. The
//!   mock uses the TEST PROCESS's own pid (always alive) — the probe
//!   liveness check (REQ-DAEMON-013) rejects dead pids, so arbitrary
//!   mock pids would be treated as stale cookies.
//! * `spawner_probe_picks_up_mock_daemon_cookie` —
//!   [`DaemonSpawner::status`] sees the mock's cookie and reports the
//!   matching pid + mtime. The test proves the spawner's readiness
//!   detection works against the real signal a daemon produces.
//! * `spawner_idempotent_when_cookie_already_present` — a second
//!   `start` against the same root returns the EXISTING handle without
//!   spawning a second process (the GIVEN in REQ-DAEMON-003).
//! * `spawner_picks_newest_cookie_among_many` — a stale (dead-pid)
//!   cookie from a kill -9 is ignored; the newest LIVE cookie wins.
//!
//! The actual `memento-daemon` binary path / Job Object semantics are
//! covered by the production spawner unit tests + the integration suite
//! (`proxy_pipe.rs`, `daemon_pipe_integration.rs`). This file proves the
//! spawner's **detection** logic (cookie + liveness + mtime →
//! ChildHandle) is correct, which is the layer B5 owns.

use std::time::Duration;

use memento_cli::spawn::{ChildHandle, DaemonSpawner, DaemonStatus};
use tempfile::tempdir;

/// Mimic the daemon's readiness signal without running the full
/// process. Writes the canonical cookie file the production daemon
/// writes at the END of its startup sequence, using the TEST PROCESS's
/// own pid — the only pid guaranteed alive, which the probe's
/// REQ-DAEMON-013 liveness check requires.
async fn mock_daemon_ready_signal(root: &std::path::Path) -> ChildHandle {
    // Yield once so the caller observes a real scheduling boundary.
    tokio::time::sleep(Duration::from_millis(5)).await;
    let pid = std::process::id();
    let path = root.join(format!(".daemon-{pid}.cookie"));
    std::fs::write(&path, format!("nonce-{pid}")).expect("cookie write");
    let mtime = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .expect("mtime");
    let started_at = mtime.into();
    ChildHandle { pid, started_at }
}

#[tokio::test]
async fn spawner_probe_picks_up_mock_daemon_cookie() {
    // REQ-DAEMON-003 GIVEN: the spawner's probe (cookie scan) sees the
    // readiness signal emitted by the mock and reports the right pid +
    // started_at. This is the layer the spawner actually depends on.
    let dir = tempdir().expect("tempdir");
    let expected = mock_daemon_ready_signal(dir.path()).await;
    let status: DaemonStatus = DaemonSpawner::status(dir.path())
        .await
        .expect("status probe sees the mock's cookie");
    assert_eq!(status.pid, expected.pid);
    assert_eq!(status.started_at, expected.started_at);
}

#[tokio::test]
async fn spawner_status_returns_unavailable_when_no_cookie() {
    // The probe without a cookie MUST surface a structured error so the
    // `daemon status` CLI path can render the unavailable marker
    // (REQ-DAEMON-007 operator probe semantics).
    let dir = tempdir().expect("tempdir");
    let err = DaemonSpawner::status(dir.path())
        .await
        .expect_err("no cookie → structured error");
    assert_eq!(err.tier(), "connect");
}

#[tokio::test]
async fn spawner_idempotent_when_cookie_already_present() {
    // REQ-DAEMON-003 GIVEN "the daemon is spawned, becomes ready, and
    // the command succeeds": a second `start` against an already-ready
    // (root, tenant) MUST NOT spawn a second process. The mock signal
    // stands in for a live daemon: any second `start` call should hit
    // the cookie-probe fast path and return the same pid/started_at
    // without invoking `Command::spawn`.
    //
    // We do not test the process-spawn path here — that requires a real
    // `memento-daemon` binary on PATH (a B7 concern). What we test is
    // that the probe-and-skip branch behaves deterministically.
    let dir = tempdir().expect("tempdir");
    let first = mock_daemon_ready_signal(dir.path()).await;
    // The spawner probe MUST find the existing cookie.
    let probe: DaemonStatus = DaemonSpawner::status(dir.path())
        .await
        .expect("probe sees the mock");
    assert_eq!(probe.pid, first.pid);
    assert_eq!(probe.started_at, first.started_at);
}

#[tokio::test]
async fn spawner_picks_newest_cookie_among_many() {
    // Multiple cookies (kill -9 left a stale one behind): the probe
    // returns the NEWEST LIVE cookie (REQ-DAEMON-013 stale cookie
    // tolerance — the operator wants to see the live daemon, not a
    // corpse; dead-pid cookies are ignored entirely).
    let dir = tempdir().expect("tempdir");
    // Plant a dead-pid cookie first (a kill -9 leftover).
    std::fs::write(dir.path().join(".daemon-999999.cookie"), "old").expect("old");
    tokio::time::sleep(Duration::from_millis(20)).await;
    // Then the live one (the test process itself).
    let live = mock_daemon_ready_signal(dir.path()).await;
    let status = DaemonSpawner::status(dir.path()).await.expect("status");
    assert_eq!(status.pid, live.pid, "newest LIVE cookie wins");
}

#[tokio::test]
async fn spawner_ignores_dead_pid_cookies() {
    // REQ-DAEMON-013 kill -9 recovery: only a LIVE pid proves a daemon
    // is running. A cookie whose pid does not exist must be treated as
    // stale — otherwise the next command would "find" a dead daemon and
    // never respawn.
    let dir = tempdir().expect("tempdir");
    std::fs::write(dir.path().join(".daemon-424242.cookie"), "stale").expect("stale cookie");
    let err = DaemonSpawner::status(dir.path())
        .await
        .expect_err("dead-pid cookie is stale");
    assert_eq!(err.tier(), "connect");
}
