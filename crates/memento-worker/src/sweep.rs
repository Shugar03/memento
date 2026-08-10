//! Retention sweep job (T-091, design D5, REQ-ML-003).
//!
//! Per rotation the job:
//!
//! 1. hard-deletes every chunk older than the tenant's retention horizon
//!    (`AppService::retention_sweep` — cutoff computed from the app's
//!    injectable clock, per-tenant override honoured, opt-out supported);
//! 2. compacts the store ONCE per sweep, LAZILY — only when the sweep
//!    actually deleted rows (D5 "lazy compact": a no-deletion sweep changed
//!    nothing, so rewriting the tables would be wasted I/O; a deletion sweep
//!    gets the purge chain started so freed space is reclaimed).
//!
//! The sweep itself is audited by the application layer (`sweep` event with
//! retention/cutoff/counts — REQ-CG-003); the compact is a consequence of
//! the sweep and is reported in the job's structured outcome.

use crate::scheduler::Job;
use memento_application::AppService;
use memento_domain::{DomainError, TenantContext};
use serde_json::{Value, json};
use std::sync::Arc;

/// The sweep job: `retention_sweep` + lazy compact (T-091).
pub struct SweepJob {
    app: Arc<AppService>,
    ctx: TenantContext,
}

impl SweepJob {
    /// A sweep job for the bound tenant's app.
    pub fn new(app: Arc<AppService>, ctx: TenantContext) -> Self {
        Self { app, ctx }
    }
}

#[async_trait::async_trait]
impl Job for SweepJob {
    fn name(&self) -> &'static str {
        "sweep"
    }

    async fn run(&self) -> Result<Value, DomainError> {
        let report = self.app.retention_sweep(&self.ctx).await?;
        let compacted = if report.expired_count > 0 {
            memento_lancedb::compact(self.app.store(), &self.ctx).await?;
            true
        } else {
            false
        };
        Ok(json!({
            "expired_count": report.expired_count,
            "freed_bytes": report.freed_bytes,
            "compacted": compacted,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memento_ports::{IngestTextRequest, SearchPort, SearchQuery};
    use memento_testkit::{TempStore, TestClock};
    use std::sync::Arc;

    /// A worker-side app clock over the shared `TestClock` (newtype impl —
    /// the application `Clock` trait is foreign, the newtype is local).
    struct AppClock(TestClock);

    impl memento_application::Clock for AppClock {
        fn now(&self) -> chrono::DateTime<chrono::Utc> {
            self.0.now()
        }
    }

    /// Open a worker-style app (never-invoked parse, no embedder — REQ-MC-004
    /// absent-vector path) over a temp store with a shared test clock.
    async fn test_app(ts: &TempStore, clock: TestClock) -> Arc<AppService> {
        let parse: Arc<dyn memento_ports::ParsePort> = Arc::new(NoParse);
        Arc::new(
            AppService::open(&ts.ctx(), ts.root(), parse, None, Arc::new(AppClock(clock)))
                .await
                .expect("worker test app opens"),
        )
    }

    struct NoParse;

    #[async_trait::async_trait]
    impl memento_ports::ParsePort for NoParse {
        async fn parse(
            &self,
            _blob: &[u8],
            _hint: memento_domain::SourceKind,
        ) -> Result<memento_ports::ParsedDocument, DomainError> {
            unreachable!("worker jobs never parse")
        }
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

    fn clock_at(days_ago: i64) -> TestClock {
        TestClock::new(chrono::Utc::now() - chrono::Duration::days(days_ago))
    }

    #[tokio::test]
    async fn sweep_job_removes_expired_chunks_and_compacts() {
        // T-091 acceptance (part 1): expired chunks are removed.
        // Ingest at T-40d (already past the default 30d horizon), then run
        // the job with the clock at T.
        let ts = TempStore::new();
        let clock = clock_at(40);
        let app = test_app(&ts, clock.clone()).await;
        ingest(&app, &ts, "recuerdo antiguo que debe expirar en el barrido").await;
        clock.advance(chrono::Duration::days(40));

        let job = SweepJob::new(app.clone(), ts.ctx());
        let report = job.run().await.expect("sweep job runs");

        assert_eq!(report["expired_count"], 1);
        assert_eq!(
            report["compacted"], true,
            "lazy compact runs after deletions"
        );

        let hits = app
            .store()
            .search(
                &ts.ctx(),
                SearchQuery::new("antiguo", 10, *ts.workspace_id()),
            )
            .await
            .expect("search");
        assert!(hits.is_empty(), "expired chunk no longer searchable");
    }

    #[tokio::test]
    async fn sweep_job_skips_compact_when_nothing_expired() {
        // D5 lazy compact: a no-deletion sweep does not rewrite the tables.
        let ts = TempStore::new();
        let app = test_app(&ts, TestClock::default()).await;
        ingest(&app, &ts, "recuerdo reciente que debe sobrevivir").await;

        let job = SweepJob::new(app, ts.ctx());
        let report = job.run().await.expect("sweep job runs");

        assert_eq!(report["expired_count"], 0);
        assert_eq!(report["compacted"], false, "no deletions → no compact");
    }

    #[tokio::test]
    async fn sweep_job_honours_opt_out() {
        // REQ-ML-003: retention 0 disables the sweep regardless of age.
        let ts = TempStore::new();
        let clock = clock_at(200);
        let app = test_app(&ts, clock).await;
        ingest(&app, &ts, "recuerdo muy antiguo pero protegido").await;
        app.set_retention_days(&ts.ctx(), 0).await.expect("opt-out");

        let job = SweepJob::new(app.clone(), ts.ctx());
        let report = job.run().await.expect("sweep job runs");

        assert_eq!(report["expired_count"], 0);
        assert_eq!(report["compacted"], false);
        assert_eq!(app.store().count_chunks(&ts.ctx()).await.unwrap(), 1);
    }
}
