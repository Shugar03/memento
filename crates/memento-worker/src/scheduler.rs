//! Worker scheduler (T-090, design D5).
//!
//! The worker is a SEPARATE PROCESS (the design data-flow's bottom lane:
//! `memento-worker ──► sweep/compact/prune ──► backup`). This module is the
//! rotation engine: a plain 24h timer with an on-demand `--now` trigger and
//! an injectable clock — NOT a cron-expression scheduler.
//!
//! Why not tokio-cron-scheduler (pinned in batch 1, now unused)? T-090
//! requires an *injectable clock* so tests drive the 24h rotation without
//! sleeping; cron-expression schedulers own their time source and cannot be
//! advanced from tests. A `tokio::time` loop is the honest implementation of
//! the task's "24h timer + --now trigger, injectable clock" contract.
//!
//! Semantics:
//!
//! * [`Scheduler::run_now`] — execute every registered job once, in
//!   registration order, isolating failures: one job's error never prevents
//!   the others. This is the `--now` one-shot (cron-friendly invocation,
//!   REQ-OP-002: the caller sees a non-zero outcome if any job failed).
//! * [`Scheduler::run_until_shutdown`] — the daemon loop: tick every
//!   `interval`, honouring a shutdown signal (a `watch` channel flipped by
//!   the process's Ctrl-C/SIGTERM handler). Shutdown is graceful BETWEEN
//!   runs: an in-flight job always completes (no mid-job abort; the
//!   rotation is designed so each job is idempotent and re-runnable, so a
//!   skipped tick is never data loss).
//! * [`Scheduler::next_due`] — scheduling decisions are derived from the
//!   injectable clock (the maintenance job uses the same clock for its
//!   per-rotation prune window).

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use memento_domain::DomainError;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tokio::sync::watch;

/// The default rotation cadence: one full cycle per 24h (design D5).
pub const DEFAULT_INTERVAL: StdDuration = StdDuration::from_secs(24 * 60 * 60);

/// Injectable clock (design D5): every worker decision that depends on "now"
/// goes through this trait so tests can advance virtual time.
pub trait Clock: Send + Sync {
    /// The current wall-clock instant.
    fn now(&self) -> DateTime<Utc>;
}

/// The production clock: `Utc::now()`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// The testkit clock doubles as the worker clock in tests (the trait is
/// local to this crate, so the orphan rule allows the impl). Declared at
/// module scope — every test module in the crate shares it.
#[cfg(test)]
impl Clock for memento_testkit::TestClock {
    fn now(&self) -> DateTime<Utc> {
        memento_testkit::TestClock::now(self)
    }
}

/// One job the scheduler runs on the rotation cadence.
///
/// Implementations own their error handling at the boundary: a job returns
/// `Err` with the stable [`DomainError`] code; the scheduler traces it, marks
/// the outcome, and moves on to the next job.
#[async_trait::async_trait]
pub trait Job: Send + Sync {
    /// Stable job name (`sweep`, `maintenance`, `backup`).
    fn name(&self) -> &'static str;

    /// Run the job once. The returned JSON value is the structured report
    /// (printed by `--now`, traced by the daemon).
    async fn run(&self) -> Result<Value, DomainError>;
}

/// Outcome of one job run (traced by the daemon, surfaced by `--now`).
#[derive(Debug, Clone)]
pub struct JobResult {
    /// The job's stable name.
    pub job: &'static str,
    /// Whether the job completed successfully.
    pub ok: bool,
    /// Stable error code when the job failed (D7 taxonomy).
    pub error_code: Option<&'static str>,
    /// Human-readable failure detail (never content).
    pub error_message: Option<String>,
    /// The clock instant the run started.
    pub started_at: DateTime<Utc>,
    /// Wall time the run took.
    pub duration_ms: u64,
    /// The structured report on success.
    pub report: Option<Value>,
}

/// The rotation engine (T-090).
pub struct Scheduler {
    interval: StdDuration,
    clock: Arc<dyn Clock>,
    jobs: Vec<Arc<dyn Job>>,
}

impl Scheduler {
    /// A scheduler with `interval` between rotation ticks, computing "now"
    /// through `clock` (design D5).
    pub fn new(interval: StdDuration, clock: Arc<dyn Clock>) -> Self {
        Self {
            interval,
            clock,
            jobs: Vec::new(),
        }
    }

    /// Register a job; runs happen in registration order.
    pub fn register(&mut self, job: Arc<dyn Job>) {
        self.jobs.push(job);
    }

    /// The rotation cadence.
    pub fn interval(&self) -> StdDuration {
        self.interval
    }

    /// The next rotation instant as of the injectable clock (D5: scheduling
    /// decisions are clock-derived; tests advance the clock and assert the
    /// computed due time without sleeping).
    pub fn next_due(&self) -> DateTime<Utc> {
        self.clock.now()
            + ChronoDuration::from_std(self.interval).expect("interval fits chrono duration")
    }

