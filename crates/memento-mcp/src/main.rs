//! memento-mcp-server — stdio MCP server binary for Memento RS.
//!
//! Thin entry point that:
//! 1. Resolves startup options from CLI args (root, no-embeddings, locale).
//! 2. Boots the [`McpServer`] (binds the tenant from `MEMENTO_TOKEN` +
//!    `MEMENTO_AGENT_ID`, opens the app service, assembles the 15-tool
//!    registry).
//! 3. Serves the registry over the rmcp stdio transport until the client
//!    closes the pipe.
//!
//! Identity follows REQ-MS-003/REQ-TA-002/003/006: missing or invalid
//! credentials fail fast at startup; nothing is served unauthenticated.

use std::path::PathBuf;
use std::process::ExitCode;

use memento_i18n::Locale;
use memento_mcp::{McpServer, StartupOptions};
use rmcp::ServiceExt;

#[tokio::main]
async fn main() -> ExitCode {
    // Shared MCP subscriber (design D4): always on over stdio, honors
    // RUST_LOG + MEMENTO_LOG_FORMAT (REQ-OBS-002).
    memento_observability::tracing::init_mcp_subscriber();
    let opts = match parse_args() {
        Ok(opts) => opts,
        Err(msg) => {
            eprintln!("memento-mcp-server: {msg}");
            return ExitCode::from(2);
        }
    };

    let server = match McpServer::startup(opts).await {
        Ok(server) => server,
        Err(err) => {
            eprintln!(
                "memento-mcp-server: startup failed [{}] {}",
                err.code(),
                err
            );
            return ExitCode::from(1);
        }
    };

    tracing::info!(
        tenant = %server.ctx().tenant_id(),
        locale = server.locale().as_str(),
        tool_count = server.router().list_all().len(),
        "mcp server starting over stdio"
    );

    let (stdin, stdout) = rmcp::transport::io::stdio();
    let running = match server.serve((stdin, stdout)).await {
        Ok(running) => running,
        Err(err) => {
            eprintln!("memento-mcp-server: stdio handshake failed: {err}");
            return ExitCode::from(1);
        }
    };
    let _ = running.waiting().await;
    ExitCode::SUCCESS
}

fn parse_args() -> Result<StartupOptions, String> {
    let mut root: Option<PathBuf> = std::env::var("MEMENTO_ROOT").ok().map(PathBuf::from);
    let mut staging_dir: Option<PathBuf> =
        std::env::var("MEMENTO_STAGING_DIR").ok().map(PathBuf::from);
    let mut no_embeddings = matches!(
        std::env::var("MEMENTO_NO_EMBEDDINGS").as_deref(),
        Ok("1" | "true" | "yes")
    );
    let mut locale: Option<Locale> =
        std::env::var("MEMENTO_LOCALE")
            .ok()
            .and_then(|v| match v.as_str() {
                "es" => Some(Locale::Es),
                "en" => Some(Locale::En),
                other => {
                    eprintln!("memento-mcp-server: unknown locale '{other}', ignoring");
                    None
                }
            });

    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--root" => {
                root = argv.next().map(PathBuf::from);
            }
            "--staging-dir" => {
                staging_dir = argv.next().map(PathBuf::from);
            }
            "--no-embeddings" => {
                no_embeddings = true;
            }
            "--locale" => {
                let v = argv.next().unwrap_or_default();
                locale = match v.as_str() {
                    "es" => Some(Locale::Es),
                    "en" => Some(Locale::En),
                    _ => return Err(format!("invalid --locale '{v}', expected es|en")),
                };
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    let root = root.ok_or_else(|| "missing --root <PATH> or MEMENTO_ROOT env var".to_string())?;
    let staging_dir = staging_dir.unwrap_or_else(|| std::env::temp_dir().join("memento-anydoc"));

    Ok(StartupOptions {
        root,
        staging_dir,
        no_embeddings,
        locale,
    })
}

fn print_help() {
    println!(
        "memento-mcp-server — Memento RS MCP stdio server (rmcp)\n\n\
         USAGE:\n  memento-mcp-server [--root <PATH>] [--no-embeddings]\n\
         \n\
         OPTIONS:\n  \
           --root <PATH>          Storage root (also MEMENTO_ROOT)\n  \
           --staging-dir <PATH>   anydoc staging dir (also MEMENTO_STAGING_DIR)\n  \
           --no-embeddings        Disable the fastembed embedder (also MEMENTO_NO_EMBEDDINGS=1)\n  \
           --locale <es|en>       Surface locale (also MEMENTO_LOCALE)\n  \
           -h, --help             Show this help\n\n\
         REQUIRED ENV (REQ-MS-003):\n  \
           MEMENTO_TOKEN          Tenant API key (memo_<tid>_<secret>)\n  \
           MEMENTO_AGENT_ID       Agent identity bound to every tool call\n"
    );
}
