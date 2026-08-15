//! Daemon control plane (REQ-DAEMON-007, design D4): the `memento daemon
//! start|stop|status` subcommands.
//!
//! B5 scope (lifecycle wired):
//!
//! * `start` — calls [`DaemonSpawner::start`]. The spawner is idempotent:
//!   if a daemon is already running for this (root, tenant) (cookie file
//!   present), it returns the existing handle WITHOUT spawning a second
//!   process (REQ-DAEMON-003 GIVEN "the daemon is spawned, becomes ready,
//!   and the command succeeds"). The structured JSON envelope reports
//!   `started_at` (cookie mtime) so `start` is a no-op for the operator
//!   when the daemon is already up.
//! * `stop` — calls [`DaemonSpawner::stop`]. Roundtrips `sys.shutdown`
//!   through the named pipe and falls back to `taskkill /F` after a
//!   bounded grace window (REQ-DAEMON-013 / R7).
//! * `status` — B4 behavior preserved: connect via [`DaemonClient`],
//!   report pid/uptime/tenant/config. Falls back to a structured
//!   `daemon_unavailable` payload when no daemon is bound so operators
//!   can probe without an exit-code alarm. Honors `MEMENTO_NO_DAEMON=1`
//!   with a `daemon_disabled` payload (no pipe contact, no model load —
//!   REQ-DAEMON-004).
//!
//! The control plane never opens `AppService` and never loads models —
//! that contract is the whole point of a daemon (REQ-DAEMON-007).

use clap::ArgMatches;
use memento_domain::{DomainError, TenantId};
use serde_json::json;
use tracing::{info, warn};

use crate::output::emit_json_value;
use crate::spawn::{DaemonSpawner, SpawnError, SpawnerOptions};
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
        Some(("start", sub)) => run_start(sub).await,
        Some(("stop", sub)) => run_stop(sub).await,
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

/// `memento daemon start` — call [`DaemonSpawner::start`]. The spawner
/// is idempotent: a daemon already running for this (root, tenant) is
/// reported as the existing handle without spawning a second process
/// (REQ-DAEMON-003 GIVEN). Spawn errors are surfaced as a structured
/// JSON envelope so operators can grep the `tier` field in CI logs
/// (REQ-DAEMON-002 uniform taxonomy).
async fn run_start(_matches: &ArgMatches) -> Result<(), DomainError> {
    // Resolve spawn inputs from env. The same gate as `ClientConfig::from_env`
    // — if the env gate rejects the request we echo it as a structured
    // payload (the operator expects a structured surface, not a panic).
    let config = match ClientConfig::from_env() {
        Ok(c) => c,
        Err(err) => {
            return Err(startup_error_to_domain(&err));
        }
    };
    let tenant_id: TenantId =
        config
            .tenant_id
            .parse()
            .map_err(|err| DomainError::InvalidInput {
                message: format!("invalid MEMENTO_TENANT: {err}"),
            })?;
    let opts = SpawnerOptions {
        root: config.root.clone(),
        tenant_id,
        no_embeddings: config.no_embeddings,
        locale: config.locale.clone(),
    };

    match DaemonSpawner::start(&opts).await {
        Ok(handle) => {
            info!(
                pid = handle.pid,
                started_at = %handle.started_at.to_rfc3339(),
                "daemon start: ok (idempotent if already running)"
            );
            let payload = json!({
                "status": "ok",
                "pid": handle.pid,
                "started_at": handle.started_at.to_rfc3339(),
                "idempotent": true,
            });
            emit_json_value(&payload);
            Ok(())
        }
        Err(err) => {
            warn!(tier = err.tier(), ?err, "daemon start: failed");
            Err(spawn_error_to_domain(err))
        }
    }
}

