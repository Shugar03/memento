//! memento-worker binary (T-090: the worker is a separate process).
//!
//! Two modes:
//!
//! ```text
//! memento-worker                      # daemon: full rotation every 24h
//! memento-worker --now                # one-shot: run every job once, exit
//! ```
//!
//! * `--now` is the cron-friendly invocation (REQ-OP-002): a nightly crontab
//!   entry runs the whole rotation and fails loudly (exit 1) if any job
//!   failed.
//! * The daemon ticks every `--interval-hours` (design D5: 24h) and shuts
//!   down gracefully on Ctrl-C / SIGTERM — between runs, never mid-job.
//!
//! Help and report strings are English: the worker is an ops process, not a
//! user surface. The bilingual contract (REQ-CL-004 / REQ-MS-004) covers the
//! `memento` CLI and the MCP tools; the worker's audience is the operator's
//! cron tab, and its reports are structured JSON for machine consumers.

use anyhow::Context;
use clap::Parser;
use memento_domain::DomainError;
use memento_worker::backup_job::BackupJob;
use memento_worker::maintenance::MaintenanceJob;
use memento_worker::scheduler::{Clock, Scheduler, SystemClock};
use memento_worker::startup::{WorkerContext, open};
use memento_worker::sweep::SweepJob;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tokio::sync::watch;

/// Worker arguments (ops surface; see module docs).
#[derive(Debug, Parser)]
#[command(name = "memento-worker", about = "Memento RS background worker")]
struct Args {
    /// Run every job once and exit (cron-friendly one-shot).
    #[arg(long)]
    now: bool,

    /// Rotation cadence in hours (design D5: 24h default).
    #[arg(long, default_value_t = 24)]
    interval_hours: u64,

    /// Storage root (default: $MEMENTO_ROOT or ~/.memento).
    #[arg(long)]
    root: Option<PathBuf>,
}

/// Storage root resolution: `--root` > `MEMENTO_ROOT` env > `~/.memento`
/// (D8 layout, same precedence as the `memento` CLI).
fn resolve_root(root: Option<PathBuf>) -> Result<PathBuf, DomainError> {
    if let Some(root) = root {
        return Ok(root);
    }
    if let Some(root) = std::env::var_os("MEMENTO_ROOT") {
        return Ok(PathBuf::from(root));
    }
    memento_tenant::default_root()
}

/// The full worker rotation: sweep (with lazy compact), maintenance
/// (prune per rotation window), backup (per-backup encrypted artifact).
fn build_scheduler(ctx: &WorkerContext, interval: StdDuration) -> Scheduler {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let mut scheduler = Scheduler::new(interval, clock.clone());
    scheduler.register(Arc::new(SweepJob::new(ctx.app.clone(), ctx.ctx.clone())));
    scheduler.register(Arc::new(MaintenanceJob::new(
        ctx.app.clone(),
        ctx.ctx.clone(),
        clock,
        interval,
    )));
    scheduler.register(Arc::new(BackupJob::new(ctx.app.clone(), ctx.ctx.clone())));
    scheduler
}

/// Print one `--now` job result (machine-friendly single line per job).
fn print_result(result: &memento_worker::scheduler::JobResult) {
    if result.ok {
        println!(
            "{}: ok ({}ms) {}",
            result.job,
            result.duration_ms,
            result
                .report
                .as_ref()
                .map(|r| r.to_string())
                .unwrap_or_default()
        );
    } else {
        println!(
            "{}: FAILED ({}) in {}ms — {}",
            result.job,
            result.error_code.unwrap_or("?"),
            result.duration_ms,
            result.error_message.as_deref().unwrap_or("unknown error")
        );
    }
}

/// Wait for Ctrl-C or SIGTERM, then flip the shutdown signal.
async fn shutdown_signal(shutdown: watch::Sender<bool>) {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {},
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
    tracing::info!("shutdown signal received");
    let _ = shutdown.send(true);
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let root = resolve_root(args.root).context("resolve storage root")?;
    let interval = StdDuration::from_secs(args.interval_hours * 3600);

    let ctx = open(&root).await.context("open worker context")?;
    let scheduler = build_scheduler(&ctx, interval);

    if args.now {
        let results = scheduler.run_now().await;
        for result in &results {
            print_result(result);
        }
        if results.iter().any(|r| !r.ok) {
            tracing::error!("one or more jobs failed; exiting non-zero");
            std::process::exit(1);
        }
        return Ok(());
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let _signal = tokio::spawn(shutdown_signal(shutdown_tx));
    scheduler.run_until_shutdown(shutdown_rx).await;
    Ok(())
}
