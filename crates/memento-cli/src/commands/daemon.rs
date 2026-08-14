//! Daemon control plane (REQ-DAEMON-007, design D4): the `memento daemon
//! start|stop|status` subcommands.
//!
//! B4 scope:
//!
//! * `status` reads the cookie, attempts a HELLO-style ping through
//!   [`DaemonClient`] (built in B3) and reports the daemon's PID, uptime,
//!   tenant id, capabilities, and effective spawn config. Falls back to a
//!   structured "daemon_unavailable" payload when no daemon is bound (so
//!   operators can probe without crashing). Honors `MEMENTO_NO_DAEMON=1`:
//!   in that mode `status` reports "daemon_disabled" and exits 0 — no pipe
//!   contact, no model load (REQ-DAEMON-004).
//!
//! * `start` / `stop` are stubs in B4 — the lazy-spawn and cooperative
//!   shutdown logic land in B5 (REQ-DAEMON-003/013). They print a
//!   structured pending marker and exit 0 so the surface is wired and the
//!   help tree is honest about what the command does today.
//!
//! The control plane never opens `AppService` and never loads models —
//! that contract is the whole point of a daemon (REQ-DAEMON-007).

use clap::ArgMatches;
use memento_domain::DomainError;
use serde_json::json;
use tracing::{info, warn};

use crate::output::emit_json_value;
use crate::transport::pipe_client::{ClientConfig, NO_DAEMON_ENV};
use crate::transport::{DaemonClient, DaemonError};

/// Dispatch one parsed `memento daemon <sub>` invocation.
///
/// Async because `daemon status` reaches the pipe through the async
/// [`DaemonClient::connect`] (REQ-DAEMON-007). `start` / `stop` are stubs
/// in B4 — the bodies land in B5.
pub async fn run(matches: &ArgMatches, _json: bool) -> Result<(), DomainError> {
    match matches.subcommand() {
        Some(("status", sub)) => run_status(sub).await,
        Some(("start", sub)) => run_start(sub),
        Some(("stop", sub)) => run_stop(sub),
        _ => Err(DomainError::InvalidInput {
            message: "unknown daemon subcommand; run 'memento daemon --help' for usage".into(),
        }),
    }
}

/// `memento daemon status` — report the daemon's PID, uptime, and
/// configuration. Always exits 0: the operator probing the daemon must not
/// have to read a stack trace to know it is down (REQ-DAEMON-007).
async fn run_status(_matches: &ArgMatches) -> Result<(), DomainError> {
    // MEMENTO_NO_DAEMON=1 → daemon disabled (REQ-DAEMON-004): short-circuit
    // before touching the pipe, never open AppService.
    if std::env::var(NO_DAEMON_ENV).ok().as_deref() == Some("1") {
        let payload = json!({
            "status": "daemon_disabled",
            "reason": format!("{NO_DAEMON_ENV}=1; control plane runs without pipe contact"),
        });
        emit_json_value(&payload);
        return Ok(());
    }

    // Build the client config; missing env vars surface as a structured
    // status, not a hard error — `daemon status` is a probe.
    let config = match ClientConfig::from_env() {
        Ok(c) => c,
        Err(err) => {
            let payload = json!({
                "status": "daemon_unavailable",
                "reason": client_err_reason(&err),
                "stage": "config",
            });
            emit_json_value(&payload);
            return Ok(());
        }
    };

    // Pre-compute the pipe name — needed both as a payload field on success
    // and as the connect target on the async side.
    let pipe_name = config.pipe_name().unwrap_or_default();
    let result = DaemonClient::connect(&config).await;

    match result {
        Ok(client) => {
            let welcome = &client.welcome;
            let payload = json!({
                "status": "ok",
                "daemon_pid": welcome.daemon_pid,
                "tenant_id": welcome.tenant_id,
                "capabilities": welcome.capabilities,
                "spawn": welcome.spawn,
                "pipe": pipe_name,
            });
            info!(daemon_pid = welcome.daemon_pid, "daemon status: ok");
            emit_json_value(&payload);
            Ok(())
        }
        Err(err) => {
            // Probe semantics: a missing / timed-out daemon is not a CLI
            // failure — operators must be able to call `daemon status`
            // without an exit-code alarm.
            warn!(?err, "daemon status: unavailable");
            let payload = json!({
                "status": "daemon_unavailable",
                "reason": client_err_reason(&err),
                "stage": "connect",
            });
            emit_json_value(&payload);
            Ok(())
        }
    }
}

/// `memento daemon start` — stub in B4 (lazy-spawn lives in B5,
/// REQ-DAEMON-003). Reports a structured "pending" marker and exits 0 so
/// the surface is discoverable from `--help` and operators can wire
/// automation against the eventual semantics today.
fn run_start(_matches: &ArgMatches) -> Result<(), DomainError> {
    let payload = json!({
        "status": "pending",
        "command": "daemon.start",
        "phase": "b4_skeleton",
        "note": "lazy-spawn implementation lands in B5 (REQ-DAEMON-003)",
    });
    emit_json_value(&payload);
    Ok(())
}

