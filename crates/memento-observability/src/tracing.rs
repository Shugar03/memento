//! Per-binary tracing subscribers on stderr (REQ-OBS-001/002, design D4).
//!
//! Three shared subscriber constructors — one per binary — so the setup
//! cannot drift: `init_cli_subscriber` (env-gated by `MEMENTO_LOG=1` and
//! skipping `--json` runs to keep stderr byte-pure for the equivalence
//! harness), `init_mcp_subscriber`, and `init_worker_subscriber`.
//!
//! All three honor `RUST_LOG` via `EnvFilter` (with a per-binary default:
//! CLI `info,memento=info`, MCP `info,memento_mcp=info`, worker `info`) and
//! `MEMENTO_LOG_FORMAT=pretty|json` (default pretty). Output goes to stderr
//! only — stdout stays machine-pure (REQ-OBS-001). Without a subscriber,
//! `tracing` macros are no-ops, so instrumented hot paths cost nothing
//! (REQ-OBS-004).

use tracing_subscriber::EnvFilter;

/// Log output format selected by `MEMENTO_LOG_FORMAT` (REQ-OBS-002).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// Human-readable pretty formatter (default).
    Pretty,
    /// One JSON object per line.
    Json,
}

/// Resolve `MEMENTO_LOG_FORMAT` (pure: takes the raw env value so the gate
/// is testable without touching the process environment). Anything that is
/// not exactly `json` resolves to pretty (REQ-OBS-002).
pub fn resolve_format(value: Option<&str>) -> LogFormat {
    match value {
        Some("json") => LogFormat::Json,
        _ => LogFormat::Pretty,
    }
}

/// Read `MEMENTO_LOG_FORMAT` from the process environment.
pub fn log_format_from_env() -> LogFormat {
    resolve_format(std::env::var("MEMENTO_LOG_FORMAT").ok().as_deref())
}

/// CLI subscriber gate (REQ-OBS-001): enabled only when `MEMENTO_LOG=1` AND
/// `--json` is not among the CLI args. JSON error paths must stay
/// byte-identical to the pre-change binary (equivalence harness), so no
/// tracing line may ever touch stderr there.
pub fn cli_gate(log_var: Option<&str>, args: &[String]) -> bool {
    log_var == Some("1") && !args.iter().any(|arg| arg == "--json")
}

/// Read the CLI gate from the process environment and argv.
pub fn cli_gate_from_env() -> bool {
    cli_gate(
        std::env::var("MEMENTO_LOG").ok().as_deref(),
        &std::env::args().collect::<Vec<_>>(),
    )
}

/// CLI default EnvFilter (design D4 apply note; `info` + `memento` crates).
pub const CLI_DEFAULT_FILTER: &str = "info,memento=info";
/// MCP default EnvFilter (design D4 apply note).
pub const MCP_DEFAULT_FILTER: &str = "info,memento_mcp=info";
/// Worker default EnvFilter (design D4 apply note).
pub const WORKER_DEFAULT_FILTER: &str = "info";

/// Install a stderr subscriber with `default_filter` (used when RUST_LOG is
/// unset). Installation is best-effort: a process installs its subscriber
/// exactly once at startup; if one is already set, we keep the first.
fn install(format: LogFormat, default_filter: &str) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    let _ = match format {
        LogFormat::Json => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .with_writer(std::io::stderr)
            .try_init(),
        LogFormat::Pretty => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .try_init(),
    };
}

/// Install the CLI subscriber. Does nothing unless `MEMENTO_LOG=1` and
/// `--json` is absent from argv (REQ-OBS-001).
pub fn init_cli_subscriber() {
    if cli_gate_from_env() {
        install(log_format_from_env(), CLI_DEFAULT_FILTER);
    }
}

/// Install the MCP subscriber (always on; MCP has no stdout contract to
/// protect — its protocol runs over stdio).
pub fn init_mcp_subscriber() {
    install(log_format_from_env(), MCP_DEFAULT_FILTER);
}

/// Install the worker subscriber (always on; daemon logs to stderr).
pub fn init_worker_subscriber() {
    install(log_format_from_env(), WORKER_DEFAULT_FILTER);
}

#[cfg(test)]
mod tests {
    use super::{
        CLI_DEFAULT_FILTER, LogFormat, MCP_DEFAULT_FILTER, WORKER_DEFAULT_FILTER, cli_gate,
        resolve_format,
    };

    #[test]
    fn format_defaults_to_pretty() {
        // REQ-OBS-002: MEMENTO_LOG_FORMAT unset, empty, or unknown → pretty.
        assert_eq!(resolve_format(None), LogFormat::Pretty);
        assert_eq!(resolve_format(Some("")), LogFormat::Pretty);
        assert_eq!(resolve_format(Some("bogus")), LogFormat::Pretty);
    }

    #[test]
    fn format_json_selected() {
        // REQ-OBS-002: MEMENTO_LOG_FORMAT=json → JSON layer.
        assert_eq!(resolve_format(Some("json")), LogFormat::Json);
    }

    #[test]
    fn cli_gate_requires_log_var() {
        // REQ-OBS-001: MEMENTO_LOG unset or not "1" → CLI subscriber disabled.
        assert!(!cli_gate(None, &[]));
        assert!(!cli_gate(Some("0"), &["memento".to_string()]));
        assert!(!cli_gate(Some(""), &["memento".to_string()]));
    }

    #[test]
    fn cli_gate_enabled_with_log_and_no_json() {
        // REQ-OBS-001: MEMENTO_LOG=1 without --json → subscriber enabled.
        assert!(cli_gate(
            Some("1"),
            &["memento".to_string(), "search".to_string()]
        ));
    }

    #[test]
    fn cli_gate_disabled_in_json_mode() {
        // REQ-OBS-001: --json mode must keep stderr byte-pure (equivalence).
        assert!(!cli_gate(
            Some("1"),
            &["memento".to_string(), "--json".to_string()]
        ));
    }

    #[test]
    fn default_filters_per_binary() {
        // Design D4 apply note: CLI `info,memento=info`; MCP
        // `info,memento_mcp=info`; worker `info`. RUST_LOG overrides at init.
        assert!(CLI_DEFAULT_FILTER.starts_with("info,memento="));
        assert!(MCP_DEFAULT_FILTER.contains("memento_mcp=info"));
        assert_eq!(WORKER_DEFAULT_FILTER, "info");
    }
}
