//! Observability commands (REQ-OBS-007, design D7): the process-local
//! metrics dump. No HTTP listener is ever bound — the exporter is compiled
//! with `default-features=false` (no hyper in the tree), and the dump is a
//! plain text render of THIS process's registry.

use clap::ArgMatches;
use memento_domain::DomainError;
use std::path::PathBuf;

/// The dump destination override (REQ-OBS-007): `MEMENTO_METRICS_FILE` when
/// set, stdout otherwise.
fn file_override() -> Option<PathBuf> {
    std::env::var_os("MEMENTO_METRICS_FILE").map(PathBuf::from)
}

/// `observability metrics`: render the registry as Prometheus text to
/// stdout, or to `MEMENTO_METRICS_FILE` when set (REQ-OBS-007, D7).
///
/// Process-local and root-independent: no tenant context, no credentials,
/// no app open. Always exits 0 — with an empty registry while
/// `MEMENTO_METRICS` is off (the recorder is never installed, `render()`
/// returns `""`).
pub fn run(m: &ArgMatches) -> Result<(), DomainError> {
    match m.subcommand() {
        Some(("metrics", _)) => {
            let dump = memento_observability::metrics::render();
            match file_override() {
                Some(path) => std::fs::write(&path, &dump).map_err(DomainError::from)?,
                None => print!("{dump}"),
            }
            Ok(())
        }
        _ => Err(DomainError::InvalidInput {
            message: "unknown observability subcommand; run 'memento observability --help' for usage"
                .into(),
        }),
    }
}
