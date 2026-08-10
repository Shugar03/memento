//! Worker integration tests (cluster J — T-091/T-092 acceptance).
//!
//! Real LanceDB stores on temp dirs, real jobs, injectable clock:
//!
//! * [`full_rotation_removes_expired_and_reduces_footprint`] — T-091
//!   acceptance: the rotation (sweep → maintenance → backup) removes
//!   expired chunks from search, collapses the version history (footprint
//!   reduced), and the backup captures only the survivors.
//! * [`daemon_tick_runs_the_real_jobs_until_shutdown`] — T-090 acceptance:
//!   the scheduler's timer actually fires the real jobs, and shutdown is
//!   graceful.
//! * `now_one_shot_*` — the `memento-worker --now` binary smoke tests:
//!   cron-friendly one-shot, fail-loudly exit code (REQ-OP-002).
//!
//! The app under test opens like the worker binary does: never-invoked
//! parse boundary, no embedder (absent vectors, REQ-MC-004) — the worker
//! process never downloads models (REQ-CG-004).

use memento_application::AppService;
use memento_domain::{DomainError, SourceKind};
use memento_ports::{IngestTextRequest, ParsePort, ParsedDocument, SearchPort, SearchQuery};
use memento_testkit::{TempStore, TestClock};
use memento_worker::backup_job::BackupJob;
use memento_worker::maintenance::MaintenanceJob;
use memento_worker::scheduler::{Clock, Job, Scheduler};
use memento_worker::sweep::SweepJob;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration as StdDuration;

/// A worker-side clock over the shared `TestClock` (newtype impl — the
/// worker `Clock` trait is foreign to this test crate, the newtype is
/// local, so the orphan rule allows it).
#[derive(Clone)]
struct WorkerClock(TestClock);

impl Clock for WorkerClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        self.0.now()
    }
}

/// An application-side clock over the SAME `TestClock` (application `Clock`
/// trait is foreign, the newtype is local).
struct AppClock(TestClock);

impl memento_application::Clock for AppClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        self.0.now()
    }
}

/// The parse boundary the worker never calls.
struct NoParse;

#[async_trait::async_trait]
impl ParsePort for NoParse {
    async fn parse(&self, _blob: &[u8], _hint: SourceKind) -> Result<ParsedDocument, DomainError> {
        unreachable!("worker jobs never parse")
    }
}

/// Open a worker-style app (no parse, no embedder) over a temp store with a
/// shared test clock.
async fn test_app(ts: &TempStore, clock: TestClock) -> Arc<AppService> {
    let parse: Arc<dyn ParsePort> = Arc::new(NoParse);
    Arc::new(
        AppService::open(&ts.ctx(), ts.root(), parse, None, Arc::new(AppClock(clock)))
            .await
            .expect("worker test app opens"),
    )
}

async fn ingest(app: &AppService, ts: &TempStore, text: &str) {
    app.ingest_text(
        &ts.ctx(),
        IngestTextRequest {
            text: text.to_string(),
            doc_id: None,
            metadata: None,
        },
    )
    .await
    .expect("ingest");
}

/// Total on-disk bytes under `path` (the footprint probe).
fn dir_size(path: &Path) -> u64 {
    fn walk(dir: &Path, total: &mut u64) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    walk(&p, total);
                } else if let Ok(meta) = std::fs::metadata(&p) {
                    *total += meta.len();
                }
            }
        }
    }
    let mut total = 0;
    walk(path, &mut total);
    total
}

async fn search(app: &AppService, ts: &TempStore, term: &str) -> usize {
    app.store()
        .search(&ts.ctx(), SearchQuery::new(term, 10, *ts.workspace_id()))
        .await
        .expect("search")
        .len()
}

#[tokio::test]
async fn full_rotation_removes_expired_and_reduces_footprint() {
    // T-091 acceptance: expired chunks removed from search; footprint
    // reduced (old versions dropped). T-092: the backup job captures only
    // the survivors in a decryptable artifact.
    let ts = TempStore::new();
    let clock = TestClock::new(chrono::Utc::now() - chrono::Duration::days(40));
    let app = test_app(&ts, clock.clone()).await;

    // Two chunks already past the default 30d horizon...
    ingest(&app, &ts, "recuerdo antiguo uno que debe expirar").await;
    ingest(&app, &ts, "recuerdo antiguo dos que debe expirar").await;
    clock.advance(chrono::Duration::days(40));
    // ...and one fresh chunk ingested at "now".
    ingest(&app, &ts, "recuerdo reciente que debe sobrevivir").await;

    // 1. Sweep: hard-delete + lazy compact.
    let sweep = SweepJob::new(app.clone(), ts.ctx());
    let report = sweep.run().await.expect("sweep job");
    assert_eq!(report["expired_count"], 2);
    assert_eq!(report["compacted"], true);
    assert_eq!(
        search(&app, &ts, "antiguo").await,
        0,
        "expired removed from search"
    );

    // 2. Maintenance: prune per rotation → only the current version stays
    //    (footprint reduced — the old data files are unreachable now).
    let footprint_before = dir_size(&ts.lancedb_dir());
    let maintenance = MaintenanceJob::new(
        app.clone(),
        ts.ctx(),
        Arc::new(WorkerClock(clock.clone())),
        StdDuration::from_secs(24 * 3600),
    );
    let report = maintenance.run().await.expect("maintenance job");
    assert_eq!(report["pruned"], true);

    let versions = memento_lancedb::list_versions(app.store(), &ts.ctx())
        .await
        .expect("list versions");
    assert_eq!(versions.len(), 1, "prune collapses the version history");

    let footprint_after = dir_size(&ts.lancedb_dir());
    assert!(
        footprint_after < footprint_before,
        "footprint reduced: {footprint_after} >= {footprint_before}"
    );

    // 3. Backup: only the survivor is captured.
    let backup = BackupJob::new(app.clone(), ts.ctx());
    let report = backup.run().await.expect("backup job");
    assert_eq!(report["chunk_count"], 1);
    assert!(report["backup_dir"].is_string());

    assert_eq!(
        search(&app, &ts, "reciente").await,
        1,
        "fresh chunk survives"
    );
}

