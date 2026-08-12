//! Gated process sampler (REQ-OBS-011, design D6).
//!
//! [`Sampler`] samples RSS bytes and thread count through a [`SystemProbe`]
//! every [`Sampler::DEFAULT_INTERVAL`] (30s) and appends one `sample` event
//! line to the bound tenant's events file (REQ-OBS-008/011). Spawned only by
//! the worker under `MEMENTO_OBSERVE_SAMPLES=1` (wired in slice S4); never
//! runs in CLI/MCP processes or any hot path.
//!
//! Both the interval clock ([`Clock`]) and the probe ([`SystemProbe`]) are
//! injectable traits: the real impls (`SystemClock`, `SysinfoProbe`) isolate
//! `sysinfo` API churn, and tests inject fakes (see `tests` below). On
//! Windows, RSS is the working set including shared pages — a trend, not an
//! absolute (design D6 note).

use crate::events::{EventRecord, EventSink};
use memento_domain::DomainError;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// One process sample: RSS bytes and thread count (REQ-OBS-011).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleData {
    pub rss_bytes: u64,
    pub thread_count: u32,
}

/// Source of process telemetry. The real impl wraps sysinfo; tests inject
/// fixed-data fakes.
pub trait SystemProbe: Send + Sync {
    fn sample(&self) -> SampleData;
}

/// Time source for the sample interval. The real impl reads the system
/// clock; tests inject a fake they advance manually.
pub trait Clock: Send + Sync {
    fn now(&self) -> Duration;
}

/// System-clock impl (epoch offset; only deltas are used).
#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
    }
}

/// Real probe over sysinfo (pure Rust; see module docs for the Windows RSS
/// caveat).
#[derive(Debug, Default)]
pub struct SysinfoProbe;

impl SystemProbe for SysinfoProbe {
    fn sample(&self) -> SampleData {
        let mut system = sysinfo::System::new();
        system.refresh_memory();
        let pid = sysinfo::Pid::from_u32(std::process::id());
        // everything(): the plain refresh_processes does not populate
        // Process::tasks() nor refresh memory (sysinfo 0.39).
        system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[pid]),
            true,
            sysinfo::ProcessRefreshKind::everything(),
        );
        SampleData {
            rss_bytes: system.process(pid).map(|p| p.memory()).unwrap_or(0),
            thread_count: process_thread_count(&system, pid),
        }
    }
}

/// Thread count for `pid`.
///
/// sysinfo 0.39 only populates `Process::tasks()` on linux/android — on
/// every other target it is cfg'd to always return `None` (sysinfo 0.39.6
/// `common/system.rs`). The [`SystemProbe`] trait isolates that churn
/// (design D6): linux reads tasks from the already-refreshed system;
/// Windows counts threads with the same Toolhelp snapshot API sysinfo uses
/// for processes (windows-sys, already in the lock via sysinfo).
#[cfg(target_os = "linux")]
fn process_thread_count(system: &sysinfo::System, pid: sysinfo::Pid) -> u32 {
    system
        .process(pid)
        .and_then(|p| p.tasks())
        .map(|tasks| tasks.len() as u32)
        .unwrap_or(0)
}