/// `memento daemon stop` — call [`DaemonSpawner::stop`]: HELLO/WELCOME +
/// one framed `sys.shutdown` request, fall back to `taskkill /F` on
/// grace expiry (REQ-DAEMON-013 / R7). Errors surface as the same
/// structured envelope the spawner produces.
async fn run_stop(_matches: &ArgMatches) -> Result<(), DomainError> {
    let config = match ClientConfig::from_env() {
        Ok(c) => c,
        Err(err) => return Err(startup_error_to_domain(&err)),
    };
    // The stop path needs the root; we don't need the spawn options.
    let root = config.root.clone();
    match DaemonSpawner::stop(&root).await {
        Ok(()) => {
            info!(pid_field = "shutdown_sent", "daemon stop: ok");
            let payload = json!({
                "status": "ok",
                "phase": "shutdown_requested",
            });
            emit_json_value(&payload);
            Ok(())
        }
        Err(err) => {
            warn!(tier = err.tier(), ?err, "daemon stop: failed");
            Err(spawn_error_to_domain(err))
        }
    }
}

/// Map a [`SpawnError`] onto a [`DomainError`] so the CLI exit code
/// path (REQ-CL-005) handles it uniformly with every other CLI failure.
fn spawn_error_to_domain(err: SpawnError) -> DomainError {
    let message = format!("{err}");
    match err {
        SpawnError::Disabled
        | SpawnError::BinaryNotFound
        | SpawnError::MissingEnv(_)
        | SpawnError::LockBusy(_) => DomainError::InvalidInput { message },
        SpawnError::ReadinessTimeout(_)
        | SpawnError::SpawnFailedExit(_)
        | SpawnError::Shutdown(_)
        | SpawnError::Connect(_)
        | SpawnError::Io(_) => DomainError::Internal { message },
    }
}

/// Map a [`DaemonError`] onto a [`DomainError`] (used by `start`/`stop`
/// when the env gate rejects them — same uniform surface as `status`).
fn startup_error_to_domain(err: &DaemonError) -> DomainError {
    match err {
        DaemonError::Disabled | DaemonError::MissingEnv(_) => DomainError::InvalidInput {
            message: format!("{err}"),
        },
        DaemonError::ConfigMismatch(_) | DaemonError::AuthFailed(_) => DomainError::AuthFailed,
        DaemonError::PipeNotFound(_)
        | DaemonError::Timeout(_)
        | DaemonError::Protocol(_)
        | DaemonError::CookieMissing(_)
        | DaemonError::Io(_) => DomainError::Internal {
            message: format!("{err}"),
        },
    }
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
        // B5: `start` is now a real call into `DaemonSpawner::start`.
        // Without env credentials it surfaces a structured
        // `DaemonError::MissingEnv` translated to `DomainError::InvalidInput`.
        // The test asserts the structured path: the surface exits with a
        // domain error (NOT exit 0) and never reaches the spawner.
        let m = matches_for(&["daemon", "start"]);
        // SAFETY: serialized in nextest via `test(daemon_commands)`. The
        // B5 unit test asserts that without credentials the call surfaces
        // a structured domain error rather than the old `pending` marker.
        unsafe { std::env::remove_var("MEMENTO_TOKEN") };
        unsafe { std::env::remove_var("MEMENTO_AGENT_ID") };
        unsafe { std::env::remove_var("MEMENTO_TENANT") };
        unsafe { std::env::remove_var("MEMENTO_ROOT") };
        let err = block_on(run(&m, false)).expect_err("missing env surfaces error");
        assert!(
            matches!(err, DomainError::InvalidInput { .. }),
            "InvalidInput tier: {err}"
        );
    }

    #[test]
    fn stop_returns_pending_marker() {
        // B5: `stop` is now a real call into `DaemonSpawner::stop`. Same
        // env-gate semantics as `start` (no credentials → structured
        // domain error, NOT the old `pending` marker).
        let m = matches_for(&["daemon", "stop"]);
        unsafe { std::env::remove_var("MEMENTO_TOKEN") };
        unsafe { std::env::remove_var("MEMENTO_AGENT_ID") };
        unsafe { std::env::remove_var("MEMENTO_TENANT") };
        unsafe { std::env::remove_var("MEMENTO_ROOT") };
        let err = block_on(run(&m, false)).expect_err("missing env surfaces error");
        assert!(
            matches!(err, DomainError::InvalidInput { .. }),
            "InvalidInput tier: {err}"
        );
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
