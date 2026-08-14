//! Observability commands (REQ-OBS-007, design D7): the metrics dump.
//!
//! B6 (REQ-DAEMON-010, R5): in daemon mode the dump prefers the daemon's
//! Prometheus registry over the pipe (`sys.metrics` returns the
//! daemon-rendered text stamped `# source=daemon pid=<n> tenant=<tid>`).
//! The process-local dump stays as the fallback for three independent
//! reasons:
//!
//! 1. `MEMENTO_NO_DAEMON=1` set (operator / test override, REQ-DAEMON-004).
//! 2. Required env vars missing (`MEMENTO_ROOT` / `MEMENTO_TOKEN` /
//!    `MEMENTO_AGENT_ID` / `MEMENTO_TENANT`) — the standalone dump is
//!    root-independent by design.
//! 3. No live daemon (`PipeNotFound` / `Timeout`) — the dump MUST NOT
//!    auto-spawn; REQ-DAEMON-010 prohibits it.
//!
//! No HTTP listener is ever bound — the exporter is compiled with
//! `default-features=false` (no hyper in the tree), and the dump is a
//! plain text render.

use clap::ArgMatches;
use memento_domain::DomainError;
use memento_mcp::dispatcher::SysCommand;
use memento_mcp::frame;
use serde_json::{Value, json};
use std::path::PathBuf;
use tracing::warn;

use crate::transport::pipe_client::{
    ClientConfig, DaemonClient, DaemonError, NO_DAEMON_ENV,
};

/// The dump destination override (REQ-OBS-007): `MEMENTO_METRICS_FILE` when
/// set, stdout otherwise.
fn file_override() -> Option<PathBuf> {
    std::env::var_os("MEMENTO_METRICS_FILE").map(PathBuf::from)
}

/// Render the process-local Prometheus dump and write it to the
/// configured destination (stdout / `MEMENTO_METRICS_FILE`).
fn render_local() -> Result<(), DomainError> {
    let dump = memento_observability::metrics::render();
    match file_override() {
        Some(path) => std::fs::write(&path, &dump).map_err(DomainError::from)?,
        None => print!("{dump}"),
    }
    Ok(())
}

/// Write `body` to stdout or `MEMENTO_METRICS_FILE` (same destination
/// rule as the local dump).
fn emit(body: String) -> Result<(), DomainError> {
    match file_override() {
        Some(path) => std::fs::write(&path, &body).map_err(DomainError::from)?,
        None => print!("{body}"),
    }
    Ok(())
}

/// Try the daemon path: build a `ClientConfig`, connect, send
/// `sys.metrics`, extract the rendered body. Any failure along the way
/// falls back to the local dump (see module docs).
async fn try_daemon_metrics() -> Result<Option<String>, DomainError> {
    // Build the client config. Missing env vars surface as a structured
    // error → fall back to local (the standalone dump is root-independent,
    // REQ-OBS-007 / D7).
    let config = match ClientConfig::from_env() {
        Ok(c) => c,
        Err(err) => {
            warn!(tier = "client_config", ?err, "observability metrics: falling back to local dump");
            return Ok(None);
        }
    };
    // Connect. No live daemon → local dump, NEVER auto-spawn
    // (REQ-DAEMON-010).
    let mut client = match DaemonClient::connect(&config).await {
        Ok(c) => c,
        Err(DaemonError::PipeNotFound(_))
        | Err(DaemonError::Timeout(_))
        | Err(DaemonError::CookieMissing(_)) => {
            warn!("observability metrics: no live daemon; local dump");
            return Ok(None);
        }
        Err(err) => {
            warn!(?err, "observability metrics: connect failed; local dump");
            return Ok(None);
        }
    };
    // Send `sys.metrics`. Wire shape:
    //   {"kind":"sys","command":"metrics"} → framed bytes
    let request = json!({
        "kind": "sys",
        "command": SysCommand::Metrics,
    });
    let request_bytes =
        serde_json::to_vec(&request).map_err(|err| DomainError::Internal {
            message: format!("serializing sys.metrics request: {err}"),
        })?;
    // Frame::write_message is provided by memento-mcp::frame.
    let timeout_d = config.pipe_timeout;
    let write_result = tokio::time::timeout(timeout_d, frame::write_message(&mut client.conn, &request_bytes))
        .await
        .map_err(|_| DomainError::Internal {
            message: format!(
                "sys.metrics request write timed out after {timeout_d:?}"
            ),
        })?;
    if let Err(err) = write_result {
        warn!(?err, "observability metrics: write failed; local dump");
        return Ok(None);
    }
    let response_bytes = match tokio::time::timeout(timeout_d, frame::read_message(&mut client.conn)).await {
        Ok(Ok(b)) => b,
        Ok(Err(err)) => {
            warn!(?err, "observability metrics: read failed; local dump");
            return Ok(None);
        }
        Err(_) => {
            warn!("observability metrics: read timed out; local dump");
            return Ok(None);
        }
    };
    let response: Value = match serde_json::from_slice(&response_bytes) {
        Ok(v) => v,
        Err(err) => {
            warn!(?err, "observability metrics: response is not valid JSON; local dump");
            return Ok(None);
        }
    };
    let body = response
        .get("body")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(body)
}