#[cfg(windows)]
fn process_thread_count(_system: &sysinfo::System, pid: sysinfo::Pid) -> u32 {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };

    // SAFETY: Toolhelp snapshot API. The handle is closed on every path and
    // the entry is zero-initialized with dwSize set as documented.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return 0;
        }
        let mut entry: THREADENTRY32 = zeroed();
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        let mut count = 0u32;
        if Thread32First(snapshot, &mut entry) != 0 {
            loop {
                if entry.th32OwnerProcessID == pid.as_u32() {
                    count += 1;
                }
                if Thread32Next(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
        count
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn process_thread_count(_system: &sysinfo::System, _pid: sysinfo::Pid) -> u32 {
    // sysinfo tasks() is linux-only and there is no other portable source:
    // report 0 rather than guessing (best-effort contract).
    0
}

/// Gated process sampler (REQ-OBS-011, design D6): every `interval` (default
/// 30s) it samples through the probe and appends an event line to the bound
/// tenant's events file.
pub struct Sampler {
    interval: Duration,
    clock: Arc<dyn Clock>,
    probe: Arc<dyn SystemProbe>,
    sink: EventSink,
}

impl Sampler {
    /// Default sample interval (REQ-OBS-011: every 30s).
    pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(30);
    /// How often the loop re-checks the clock: keeps fake-clock tests fast
    /// and the real loop responsive without busy-spinning.
    const POLL_STEP: Duration = Duration::from_millis(100);

    pub fn new(
        interval: Duration,
        clock: Arc<dyn Clock>,
        probe: Arc<dyn SystemProbe>,
        sink: EventSink,
    ) -> Self {
        Self {
            interval,
            clock,
            probe,
            sink,
        }
    }

    /// Sample right now and append the event line. Best-effort: the sink
    /// never propagates write failures, so this is infallible in practice
    /// (signature keeps the Result for future probe errors).
    pub async fn sample_now(&self) -> Result<(), DomainError> {
        let data = self.probe.sample();
        let event = EventRecord {
            ts: chrono::Utc::now(),
            tenant_id: self.sink.tenant_id(),
            agent_id: None, // worker is tenant-bound, not agent-bound
            action: "sample".to_string(),
            target: json!({
                "rss_bytes": data.rss_bytes,
                "thread_count": data.thread_count,
            }),
            outcome: "ok",
            error_code: None,
            chore_id: None,
        };
        self.sink.record(&event);
        Ok(())
    }

    /// Run forever: every `interval` (per the clock) take one sample.
    /// Cancellation-safe — aborting the task just stops the loop. Takes
    /// `Arc<Self>` so the caller can `tokio::spawn(handle.clone().run())`.
    pub async fn run(self: Arc<Self>) {
        let mut next_due = self.clock.now() + self.interval;
        loop {
            tokio::time::sleep(Self::POLL_STEP).await;
            if self.clock.now() >= next_due {
                if let Err(err) = self.sample_now().await {
                    tracing::warn!(%err, "sampler tick failed");
                }
                next_due = self.clock.now() + self.interval;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::events::EventSink;
    use memento_domain::TenantId;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::{Clock, Sampler, SampleData, SystemClock, SystemProbe, SysinfoProbe};

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

    fn event_count(path: &Path) -> usize {
        std::fs::read_to_string(path)
            .map(|raw| raw.lines().count())
            .unwrap_or(0)
    }

    fn read_first_event(path: &Path) -> serde_json::Value {
        let raw = std::fs::read_to_string(path).expect("events file");
        serde_json::from_str(raw.lines().next().expect("one line")).expect("JSON line")
    }

    #[test]
    fn fixed_probe_reports_its_sample() {
        // The probe abstraction hands a fixed sample up unchanged.
        let probe = FixedProbe(SampleData { rss_bytes: 1234, thread_count: 7 });
        assert_eq!(
            probe.sample(),
            SampleData { rss_bytes: 1234, thread_count: 7 }
        );
    }

    #[test]
    fn fake_clock_advances_manually() {
        // The clock abstraction is a time source the tests control.
        let clock = FakeClock::new(Duration::from_secs(5));
        assert_eq!(clock.now(), Duration::from_secs(5));
        clock.advance(Duration::from_secs(2));
        assert_eq!(clock.now(), Duration::from_secs(7));
    }

    #[test]
    fn system_clock_now_is_positive() {
        // Real clock impl sanity: epoch offset is a positive duration.
        assert!(SystemClock.now() > Duration::ZERO);
    }

    #[test]
    fn sysinfo_probe_reports_live_process() {
        // Real probe over sysinfo: our own process must report RSS > 0 and
        // at least one thread (REQ-OBS-011 fields).
        let data = SysinfoProbe.sample();
        assert!(data.rss_bytes > 0, "live process has RSS: {data:?}");
        assert!(data.thread_count >= 1, "live process has threads: {data:?}");
    }

    #[tokio::test]
    async fn sample_now_appends_event_with_probe_data() {
        // REQ-OBS-011: one sample event with rss_bytes + thread_count lands
        // in the tenant events file; no agent id (worker is tenant-bound).
        let dir = tempfile::tempdir().expect("tempdir");
        let tid = TenantId::new();
        let sink = EventSink::tenant(dir.path(), &tid).expect("sink");
        let path = sink.log_path();
        let sampler = Sampler::new(
            Duration::from_secs(30),
            Arc::new(FakeClock::new(Duration::ZERO)),
            Arc::new(FixedProbe(SampleData { rss_bytes: 9876, thread_count: 3 })),
            sink,
        );

        sampler.sample_now().await.expect("sample_now never fails");

        let line = read_first_event(&path);
        assert_eq!(line["action"], "sample");
        assert_eq!(line["target"]["rss_bytes"], 9876);
        assert_eq!(line["target"]["thread_count"], 3);
        assert_eq!(line["tenant_id"], tid.to_string());
        assert_eq!(line["outcome"], "ok");
        assert!(line["agent_id"].is_null(), "no agent id faked for worker events");
    }

    #[tokio::test]
    async fn sample_now_tolerates_zeroed_probe() {
        // Triangulation: a probe that found no process data (0/0) still
        // writes the event line instead of failing the caller.
        let dir = tempfile::tempdir().expect("tempdir");
        let tid = TenantId::new();
        let sink = EventSink::tenant(dir.path(), &tid).expect("sink");
        let path = sink.log_path();
        let sampler = Sampler::new(
            Duration::from_secs(30),
            Arc::new(FakeClock::new(Duration::ZERO)),
            Arc::new(FixedProbe(SampleData { rss_bytes: 0, thread_count: 0 })),
            sink,
        );

        sampler.sample_now().await.expect("sample_now never fails");

        let line = read_first_event(&path);
        assert_eq!(line["target"]["rss_bytes"], 0);
        assert_eq!(line["target"]["thread_count"], 0);
    }

    #[tokio::test(start_paused = true)]
    async fn run_samples_once_per_interval_per_fake_clock() {
        // REQ-OBS-011: the loop samples every `interval`. The fake clock is
        // advanced manually; tokio time is paused so the poll sleep only
        // progresses when the test advances it — deterministic, no waiting.
        let dir = tempfile::tempdir().expect("tempdir");
        let tid = TenantId::new();
        let sink = EventSink::tenant(dir.path(), &tid).expect("sink");
        let path = sink.log_path();
        let clock = FakeClock::new(Duration::ZERO);
        let sampler = Arc::new(Sampler::new(
            Duration::from_millis(100),
            Arc::new(clock.clone()),
            Arc::new(FixedProbe(SampleData { rss_bytes: 1, thread_count: 1 })),
            sink,
        ));

        let task = tokio::spawn(sampler.clone().run());

        // First poll: now(0) < interval → no sample yet.
        tokio::time::advance(Sampler::POLL_STEP).await;
        tokio::task::yield_now().await;
        assert_eq!(event_count(&path), 0, "no sample before the first interval elapses");

        // Advance past one interval → exactly one sample.
        clock.advance(Duration::from_millis(150));
        tokio::time::advance(Sampler::POLL_STEP).await;
        tokio::task::yield_now().await;
        assert_eq!(event_count(&path), 1, "one sample per interval");

        // Advance past a second interval → a second sample.
        clock.advance(Duration::from_millis(200));
        tokio::time::advance(Sampler::POLL_STEP).await;
        tokio::task::yield_now().await;
        assert_eq!(event_count(&path), 2, "second sample after the second interval");

        task.abort();
    }
}