#[tokio::test]
async fn daemon_tick_runs_the_real_jobs_until_shutdown() {
    // T-090 acceptance at the integration level: the scheduler's timer
    // fires the REAL jobs (not fakes), and shutdown lands gracefully
    // between runs.
    let ts = TempStore::new();
    let clock = TestClock::new(chrono::Utc::now() - chrono::Duration::days(40));
    let app = test_app(&ts, clock.clone()).await;
    ingest(&app, &ts, "recuerdo que expira durante la rotación").await;
    clock.advance(chrono::Duration::days(40));

    let mut scheduler = Scheduler::new(
        StdDuration::from_millis(30),
        Arc::new(WorkerClock(clock.clone())),
    );
    scheduler.register(Arc::new(SweepJob::new(app.clone(), ts.ctx())));
    scheduler.register(Arc::new(MaintenanceJob::new(
        app.clone(),
        ts.ctx(),
        Arc::new(WorkerClock(clock.clone())),
        StdDuration::from_secs(24 * 3600),
    )));
    scheduler.register(Arc::new(BackupJob::new(app.clone(), ts.ctx())));

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn({
        let scheduler = scheduler;
        async move { scheduler.run_until_shutdown(shutdown_rx).await }
    });

    // Let at least one full rotation happen, then shut down.
    tokio::time::sleep(StdDuration::from_millis(150)).await;
    shutdown_tx.send(true).expect("signal shutdown");
    handle.await.expect("daemon task joins");

    // The daemon's sweep actually removed the expired chunk...
    assert_eq!(
        search(&app, &ts, "expira").await,
        0,
        "expired removed by the daemon tick"
    );
    // ...and the backup artifact exists on disk.
    let backups = ts.root().join("backups").join(ts.tenant_id().to_string());
    let dirs: Vec<_> = std::fs::read_dir(&backups)
        .expect("backups dir")
        .map(|e| e.unwrap().path())
        .collect();
    assert!(!dirs.is_empty(), "daemon ran the backup job");
}

#[test]
fn now_one_shot_runs_every_job_and_exits_zero() {
    // The real binary, real credentials, real store: `--now` is the
    // cron-friendly invocation (REQ-OP-002) — all jobs run once, exit 0.
    let ts = TempStore::new();
    let store = memento_tenant::CredentialStore::new(ts.root());
    let (_, key) = store.create_tenant("smoke").expect("provision tenant");

    assert_cmd::Command::cargo_bin("memento-worker")
        .expect("binary built")
        .env("MEMENTO_TOKEN", key.to_string())
        .env("MEMENTO_AGENT_ID", "smoke-agent")
        .arg("--root")
        .arg(ts.root())
        .arg("--now")
        .assert()
        .success()
        .stdout(predicates::str::contains("sweep: ok"))
        .stdout(predicates::str::contains("maintenance: ok"))
        .stdout(predicates::str::contains("backup: ok"));
}

#[test]
fn now_one_shot_fails_loudly_when_a_job_fails() {
    // REQ-OP-002 fail-loudly: a failing job (corrupt master key → backup
    // cannot wrap the per-backup key) makes `--now` exit non-zero and name
    // the failed job.
    let ts = TempStore::new();
    let store = memento_tenant::CredentialStore::new(ts.root());
    let (tenant_id, key) = store.create_tenant("smoke").expect("provision tenant");

    // Corrupt the master key BEFORE any backup created it: an invalid
    // length makes the backup fail with BACKUP_CORRUPT.
    let master = ts
        .root()
        .join("db")
        .join("tenants")
        .join(tenant_id.to_string())
        .join("keys")
        .join("master.key");
    std::fs::create_dir_all(master.parent().unwrap()).unwrap();
    std::fs::write(&master, b"too-short").unwrap();

    assert_cmd::Command::cargo_bin("memento-worker")
        .expect("binary built")
        .env("MEMENTO_TOKEN", key.to_string())
        .env("MEMENTO_AGENT_ID", "smoke-agent")
        .arg("--root")
        .arg(ts.root())
        .arg("--now")
        .assert()
        .code(1)
        .stdout(predicates::str::contains("backup: FAILED"));
}
