//! memento-mcp-server — stdio MCP server binary for Memento RS.
//!
//! Thin entry point that:
//! 1. Resolves startup options from CLI args (root, no-embeddings, locale).
//! 2. Selects the carrier (REQ-DAEMON-008, design D1/S4.5):
//!    - Daemon available (root+tenant+config consistent, no
//!      `MEMENTO_NO_DAEMON`) → the server becomes a THIN stdio→pipe
//!      proxy ([`memento_mcp::proxy::StdioProxy`]) — zero AppService open
//!      in this process, so the model stays loaded exactly once per
//!      tenant (REQ-DAEMON-001 GIVEN-2).
//!    - Otherwise → direct [`McpServer`] (pre-change behavior; the
//!      fallback also covers hosts without the daemon).
//! 3. Serves the registry over the rmcp stdio transport until the client
//!    closes the pipe.
//!
//! Identity follows REQ-MS-003/REQ-TA-002/003/006: missing or invalid
//! credentials fail fast at startup; nothing is served unauthenticated.

use std::path::PathBuf;
use std::process::ExitCode;

use memento_i18n::Locale;
use memento_mcp::proxy::{ProxyConfig, StdioProxy};
use memento_mcp::{McpServer, StartupOptions};
use rmcp::ServiceExt;
use tracing::warn;

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

    // Carrier selection (REQ-DAEMON-008): the daemon proxy wins when a
    // daemon is reachable; any failure to connect falls back to the
    // direct server (the proxy is an optimization, never a gate — except
    // that MEMENTO_NO_DAEMON=1 short-circuits it, REQ-DAEMON-004 parity).
    if let Some(config) = ProxyConfig::from_env() {
        match StdioProxy::connect(&config).await {
            Ok(proxy) => return serve_proxy(proxy, &opts).await,
            Err(err) => {
                warn!(
                    ?err,
                    "mcp proxy unavailable; falling back to the direct server (double model load on this host)"
                );
            }
        }
    }

    serve_direct(opts).await
}

/// Direct carrier: boot the in-process `McpServer` (opens AppService +
/// embedder — the pre-change path).
async fn serve_direct(opts: StartupOptions) -> ExitCode {
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
        "mcp server starting over stdio (direct)"
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

/// Proxy carrier: thin rmcp client over the daemon's named pipe — this
/// process opens NO AppService (REQ-DAEMON-001 GIVEN-2).
async fn serve_proxy(proxy: StdioProxy, opts: &StartupOptions) -> ExitCode {
    tracing::info!(
        tenant = %proxy.welcome().tenant_id,
        daemon_pid = proxy.welcome().daemon_pid,
        tool_count = 15,
        "mcp server starting over stdio (daemon proxy; no model load in this process)"
    );
    let _ = opts;
    let (stdin, stdout) = rmcp::transport::io::stdio();
    let running = match proxy.serve((stdin, stdout)).await {
        Ok(running) => running,
        Err(err) => {
            eprintln!("memento-mcp-server: stdio handshake failed (proxy): {err}");
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