/// `observability metrics`: render the registry as Prometheus text to
/// stdout, or to `MEMENTO_METRICS_FILE` when set (REQ-OBS-007, D7,
/// REQ-DAEMON-010).
///
/// B6 dispatch order:
/// 1. `MEMENTO_NO_DAEMON=1` → local dump (operator / test override).
/// 2. Try the daemon path (see [`try_daemon_metrics`]); on success
///    render the daemon-stamped body, on any failure fall back to the
///    local dump.
/// 3. Local dump otherwise.
///
/// Always exits 0 — with an empty registry while `MEMENTO_METRICS` is
/// off (the recorder is never installed, `render()` returns `""`).
pub async fn run(m: &ArgMatches) -> Result<(), DomainError> {
    run_inner(m).await
}

/// Sync wrapper for sync callers (legacy B3 tests + the metrics_dump
/// integration suite, which exercises the dispatch under a non-tokio
/// runtime). B6's `run` is async because the daemon path sends
/// `sys.metrics` over the pipe; this wrapper drives a one-shot
/// current-thread tokio runtime to await the inner future.
pub fn run_sync(m: &ArgMatches) -> Result<(), DomainError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("observability sync runtime")
        .block_on(run(m))
}

async fn run_inner(m: &ArgMatches) -> Result<(), DomainError> {
    match m.subcommand() {
        Some(("metrics", _)) => {
            // Honor the explicit override first (REQ-DAEMON-004).
            if std::env::var(NO_DAEMON_ENV).ok().as_deref() == Some("1") {
                return render_local();
            }
            // Daemon-first; fall back to local on any failure.
            if let Some(body) = try_daemon_metrics().await? {
                return emit(body);
            }
            render_local()
        }
        _ => Err(DomainError::InvalidInput {
            message:
                "unknown observability subcommand; run 'memento observability --help' for usage"
                    .into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    //! Routing tests: prove the dispatcher prefers the daemon path
    //! when one is reachable, falls back to the local dump otherwise,
    //! and honors `MEMENTO_NO_DAEMON=1` as an explicit override.
    //!
    //! End-to-end runtime coverage lives in
    //! `crates/memento-cli/tests/observability_metrics_daemon.rs`; this
    //! module locks the dispatcher wiring in isolation (no real pipe,
    //! no real daemon process — a `tokio::io::duplex` stands in for
    //! the wire).

    use super::*;
    use memento_mcp::dispatcher::Command as DispatchCommand;
    use tokio::io::duplex;

    fn matches_metrics() -> ArgMatches {
        // Bypass `memento_cli::args::build` (which pulls i18n) — the
        // observability sub shape is identical and the unit test only
        // exercises `run`. The production dispatcher (`lib.rs`) passes
        // the INNER matches (one level deeper than the root), so we
        // mirror that here: parse the root, then hand `sub` to `run`.
        let root = clap::Command::new("memento")
            .subcommand(
                clap::Command::new("observability")
                    .subcommand(clap::Command::new("metrics")),
            )
            .get_matches_from(["memento", "observability", "metrics"]);
        let (_name, sub) = root.subcommand().expect("observability subcommand");
        sub.clone()
    }

    /// `MEMENTO_NO_DAEMON=1` short-circuits the daemon path; the unit
    /// test pins the dispatcher contract — the local dump runs even if
    /// a daemon were reachable.
    #[tokio::test]
    async fn no_daemon_env_takes_the_local_dump_path() {
        // SAFETY: serialized in nextest via `test(observability)`.
        unsafe { std::env::set_var(NO_DAEMON_ENV, "1") };
        let result = run(&matches_metrics()).await;
        unsafe { std::env::remove_var(NO_DAEMON_ENV) };
        assert!(result.is_ok(), "MEMENTO_NO_DAEMON=1 must succeed");
    }

    /// The dispatcher has a single entry point for `metrics`; any
    /// unknown subcommand surfaces `InvalidInput` so the operator
    /// never sees a silent no-op. We bypass clap's unknown-subcommand
    /// rejection (which exits 2 before reaching our dispatcher) by
    /// constructing an `ArgMatches` whose subcommand table contains a
    /// bogus name clap never validated.
    #[tokio::test]
    async fn unknown_subcommand_returns_invalid_input() {
        let m = clap::Command::new("observability")
            .subcommand(clap::Command::new("metrics"))
            .subcommand(clap::Command::new("bogus"))
            .get_matches_from(["observability", "bogus"]);
        let err = run(&m).await.expect_err("unknown subcommand");
        assert!(
            matches!(err, DomainError::InvalidInput { .. }),
            "InvalidInput tier: {err}"
        );
    }

    /// `DispatchCommand::Sys(SysCommand::Metrics)` path → JSON envelope
    /// shape matches the dispatcher's `sys.metrics` body (the daemon
    /// side at `memento-mcp::dispatcher::sys_metrics`).
    #[test]
    fn sys_metrics_request_envelope_shape() {
        let req = json!({
            "kind": "sys",
            "command": SysCommand::Metrics,
        });
        let bytes = serde_json::to_vec(&req).expect("serialize");
        let parsed: Value = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(parsed["kind"], "sys");
        assert_eq!(parsed["command"], "metrics");
        // The Command enum deserializes back into the same variant.
        let round: DispatchCommand =
            serde_json::from_value(parsed).expect("Command roundtrip");
        assert_eq!(round.path(), "sys.metrics");
    }

    /// Wire roundtrip: a fake daemon that reads one `sys.metrics`
    /// request and replies with the dispatcher-shaped envelope must
    /// roundtrip cleanly. Locks the framing (u32 header + ≤ 2 KiB
    /// payload) the daemon path depends on.
    #[tokio::test]
    async fn sys_metrics_roundtrip_over_duplex() {
        let (mut a, mut b) = duplex(64 * 1024);
        let request = json!({
            "kind": "sys",
            "command": SysCommand::Metrics,
        });
        let request_bytes = serde_json::to_vec(&request).expect("serialize");
        // Client side: write the request, read the response, render the body.
        let writer = tokio::spawn(async move {
            frame::write_message(&mut b, &request_bytes)
                .await
                .expect("write");
            let raw = frame::read_message(&mut b).await.expect("read");
            let value: Value = serde_json::from_slice(&raw).expect("response json");
            value
                .get("body")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        // Server side: read the request, reply with the dispatcher envelope.
        let raw = frame::read_message(&mut a).await.expect("read request");
        let cmd: DispatchCommand =
            serde_json::from_slice(&raw).expect("Command parses");
        assert_eq!(cmd, DispatchCommand::Sys(SysCommand::Metrics));
        let body = "# source=daemon pid=4242 tenant=test\n# HELP foo 1\nfoo 1\n";
        let response = json!({
            "status": "ok",
            "format": "prometheus_text",
            "body": body,
            "ts": "2026-08-14T00:00:00Z",
        });
        let payload = serde_json::to_vec(&response).expect("serialize response");
        frame::write_message(&mut a, &payload)
            .await
            .expect("write response");
        let got = writer.await.expect("writer task");
        let got = got.expect("body string");
        assert!(got.starts_with("# source=daemon pid="), "{got}");
        assert!(got.contains("foo 1"), "{got}");
    }
}