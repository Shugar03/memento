//! Sampler wiring integration tests (REQ-OBS-011, design D6).
//!
//! The worker is the ONLY surface that may run the process sampler. These
//! tests pin the env gate (`MEMENTO_OBSERVE_SAMPLES=1`) and the tenant-bound
//! events file through the real worker wiring (`startup::build_sampler`)
//! with fakes for clock and probe — the same injection seam the
//! observability crate uses (sampler.rs tests): no sysinfo, no wall-clock
//! waiting, deterministic.

use memento_domain::TenantId;
use memento_observability::sampler::{Clock, SampleData, SystemProbe};
use memento_worker::startup::build_sampler;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The sampler gate env var (REQ-OBS-011).
const SAMPLES_ENV: &str = "MEMENTO_OBSERVE_SAMPLES";

/// Serializes env mutation across the tests in this binary: tests in one
/// file run in parallel and env is process-global.
static SAMPLER_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Fixed-data probe (fake: no sysinfo involved).
#[derive(Debug, Clone, Copy)]
struct FixedProbe(SampleData);

impl SystemProbe for FixedProbe {
    fn sample(&self) -> SampleData {
        self.0
    }
}

/// Manually-advanced clock (fake: deterministic time).
#[derive(Debug, Clone, Default)]
struct FakeClock(Arc<Mutex<Duration>>);

impl FakeClock {
    fn new(now: Duration) -> Self {
        Self(Arc::new(Mutex::new(now)))
    }

    fn advance(&self, by: Duration) {
        *self.0.lock().expect("clock lock") += by;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        *self.0.lock().expect("clock lock")
    }
}

/// The bound tenant's events file (D5 layout: `<root>/logs/<tid>.events.jsonl`).
fn events_file(root: &Path, tid: &TenantId) -> std::path::PathBuf {
    root.join("logs").join(format!("{tid}.events.jsonl"))
}

fn event_count(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|raw| raw.lines().count())
        .unwrap_or(0)
}

/// One full poll step of the sampler loop. The sampler polls its clock
/// every 100ms (observability sampler.rs `POLL_STEP`); with tokio time
/// paused we advance one poll at a time, then yield so the spawned loop
/// actually runs.
async fn poll_once() {
    tokio::time::advance(Duration::from_millis(100)).await;
    tokio::task::yield_now().await;
}

#[tokio::test(start_paused = true)]
async fn sampler_gate_on_writes_bound_tenant_sample_events() {
    // REQ-OBS-011: MEMENTO_OBSERVE_SAMPLES=1 → the worker wiring builds the
    // sampler; advancing the fake clock past two 100ms intervals produces
    // two `sample` events with rss_bytes and thread_count in the bound
    // tenant's events file (never an agent id — the worker is tenant-bound).
    let _guard = SAMPLER_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: test-only env mutation, serialized by SAMPLER_ENV_LOCK.
    unsafe { std::env::set_var(SAMPLES_ENV, "1") };

    let dir = tempfile::tempdir().expect("tempdir");
    let tid = TenantId::new();
    let clock = FakeClock::new(Duration::ZERO);
    let sampler = build_sampler(
        dir.path(),
        &tid,
        Duration::from_millis(100),
        Arc::new(clock.clone()),
        Arc::new(FixedProbe(SampleData {
            rss_bytes: 4242,
            thread_count: 5,
        })),
    )
    .expect("gate on builds the sampler");

    let task = tokio::spawn(Arc::new(sampler).run());
    let path = events_file(dir.path(), &tid);

    // First poll: now(0) < interval → no sample yet.
    poll_once().await;
    assert_eq!(
        event_count(&path),
        0,
        "no sample before the first interval elapses"
    );

    // Past the first interval → exactly one sample.
    clock.advance(Duration::from_millis(150));
    poll_once().await;
    assert_eq!(event_count(&path), 1, "one sample per interval");

    // Past a second interval → a second sample, same tenant-bound shape.
    clock.advance(Duration::from_millis(200));
    poll_once().await;
    let raw = std::fs::read_to_string(&path).expect("events file");
    assert_eq!(raw.lines().count(), 2, "two samples after two intervals");
    for line in raw.lines() {
        let ev: serde_json::Value = serde_json::from_str(line).expect("JSON line");
        assert_eq!(ev["action"], "sample");
        assert_eq!(ev["tenant_id"], tid.to_string());
        assert_eq!(ev["target"]["rss_bytes"], 4242);
        assert_eq!(ev["target"]["thread_count"], 5);
        assert_eq!(ev["outcome"], "ok");
        assert!(ev["agent_id"].is_null(), "worker events carry no agent id");
    }

    task.abort();
    // SAFETY: test-only env cleanup, serialized by SAMPLER_ENV_LOCK.
    unsafe { std::env::remove_var(SAMPLES_ENV) };
}

#[test]
fn sampler_gate_off_builds_nothing_and_creates_no_file() {
    // REQ-OBS-011: var unset → the worker wiring builds no sampler and the
    // events file is never created (zero I/O while off).
    let _guard = SAMPLER_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: test-only env mutation, serialized by SAMPLER_ENV_LOCK.
    unsafe { std::env::remove_var(SAMPLES_ENV) };

    let dir = tempfile::tempdir().expect("tempdir");
    let tid = TenantId::new();
    let sampler = build_sampler(
        dir.path(),
        &tid,
        Duration::from_secs(30),
        Arc::new(FakeClock::new(Duration::ZERO)),
        Arc::new(FixedProbe(SampleData {
            rss_bytes: 1,
            thread_count: 1,
        })),
    );
    assert!(sampler.is_none(), "unset env builds no sampler");
    assert!(
        !events_file(dir.path(), &tid).exists(),
        "no events file is created while the sampler is off"
    );
}

#[test]
fn sampler_gate_ignores_values_other_than_one() {
    // Triangulation: only the exact value "1" enables the sampler; anything
    // else (including empty) stays off.
    let _guard = SAMPLER_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for value in ["0", "true", "yes", ""] {
        // SAFETY: test-only env mutation, serialized by SAMPLER_ENV_LOCK.
        unsafe { std::env::set_var(SAMPLES_ENV, value) };
        let dir = tempfile::tempdir().expect("tempdir");
        let tid = TenantId::new();
        let sampler = build_sampler(
            dir.path(),
            &tid,
            Duration::from_secs(30),
            Arc::new(FakeClock::new(Duration::ZERO)),
            Arc::new(FixedProbe(SampleData {
                rss_bytes: 1,
                thread_count: 1,
            })),
        );
        assert!(
            sampler.is_none(),
            "value {value:?} must not enable the sampler"
        );
    }
}