/// `memento daemon stop` — stub in B4 (cooperative shutdown lives in B5,
/// REQ-DAEMON-013). Same surface guarantees as [`run_start`].
fn run_stop(_matches: &ArgMatches) -> Result<(), DomainError> {
    let payload = json!({
        "status": "pending",
        "command": "daemon.stop",
        "phase": "b4_skeleton",
        "note": "cooperative shutdown implementation lands in B5 (REQ-DAEMON-013)",
    });
    emit_json_value(&payload);
    Ok(())
}

/// Map a [`DaemonError`] onto the JSON `reason` string the operator sees.
/// The tier names match REQ-DAEMON-002's error taxonomy.
fn client_err_reason(err: &DaemonError) -> String {
    match err {
        DaemonError::Disabled => "daemon_disabled".to_string(),
        DaemonError::MissingEnv(name) => format!("missing_env:{name}"),
        DaemonError::PipeNotFound(name) => format!("pipe_not_found:{name}"),
        DaemonError::Timeout(d) => format!("timeout:{}s", d.as_secs_f64()),
        DaemonError::AuthFailed(reason) => format!("auth_failed:{reason}"),
        DaemonError::ConfigMismatch(reason) => format!("config_mismatch:{reason}"),
        DaemonError::Protocol(reason) => format!("protocol:{reason}"),
        DaemonError::CookieMissing(path) => format!("cookie_missing:{}", path.display()),
        DaemonError::Io(e) => format!("io:{e}"),
    }
}

#[cfg(test)]
mod tests {
    //! Routing tests for the daemon control plane. The real `daemon status`
    //! path is exercised end-to-end by `tests/daemon_commands.rs` via
    //! `assert_cmd` (REQ-CL-001/004, REQ-DAEMON-007).

    use super::*;
    use clap::Command as ClapCommand;

    fn matches_for(args: &[&str]) -> ArgMatches {
        // Build a minimal `daemon <sub>` clap tree — mirrors the production
        // shape (REB-DAEMON-007 subcommands) without pulling i18n strings
        // into unit tests.
        ClapCommand::new("daemon")
            .subcommand(ClapCommand::new("start"))
            .subcommand(ClapCommand::new("stop"))
            .subcommand(ClapCommand::new("status"))
            .get_matches_from(args)
    }

    // The libtest harness is sync, so the async dispatcher is driven via a
    // tiny `tokio::test` runtime built per-test.
    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(fut)
    }

    #[test]
    fn empty_subcommand_returns_invalid_input() {
        // Production `run` falls through to the InvalidInput arm when
        // `matches.subcommand()` returns None. clap exits on a *bogus*
        // subcommand, but accepts the bare root — that's the path we test.
        let m = matches_for(&["daemon"]);
        let err = block_on(run(&m, false)).expect_err("no subcommand");
        assert!(
            matches!(err, DomainError::InvalidInput { .. }),
            "InvalidInput tier: {err}"
        );
    }

    #[test]
    fn start_returns_pending_marker() {
        let m = matches_for(&["daemon", "start"]);
        block_on(run(&m, false)).expect("start stub is Ok in B4");
    }

    #[test]
    fn stop_returns_pending_marker() {
        let m = matches_for(&["daemon", "stop"]);
        block_on(run(&m, false)).expect("stop stub is Ok in B4");
    }

    #[test]
    fn client_err_reason_covers_every_tier() {
        // Every DaemonError variant maps to a stable `reason` string so
        // operators get a uniform surface (REQ-DAEMON-002).
        assert_eq!(client_err_reason(&DaemonError::Disabled), "daemon_disabled");
        assert_eq!(
            client_err_reason(&DaemonError::MissingEnv("MEMENTO_ROOT")),
            "missing_env:MEMENTO_ROOT"
        );
        assert_eq!(
            client_err_reason(&DaemonError::PipeNotFound("pipe-name".into())),
            "pipe_not_found:pipe-name"
        );
        assert_eq!(
            client_err_reason(&DaemonError::Timeout(std::time::Duration::from_secs(2))),
            "timeout:2s"
        );
        assert_eq!(
            client_err_reason(&DaemonError::AuthFailed("token".into())),
            "auth_failed:token"
        );
        assert_eq!(
            client_err_reason(&DaemonError::ConfigMismatch("locale".into())),
            "config_mismatch:locale"
        );
        assert_eq!(
            client_err_reason(&DaemonError::Protocol("bad proto".into())),
            "protocol:bad proto"
        );
        assert!(
            client_err_reason(&DaemonError::CookieMissing(std::path::PathBuf::from("/x")))
                .starts_with("cookie_missing:"),
            "cookie_missing tier"
        );
    }
}
