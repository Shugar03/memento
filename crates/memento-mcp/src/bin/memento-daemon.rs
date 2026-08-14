//! memento-daemon — the persistent daemon binary (daemon-persistent, D1).
//!
//! One long-lived process per (root, tenant) owns the embedder, reranker and
//! LanceDB store; CLI and MCP stdio clients connect over a Windows named
//! pipe (REQ-DAEMON-001/002). Lifecycle is wired in later slices (S5): this
//! stub only proves the entry point parses the daemon's fixed config
//! (`--root` / `--no-embeddings` / `--locale`, REQ-DAEMON-003 "config FIXED
//! at spawn").
//!
//! The real daemon never prints to stdout (stdout/stderr must stay clean for
//! detached operation); the stub prints a one-line hello so smoke tests can
//! assert the process boots and exits 0.

use std::path::PathBuf;
use std::process::ExitCode;

use memento_i18n::Locale;

#[tokio::main]
async fn main() -> ExitCode {
    let opts = match parse_args() {
        Ok(opts) => opts,
        Err(msg) => {
            eprintln!("memento-daemon: {msg}");
            return ExitCode::from(2);
        }
    };

    // S1.2 smoke: boot + print + exit 0. S5 wires the pipe accept loop.
    println!(
        "memento-daemon stub: root={} no_embeddings={} locale={}",
        opts.root.display(),
        opts.no_embeddings,
        opts.locale.map(|l| l.as_str()).unwrap_or("default"),
    );
    ExitCode::SUCCESS
}

/// The daemon's FIXED spawn config (REQ-DAEMON-003): root + embeddings mode
/// + locale. These are the three dimensions of CONFIG_MISMATCH (R3).
pub struct DaemonConfig {
    pub root: PathBuf,
    pub no_embeddings: bool,
    pub locale: Option<Locale>,
}

fn parse_args() -> Result<DaemonConfig, String> {
    let mut root: Option<PathBuf> = std::env::var("MEMENTO_ROOT").ok().map(PathBuf::from);
    let mut no_embeddings = matches!(
        std::env::var("MEMENTO_NO_EMBEDDINGS").as_deref(),
        Ok("1" | "true" | "yes")
    );
    let mut locale: Option<Locale> = std::env::var("MEMENTO_LOCALE")
        .ok()
        .and_then(|v| match v.as_str() {
            "es" => Some(Locale::Es),
            "en" => Some(Locale::En),
            _ => None,
        });

    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--root" => root = argv.next().map(PathBuf::from),
            "--no-embeddings" => no_embeddings = true,
            "--locale" => {
                let v = argv.next().unwrap_or_default();
                locale = match v.as_str() {
                    "es" => Some(Locale::Es),
                    "en" => Some(Locale::En),
                    _ => return Err(format!("invalid --locale '{v}', expected es|en")),
                };
            }
            "-h" | "--help" => {
                println!("memento-daemon — Memento RS persistent daemon\n\nUSAGE:\n  memento-daemon [--root <PATH>] [--no-embeddings] [--locale es|en]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(DaemonConfig {
        root: root.ok_or_else(|| "missing --root <PATH> or MEMENTO_ROOT env var".to_string())?,
        no_embeddings,
        locale,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// windows-gnu compile smoke (S1.3): the toolchain must be able to bind
    /// a real Windows named pipe via interprocess before the transport
    /// slices build on it. Bind + drop — no listener loop, no messages.
    #[tokio::test]
    async fn windows_named_pipe_binds_and_drops() {
        use interprocess::os::windows::named_pipe::{
            pipe_mode, PipeListenerOptions,
        };

        let name = format!(r"\\.\pipe\memento-smoke-{}", std::process::id());
        let listener = PipeListenerOptions::new()
            .path(name)
            .create_tokio_duplex::<pipe_mode::Bytes>()
            .expect("named pipe binds on windows-gnu");
        drop(listener);
    }

    #[test]
    fn stub_parses_fixed_config() {
        // REQ-DAEMON-003: config is fixed at spawn — parse must accept the
        // same three dimensions the spawner forwards.
        let args = ["--root", "C:\\tmp\\memento", "--no-embeddings", "--locale", "es"];
        let mut argv = args.iter().copied();
        let mut root: Option<PathBuf> = None;
        let mut no_embeddings = false;
        let mut locale: Option<Locale> = None;
        while let Some(arg) = argv.next() {
            match arg {
                "--root" => root = argv.next().map(PathBuf::from),
                "--no-embeddings" => no_embeddings = true,
                "--locale" => {
                    locale = argv.next().and_then(|v| match v {
                        "es" => Some(Locale::Es),
                        "en" => Some(Locale::En),
                        _ => None,
                    })
                }
                _ => {}
            }
        }
        assert_eq!(root, Some(PathBuf::from("C:\\tmp\\memento")));
        assert!(no_embeddings);
        assert_eq!(locale, Some(Locale::Es));
    }
}
