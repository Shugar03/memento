//! Bilingual clap v4 command tree (REQ-CL-004).
//!
//! Built with the clap BUILDER API on purpose: derive `#[command(about)]`
//! and `#[arg(help)]` accept only string literals (same limitation as
//! rmcp's `#[tool]`, discovery 2618), which would make help text a second,
//! English-only source of truth. The builder takes every string from the
//! memento-i18n ES-first tables at runtime — single source of truth, and
//! `--locale en` switches the whole tree to the EN fallback.

use clap::{Arg, ArgAction, ArgGroup, Command};
use memento_i18n::{I18n, Locale, StringKey};

/// The full bilingual command tree.
pub fn build(i18n: &I18n) -> Command {
    Command::new("memento")
        .version(env!("CARGO_PKG_VERSION"))
        .about(i18n.t(StringKey::CliHelpRoot))
        .arg(json_flag(i18n))
        .arg(no_embeddings_flag(i18n))
        .arg(root_arg(i18n))
        .arg(locale_arg(i18n))
        .subcommand(tenant_cmd(i18n))
        .subcommand(ingest_cmd(i18n))
        .subcommand(search_cmd(i18n))
        .subcommand(get_chunk_cmd(i18n))
        .subcommand(feedback_cmd(i18n))
        .subcommand(delete_cmd(i18n))
        .subcommand(context_fit_cmd(i18n))
        .subcommand(code_cmd(i18n))
        .subcommand(stats_cmd(i18n))
        .subcommand(health_cmd(i18n))
        .subcommand(observability_cmd(i18n))
        .subcommand(daemon_cmd(i18n))
}

// ---- global flags ----------------------------------------------------------

fn json_flag(i18n: &I18n) -> Arg {
    Arg::new("json")
        .long("json")
        .global(true)
        .action(ArgAction::SetTrue)
        .help(i18n.t(StringKey::CliHelpJson))
}

fn no_embeddings_flag(i18n: &I18n) -> Arg {
    Arg::new("no-embeddings")
        .long("no-embeddings")
        .global(true)
        .action(ArgAction::SetTrue)
        .help(i18n.t(StringKey::CliHelpNoEmbeddings))
}

fn root_arg(i18n: &I18n) -> Arg {
    Arg::new("root")
        .long("root")
        .global(true)
        .value_name("PATH")
        .value_parser(clap::value_parser!(std::path::PathBuf))
        .help(i18n.t(StringKey::CliHelpRootArg))
}

fn locale_arg(i18n: &I18n) -> Arg {
    Arg::new("locale")
        .long("locale")
        .global(true)
        .value_name("es|en")
        .help(i18n.t(StringKey::CliHelpLocaleArg))
}

fn query_arg(i18n: &I18n) -> Arg {
    Arg::new("query")
        .value_name("TEXT")
        .required(true)
        .help(i18n.t(StringKey::CliHelpQueryArg))
}

/// The ingest-text positional (distinct help from the search query).
fn text_arg(i18n: &I18n) -> Arg {
    Arg::new("text")
        .value_name("TEXT")
        .required(true)
        .help(i18n.t(StringKey::CliHelpTextArg))
}

fn top_k_arg(i18n: &I18n) -> Arg {
    Arg::new("top-k")
        .long("top-k")
        .value_name("N")
        .default_value("20")
        .help(i18n.t(StringKey::CliHelpTopKArg))
}

fn workspace_arg(i18n: &I18n) -> Arg {
    Arg::new("workspace")
        .long("workspace")
        .value_name("UUID")
        .help(i18n.t(StringKey::CliHelpWorkspaceArg))
}

fn rrf_flag(i18n: &I18n) -> Arg {
    Arg::new("rrf")
        .long("rrf")
        .action(ArgAction::SetTrue)
        .help(i18n.t(StringKey::CliHelpRrfArg))
}

fn rrf_k_arg(i18n: &I18n) -> Arg {
    Arg::new("rrf-k")
        .long("rrf-k")
        .value_name("K")
        .default_value("60")
        .help(i18n.t(StringKey::CliHelpRrfKArg))
}

fn rerank_flag(i18n: &I18n) -> Arg {
    Arg::new("rerank")
        .long("rerank")
        .action(ArgAction::SetTrue)
        .help(i18n.t(StringKey::CliHelpRerankArg))
}

// ---- tenant ----------------------------------------------------------------

