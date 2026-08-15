//! memento — Memento RS command-line binary (cluster I).
//!
//! Thin entry point: resolve the locale (bilingual help — REQ-CL-004),
//! parse the tree, dispatch to [`memento_cli::run`], and exit with the
//! deterministic [`DomainError::exit_code`] (REQ-CL-005).

use memento_i18n::I18n;

#[tokio::main]
async fn main() {
    // REQ-OBS-001: stderr tracing subscriber, env-gated (MEMENTO_LOG=1 AND
    // --json absent from argv — the equivalence harness needs byte-pure
    // stderr on JSON error paths). Must run before clap consumes argv.
    memento_observability::tracing::init_cli_subscriber();

    // `--no-daemon` (REQ-DAEMON-004, design D7): pre-scan argv and
    // mirror as `MEMENTO_NO_DAEMON=1` BEFORE any code reads env. Runs
    // synchronously in the current thread, before the runtime polls.
    let _ = memento_cli::args::no_daemon_from_argv();

    // Locale must be known BEFORE clap parses: the help tree is built from
    // the i18n tables, so `--locale` is pre-scanned from argv (args.rs).
    let locale = memento_cli::args::locale_from_argv();
    let i18n = I18n::load(locale);

    let matches = memento_cli::args::build(&i18n).get_matches();
    let json = matches.get_flag("json");

    let code = match memento_cli::run(&matches, &i18n).await {
        Ok(()) => 0,
        Err(err) => {
            memento_cli::output::report_error(&err, &i18n, json);
            err.exit_code()
        }
    };
    std::process::exit(code);
}
