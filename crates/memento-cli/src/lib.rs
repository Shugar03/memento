//! memento-cli — Memento RS command-line surface (cluster I).
//!
//! The CLI is a THIN adapter (REQ-MS-006, same core semantics as the MCP
//! surface): it parses arguments, resolves the process-bound tenant
//! (REQ-TA-002/003), and delegates every operation to [`AppService`].
//! Zero domain behavior lives here.
//!
//! # Output contract (REQ-CL-003/005)
//!
//! * `--json` (global) produces machine-readable output carrying the same
//!   canonical fields as the MCP tools (provenance included); the default is
//!   human-readable lines.
//! * Exit codes are [`DomainError::exit_code`] (REQ-CL-005): `0` success,
//!   `4` auth failure, `2` validation error, `1` internal, etc. Errors are
//!   bilingual (ES-first, EN fallback — REQ-CL-004) via memento-i18n, both
//!   in human mode (stderr) and in `--json` mode (structured JSON on stderr).
//!
//! # Command tree (see [`args`] for the bilingual builder)
//!
//! ```text
//! memento [--json] [--no-embeddings] [--root <path>] [--locale es|en]
//! ├── tenant create|rotate-token|delete|retention|export|backup|restore|sweep
//! ├── ingest text|document|bulk
//! ├── search | get-chunk | feedback | delete | context-fit
//! ├── code index|status|debug
//! ├── stats
//! ├── health
//! └── observability metrics
//! ```
//!
//! Only `tenant create` runs unauthenticated (bootstrap path, REQ-TA-006);
//! every other command resolves `MEMENTO_TOKEN` + `MEMENTO_AGENT_ID` and
//! refuses to run without valid credentials (REQ-MS-003 semantics — nothing
//! is served unauthenticated, REQ-CL-005 scenario).

pub mod args;
pub mod commands;
pub mod output;
pub mod spawn;
pub mod startup;
pub mod transport;

use std::path::Path;

use clap::ArgMatches;
use memento_domain::DomainError;
use memento_i18n::I18n;

/// Resolve the storage root: `--root` flag > `MEMENTO_ROOT` env >
/// `~/.memento` (design D8).
pub fn resolve_root(matches: &ArgMatches) -> Result<std::path::PathBuf, DomainError> {
    if let Some(root) = matches.get_one::<std::path::PathBuf>("root") {
        return Ok(root.clone());
    }
    if let Some(root) = std::env::var_os("MEMENTO_ROOT") {
        return Ok(std::path::PathBuf::from(root));
    }
    memento_tenant::default_root()
}

/// Dispatch one parsed invocation to the matching command module.
///
/// # Errors
///
/// Every `DomainError` propagates to [`main`] which renders it bilingually
/// and exits with the deterministic code (REQ-CL-005).
pub async fn run(matches: &ArgMatches, i18n: &I18n) -> Result<(), DomainError> {
    let root = resolve_root(matches)?;
    let no_embeddings = matches.get_flag("no-embeddings");
    let json = matches.get_flag("json");
    match matches.subcommand() {
        Some(("tenant", sub)) => commands::tenant::run(sub, &root, no_embeddings, i18n).await,
        // Process-local observability dump: no app open, no credentials
        // (REQ-OBS-007, design D7 — tenant-create bootstrap precedent).
        // B6 (REQ-DAEMON-010, R5): the dispatcher is async because the
        // daemon path sends `sys.metrics` over the named pipe; on any
        // failure it falls back to the local dump.
        Some(("observability", sub)) => commands::observability::run(sub).await,
        // Daemon control plane (REQ-DAEMON-007): no app open, never loads
        // models. B5: `start`/`stop` are real calls into
        // `DaemonSpawner::start/stop`; `status` pings the pipe.
        Some(("daemon", sub)) => commands::daemon::run(sub, json).await,
        Some((
            name @ ("ingest" | "search" | "get-chunk" | "feedback" | "delete" | "context-fit"
            | "code" | "stats" | "health"),
            sub,
        )) => {
            let app = startup::open(&root, no_embeddings).await?;
            match name {
                "ingest" => commands::ingest::run(sub, &app).await,
                "search" => commands::memory::run_search(sub, &app).await,
                "get-chunk" => commands::memory::run_get_chunk(sub, &app).await,
                "feedback" => commands::memory::run_feedback(sub, &app).await,
                "delete" => commands::memory::run_delete(sub, &app, i18n).await,
                "context-fit" => commands::memory::run_context_fit(sub, &app).await,
                "code" => commands::code::run(sub, &app).await,
                "stats" => commands::stats::run_stats(sub, &app).await,
                "health" => commands::stats::run_health(sub, &app).await,
                _ => unreachable!("guarded by the match arm"),
            }
        }
        _ => Err(DomainError::InvalidInput {
            message: "unknown command; run 'memento --help' for usage".into(),
        }),
    }
}

/// B5 daemon-aware startup entry point used by tests + the integration
/// harness. Mirrors the `daemon`-subcommand path: probe → spawn on miss
/// → retry connect → tag the result as `Local` or `Remote`. See
/// [`startup::try_open`] for the full semantics.
pub async fn open_daemon_aware(
    root: &Path,
    no_embeddings: bool,
) -> Result<startup::CliBackend, DomainError> {
    startup::try_open(root, no_embeddings).await
}