fn tenant_cmd(i18n: &I18n) -> Command {
    Command::new("tenant")
        .about(i18n.t(StringKey::CliHelpTenant))
        .subcommand(
            Command::new("create")
                .about(i18n.t(StringKey::CliHelpTenantCreate))
                .arg(
                    Arg::new("name")
                        .long("name")
                        .value_name("NAME")
                        .required(true)
                        .help(i18n.t(StringKey::CliHelpNameArg)),
                ),
        )
        .subcommand(Command::new("rotate-token").about(i18n.t(StringKey::CliHelpRotateToken)))
        .subcommand(Command::new("delete").about(i18n.t(StringKey::CliHelpTenantDelete)))
        .subcommand(
            Command::new("retention")
                .about(i18n.t(StringKey::CliHelpTenantRetention))
                .arg(
                    Arg::new("days")
                        .long("days")
                        .value_name("N")
                        .help(i18n.t(StringKey::CliHelpDaysArg)),
                ),
        )
        .subcommand(Command::new("export").about(i18n.t(StringKey::CliHelpTenantExport)))
        .subcommand(Command::new("backup").about(i18n.t(StringKey::CliHelpTenantBackup)))
        .subcommand(
            Command::new("restore")
                .about(i18n.t(StringKey::CliHelpTenantRestore))
                .arg(
                    Arg::new("backup-dir")
                        .value_name("DIR")
                        .required(true)
                        .help(i18n.t(StringKey::CliHelpBackupDirArg)),
                ),
        )
        .subcommand(Command::new("sweep").about(i18n.t(StringKey::CliHelpTenantSweep)))
}

// ---- ingest ----------------------------------------------------------------

fn ingest_cmd(i18n: &I18n) -> Command {
    Command::new("ingest")
        .about(i18n.t(StringKey::CliHelpIngest))
        .subcommand(
            Command::new("text")
                .about(i18n.t(StringKey::CliHelpIngest))
                .arg(text_arg(i18n))
                .arg(
                    Arg::new("doc-id")
                        .long("doc-id")
                        .value_name("UUID")
                        .help(i18n.t(StringKey::CliHelpDocArg)),
                ),
        )
        .subcommand(
            Command::new("document")
                .about(i18n.t(StringKey::CliHelpIngestDocument))
                .arg(
                    Arg::new("file")
                        .value_name("FILE")
                        .required(true)
                        .help(i18n.t(StringKey::CliHelpFileArg)),
                )
                .arg(
                    Arg::new("source")
                        .long("source")
                        .value_name("SOURCE")
                        .help(i18n.t(StringKey::CliHelpSourceArg)),
                )
                .arg(
                    Arg::new("doc-id")
                        .long("doc-id")
                        .value_name("UUID")
                        .help(i18n.t(StringKey::CliHelpDocArg)),
                ),
        )
        .subcommand(
            Command::new("bulk")
                .about(i18n.t(StringKey::CliHelpIngestBulk))
                .arg(
                    Arg::new("dir")
                        .value_name("DIR")
                        .required(true)
                        .help(i18n.t(StringKey::CliHelpDirArg)),
                ),
        )
}

// ---- memory ops ------------------------------------------------------------

fn search_cmd(i18n: &I18n) -> Command {
    Command::new("search")
        .about(i18n.t(StringKey::CliHelpSearch))
        .arg(query_arg(i18n))
        .arg(top_k_arg(i18n))
        .arg(workspace_arg(i18n))
        .arg(rrf_flag(i18n))
        .arg(rrf_k_arg(i18n))
        .arg(rerank_flag(i18n))
        .arg(
            Arg::new("doc-id")
                .long("doc-id")
                .value_name("UUID")
                .help(i18n.t(StringKey::CliHelpDocArg)),
        )
        .arg(
            Arg::new("source")
                .long("source")
                .value_name("SOURCE")
                .help(i18n.t(StringKey::CliHelpSourceArg)),
        )
}

fn get_chunk_cmd(i18n: &I18n) -> Command {
    Command::new("get-chunk")
        .about(i18n.t(StringKey::CliHelpGetChunk))
        .arg(
            Arg::new("chunk")
                .value_name("CHUNK_ID")
                .required(true)
                .help(i18n.t(StringKey::CliHelpChunkArg)),
        )
}

fn feedback_cmd(i18n: &I18n) -> Command {
    Command::new("feedback")
        .about(i18n.t(StringKey::CliHelpFeedback))
        .arg(
            Arg::new("chunk")
                .value_name("CHUNK_ID")
                .required(true)
                .help(i18n.t(StringKey::CliHelpChunkArg)),
        )
        .arg(
            Arg::new("useful")
                .long("useful")
                .action(ArgAction::SetTrue)
                .help(i18n.t(StringKey::CliHelpUsefulArg)),
        )
        .arg(
            Arg::new("not-useful")
                .long("not-useful")
                .action(ArgAction::SetTrue)
                .help(i18n.t(StringKey::CliHelpNotUsefulArg)),
        )
        .arg(
            Arg::new("reason")
                .long("reason")
                .value_name("TEXT")
                .help(i18n.t(StringKey::CliHelpReasonArg)),
        )
        .group(
            ArgGroup::new("rating")
                .args(["useful", "not-useful"])
                .required(true),
        )
}