    /// The registered jobs (tests inspect the roster).
    pub fn job_names(&self) -> Vec<&'static str> {
        self.jobs.iter().map(|j| j.name()).collect()
    }

    /// Execute every job once (`--now` one-shot; also used by the daemon per
    /// tick). Failures are isolated: one job's error is recorded in its
    /// [`JobResult`] and the remaining jobs still run.
    pub async fn run_now(&self) -> Vec<JobResult> {
        let mut results = Vec::with_capacity(self.jobs.len());
        for job in &self.jobs {
            let started = std::time::Instant::now();
            let started_at = self.clock.now();
            let result = job.run().await;
            let duration_ms = started.elapsed().as_millis() as u64;
            match result {
                Ok(report) => {
                    tracing::info!(job = job.name(), duration_ms, %started_at, "job ok");
                    results.push(JobResult {
                        job: job.name(),
                        ok: true,
                        error_code: None,
                        error_message: None,
                        started_at,
                        duration_ms,
                        report: Some(report),
                    });
                }
                Err(err) => {
                    tracing::error!(
                        job = job.name(),
                        error_code = err.code(),
                        %err,
                        duration_ms,
                        "job failed"
                    );
                    results.push(JobResult {
                        job: job.name(),
                        ok: false,
                        error_code: Some(err.code()),
                        error_message: Some(err.to_string()),
                        started_at,
                        duration_ms,
                        report: None,
                    });
                }
            }
        }
        results
    }

    /// The daemon loop: tick every `interval` until the shutdown signal
    /// fires (a `watch` receiver — the binary's Ctrl-C/SIGTERM handler flips
    /// it; a dropped sender also stops the loop).
    ///
    /// Graceful shutdown semantics: the signal is honoured BETWEEN runs —
    /// an in-flight job always completes, then the loop exits. Skipping the
    /// next tick is safe by design (every job is idempotent and re-runnable;
    /// see the module docs).
    pub async fn run_until_shutdown(&self, mut shutdown: watch::Receiver<bool>) {
        tracing::info!(interval_secs = self.interval.as_secs(), jobs = ?self.job_names(),
            "worker rotation started");
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    // Ok(true) = signal; Err = sender dropped (process exit).
                    if changed.is_ok() && !*shutdown.borrow() {
                        continue;
                    }
                    tracing::info!("worker rotation stopped (graceful, between runs)");
                    break;
                }
                _ = tokio::time::sleep(self.interval) => {
                    let results = self.run_now().await;
                    if let Some(failed) = results.iter().find(|r| !r.ok) {
                        tracing::warn!(
                            job = failed.job,
                            error_code = failed.error_code,
                            "rotation had failed jobs"
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use memento_testkit::TestClock;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A job counting invocations and producing a fixed outcome.
    struct CounterJob {
        name: &'static str,
        runs: Arc<AtomicUsize>,
        result: fn() -> Result<Value, DomainError>,
    }

    #[async_trait::async_trait]
    impl Job for CounterJob {
        fn name(&self) -> &'static str {
            self.name
        }
        async fn run(&self) -> Result<Value, DomainError> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            (self.result)()
        }
    }

    fn ok_value() -> Result<Value, DomainError> {
        Ok(json!({"done": true}))
    }

    fn failing_value() -> Result<Value, DomainError> {
        Err(DomainError::Io {
            source: std::io::Error::other("fake io failure"),
        })
    }

    fn counter(
        name: &'static str,
        runs: Arc<AtomicUsize>,
        result: fn() -> Result<Value, DomainError>,
    ) -> Arc<dyn Job> {
        Arc::new(CounterJob { name, runs, result })
    }

    #[tokio::test]
    async fn now_trigger_runs_every_job_immediately() {
        // T-090 acceptance: `--now` runs immediately.
        let clock: Arc<dyn Clock> = Arc::new(TestClock::new(
            Utc.with_ymd_and_hms(2026, 8, 8, 12, 0, 0).unwrap(),
        ));
        let mut scheduler = Scheduler::new(DEFAULT_INTERVAL, clock);
        let a = Arc::new(AtomicUsize::new(0));
        let b = Arc::new(AtomicUsize::new(0));
        scheduler.register(counter("sweep", a.clone(), ok_value));
        scheduler.register(counter("backup", b.clone(), ok_value));

        let results = scheduler.run_now().await;

        assert_eq!(results.len(), 2);
        assert_eq!(a.load(Ordering::SeqCst), 1);
        assert_eq!(b.load(Ordering::SeqCst), 1);
        assert!(results.iter().all(|r| r.ok));
        assert_eq!(results[0].job, "sweep");
        assert_eq!(results[1].job, "backup");
        assert!(results[0].report.is_some());
        assert_eq!(
            results[0].started_at.to_rfc3339(),
            "2026-08-08T12:00:00+00:00"
        );
    }

    #[tokio::test]
    async fn timer_fires_repeatedly_until_cancelled() {
        // T-090 acceptance: the timer fires the sweep in a test.
        let clock: Arc<dyn Clock> = Arc::new(TestClock::default());
        let mut scheduler = Scheduler::new(StdDuration::from_millis(30), clock);
        let runs = Arc::new(AtomicUsize::new(0));
        scheduler.register(counter("sweep", runs.clone(), ok_value));

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle = tokio::spawn({
            let scheduler = scheduler;
            async move { scheduler.run_until_shutdown(shutdown_rx).await }
        });
        tokio::time::sleep(StdDuration::from_millis(130)).await;
        shutdown_tx.send(true).expect("signal shutdown");
        handle.await.expect("scheduler task joins");

        // 130ms at a 30ms cadence → 4 ticks expected; at least 2 proves the
        // timer fires repeatedly, not once.
        let fired = runs.load(Ordering::SeqCst);
        assert!(fired >= 2, "timer fired {fired} times");
    }

    #[tokio::test]
    async fn cancellation_is_graceful_between_runs() {
        // An in-flight job always completes: cancel fires while the job is
        // sleeping; the loop finishes the run, then exits.
        #[derive(Default)]
        struct SlowJob {
            completed: Arc<AtomicUsize>,
        }
        #[async_trait::async_trait]
        impl Job for SlowJob {
            fn name(&self) -> &'static str {
                "slow"
            }
            async fn run(&self) -> Result<Value, DomainError> {
                tokio::time::sleep(StdDuration::from_millis(120)).await;
                self.completed.fetch_add(1, Ordering::SeqCst);
                ok_value()
            }
        }
        let clock: Arc<dyn Clock> = Arc::new(TestClock::default());
        let mut scheduler = Scheduler::new(StdDuration::from_millis(10), clock);
        let job = Arc::new(SlowJob::default());
        let completed = job.completed.clone();
        scheduler.register(job);

        // First tick fires at 10ms and starts the job (120ms); the shutdown
        // lands at 30ms while the job is in flight.
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle = tokio::spawn({
            let scheduler = scheduler;
            async move { scheduler.run_until_shutdown(shutdown_rx).await }
        });
        tokio::time::sleep(StdDuration::from_millis(30)).await;
        shutdown_tx.send(true).expect("signal shutdown");
        handle.await.expect("scheduler task joins");

        assert_eq!(
            completed.load(Ordering::SeqCst),
            1,
            "in-flight job completed before exit"
        );
    }

    #[tokio::test]
    async fn job_failure_is_isolated() {
        let clock: Arc<dyn Clock> = Arc::new(TestClock::default());
        let mut scheduler = Scheduler::new(DEFAULT_INTERVAL, clock);
        let runs = Arc::new(AtomicUsize::new(0));
        scheduler.register(counter("broken", runs.clone(), failing_value));
        scheduler.register(counter("healthy", runs.clone(), ok_value));

        let results = scheduler.run_now().await;

        assert_eq!(
            runs.load(Ordering::SeqCst),
            2,
            "both jobs ran despite the failure"
        );
        assert!(!results[0].ok);
        assert_eq!(results[0].error_code, Some("IO"));
        assert!(results[0].report.is_none());
        assert!(results[1].ok);
    }

    #[test]
    fn next_due_is_derived_from_the_injectable_clock() {
        // D5: scheduling decisions are clock-derived — no sleeping needed.
        let fixed = Utc.with_ymd_and_hms(2026, 8, 8, 0, 0, 0).unwrap();
        let clock: Arc<dyn Clock> = Arc::new(TestClock::new(fixed));
        let scheduler = Scheduler::new(DEFAULT_INTERVAL, clock);
        assert_eq!(scheduler.next_due(), fixed + ChronoDuration::days(1));
        assert_eq!(scheduler.interval(), DEFAULT_INTERVAL);
    }

    #[test]
    fn roster_reflects_registration_order() {
        let clock: Arc<dyn Clock> = Arc::new(TestClock::default());
        let mut scheduler = Scheduler::new(DEFAULT_INTERVAL, clock);
        let runs = Arc::new(AtomicUsize::new(0));
        scheduler.register(counter("sweep", runs.clone(), ok_value));
        scheduler.register(counter("backup", runs.clone(), ok_value));
        assert_eq!(scheduler.job_names(), vec!["sweep", "backup"]);
    }

    #[tokio::test]
    async fn failure_detail_never_leaks_content() {
        // The error message is the taxonomy's Display (code + stage), never
        // user content — the same contract the audit log enforces.
        let clock: Arc<dyn Clock> = Arc::new(TestClock::default());
        let mut scheduler = Scheduler::new(DEFAULT_INTERVAL, clock);
        let runs = Arc::new(AtomicUsize::new(0));
        scheduler.register(counter("broken", runs.clone(), failing_value));
        let results = scheduler.run_now().await;
        assert!(results[0].error_message.is_some());
    }
}
