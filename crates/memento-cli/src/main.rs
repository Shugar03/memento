//! memento — Memento RS command-line binary (cluster I).
//!
//! Thin entry point: resolve the locale (bilingual help — REQ-CL-004),
//! parse the tree, dispatch to [`memento_cli::run`], and exit with the
//! deterministic [`DomainError::exit_code`] (REQ-CL-005).

use memento_i18n::I18n;

#[tokio::main]
async fn main() {
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