fn delete_cmd(i18n: &I18n) -> Command {
    Command::new("delete")
        .about(i18n.t(StringKey::CliHelpDelete))
        .arg(
            Arg::new("chunk")
                .long("chunk")
                .value_name("CHUNK_ID")
                .help(i18n.t(StringKey::CliHelpChunkArg)),
        )
        .arg(
            Arg::new("doc")
                .long("doc")
                .value_name("DOC_ID")
                .help(i18n.t(StringKey::CliHelpDocArg)),
        )
        .arg(
            Arg::new("workspace")
                .long("workspace")
                .value_name("UUID")
                .help(i18n.t(StringKey::CliHelpWorkspaceArg)),
        )
        .arg(
            Arg::new("tenant")
                .long("tenant")
                .action(ArgAction::SetTrue)
                .help(i18n.t(StringKey::CliHelpTenantDelete)),
        )
        .group(
            ArgGroup::new("scope")
                .args(["chunk", "doc", "workspace", "tenant"])
                .required(true),
        )
}

fn context_fit_cmd(i18n: &I18n) -> Command {
    Command::new("context-fit")
        .about(i18n.t(StringKey::CliHelpContextFit))
        .arg(query_arg(i18n))
        .arg(
            Arg::new("budget")
                .long("budget")
                .value_name("TOKENS")
                .required(true)
                .help(i18n.t(StringKey::CliHelpBudgetArg)),
        )
        .arg(top_k_arg(i18n))
        .arg(workspace_arg(i18n))
        .arg(rrf_flag(i18n))
        .arg(rrf_k_arg(i18n))
}

// ---- code ------------------------------------------------------------------

fn code_cmd(i18n: &I18n) -> Command {
    Command::new("code")
        .about(i18n.t(StringKey::CliHelpCodeIndex))
        .subcommand(
            Command::new("index")
                .about(i18n.t(StringKey::CliHelpCodeIndex))
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .help(i18n.t(StringKey::CliHelpPathArg)),
                ),
        )
        .subcommand(
            Command::new("status")
                .about(i18n.t(StringKey::CliHelpCodeStatus))
                .arg(
                    Arg::new("project")
                        .long("project")
                        .value_name("PROJECT_ID")
                        .help(i18n.t(StringKey::CliHelpProjectArg)),
                ),
        )
        .subcommand(
            Command::new("debug")
                .about(i18n.t(StringKey::CliHelpCodeDebug))
                .arg(
                    Arg::new("project")
                        .value_name("PROJECT_ID")
                        .required(true)
                        .help(i18n.t(StringKey::CliHelpProjectArg)),
                ),
        )
}

// ---- stats + health --------------------------------------------------------

fn stats_cmd(i18n: &I18n) -> Command {
    Command::new("stats").about(i18n.t(StringKey::CliHelpStats))
}

fn health_cmd(i18n: &I18n) -> Command {
    Command::new("health").about(i18n.t(StringKey::CliHelpHealth))
}

// ---- observability ----------------------------------------------------------

/// `observability metrics`: the Prometheus text dump (REQ-OBS-007, design
/// D7). Process-local and root-independent — no tenant context is required,
/// and no HTTP listener is ever bound.
fn observability_cmd(i18n: &I18n) -> Command {
    Command::new("observability")
        .about(i18n.t(StringKey::CliHelpObservability))
        .subcommand(Command::new("metrics").about(i18n.t(StringKey::CliHelpObservabilityMetrics)))
}

// ---- daemon ------------------------------------------------------------------

/// `daemon start|stop|status`: the persistent-daemon control plane
/// (REQ-DAEMON-007, design D4). Never opens `AppService`; never loads
/// models; honors `MEMENTO_NO_DAEMON=1` on `status`.
fn daemon_cmd(i18n: &I18n) -> Command {
    Command::new("daemon")
        .about(i18n.t(StringKey::CliHelpDaemon))
        .subcommand(Command::new("start").about(i18n.t(StringKey::CliHelpDaemonStart)))
        .subcommand(Command::new("stop").about(i18n.t(StringKey::CliHelpDaemonStop)))
        .subcommand(Command::new("status").about(i18n.t(StringKey::CliHelpDaemonStatus)))
}

// ---- env / flag resolution -------------------------------------------------

/// Interface locale: `--locale <v>` (pre-scan of argv — the help tree must
/// be built with the locale BEFORE clap parses) > `MEMENTO_LOCALE` > ES
/// (the default).
pub fn locale_from_argv() -> Locale {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("--locale=") {
            return parse_locale(value);
        }
        if arg == "--locale"
            && let Some(value) = args.next()
        {
            return parse_locale(&value);
        }
    }
    std::env::var("MEMENTO_LOCALE")
        .map(|raw| parse_locale(&raw))
        .unwrap_or(Locale::Es)
}

fn parse_locale(raw: &str) -> Locale {
    match raw.to_ascii_lowercase().as_str() {
        "en" | "english" => Locale::En,
        _ => Locale::Es,
    }
}
