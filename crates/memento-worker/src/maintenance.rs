//! Maintenance job (T-091, design D5): prune per rotation.
//!
//! LanceDB is append-only per version: every write (ingest, delete, compact)
//! creates a new dataset version, and old versions keep their data files on
//! disk. Once per rotation window the maintenance job runs `prune`, which
//! drops the old versions so:
//!
//! * the tenant's on-disk footprint stays bounded (old versions ARE the
//!   footprint — the task acceptance "footprint reduced" is verified by the
//!   version count collapsing to the current version);
//! * deleted rows are no longer reachable through ANY version — the same
//!   posture the erasure chain enforces (REQ-ML-004).
//!
//! The rotation decision is clock-derived ([`Clock`], injectable — D5): the
//! job remembers when it last pruned and skips until the window elapses.
//! State is in-memory on purpose: an ops restarting the daemon nightly gets
//! exactly one prune per restart, and prune is idempotent + cost-bounded per
//! run, so a reset can never cause data loss — only an earlier reclaim.
//!
//! The prune is audited (`prune` event, ids/counts only — REQ-CG-003): the
//! application layer audits sweep/backup; the worker records its own
//! destruction event through the standalone [`AuditLogger`] (the same
//! pattern `restore_backup` uses, since it runs outside any bound service).

use crate::scheduler::{Clock, Job};
use memento_application::AppService;
use memento_application::audit::AuditLogger;
use memento_domain::{DomainError, TenantContext};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

/// The rotation window's tables (mirrors `memento_lancedb::ALL_TABLES`).
const TABLES: [&str; 4] = ["chunks", "docs", "feedback", "symbols"];

/// The maintenance job: prune once per rotation window (T-091).
pub struct MaintenanceJob {
    app: Arc<AppService>,
    ctx: TenantContext,
    clock: Arc<dyn Clock>,
    rotation: StdDuration,
    last_prune: Mutex<Option<chrono::DateTime<chrono::Utc>>>,
}

impl std::fmt::Debug for MaintenanceJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MaintenanceJob")
            .field("rotation_secs", &self.rotation.as_secs())
            .field("last_prune", &self.last_prune.lock().expect("poisoned"))
            .finish()
    }
}

impl MaintenanceJob {
    /// A prune-per-rotation job over `app`, deciding "due" through `clock`
    /// with window `rotation` (default: the scheduler's 24h interval).
    pub fn new(
        app: Arc<AppService>,
        ctx: TenantContext,
        clock: Arc<dyn Clock>,
        rotation: StdDuration,
    ) -> Self {
        Self {
            app,
            ctx,
            clock,
            rotation,
            last_prune: Mutex::new(None),
        }
    }

    /// The rotation window.
    pub fn rotation(&self) -> StdDuration {
        self.rotation
    }

    /// Whether the rotation window has elapsed since the last prune.
    fn is_due(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        match *self.last_prune.lock().expect("maintenance lock poisoned") {
            None => true,
            Some(last) => {
                now - last
                    >= chrono::Duration::from_std(self.rotation)
                        .expect("rotation fits chrono duration")
            }
        }
    }
}

#[async_trait::async_trait]
impl Job for MaintenanceJob {
    fn name(&self) -> &'static str {
        "maintenance"
    }

    async fn run(&self) -> Result<Value, DomainError> {
        let now = self.clock.now();
        if !self.is_due(now) {
            let last = self
                .last_prune
                .lock()
                .expect("maintenance lock poisoned")
                .expect("not_due implies a previous prune");
            let next_prune =
                last + chrono::Duration::from_std(self.rotation).expect("rotation fits");
            return Ok(json!({
                "pruned": false,
                "reason": "not_due",
                "next_prune_at": next_prune.to_rfc3339(),
            }));
        }

        memento_lancedb::prune(self.app.store(), &self.ctx).await?;
        *self.last_prune.lock().expect("maintenance lock poisoned") = Some(now);

        if let Ok(logger) = AuditLogger::new(self.app.root(), self.ctx.tenant_id()) {
            logger.ok(
                &self.ctx,
                "prune",
                json!({
                    "tables": TABLES,
                    "rotation_secs": self.rotation.as_secs(),
                }),
                None,
            );
        }

        Ok(json!({
            "pruned": true,
            "at": now.to_rfc3339(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memento_ports::{IngestTextRequest, SearchPort, SearchQuery};
    use memento_testkit::{TempStore, TestClock};
    use std::sync::Arc;

    struct AppClock(TestClock);

    impl memento_application::Clock for AppClock {
        fn now(&self) -> chrono::DateTime<chrono::Utc> {
            self.0.now()
        }
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

    async fn test_app(ts: &TempStore, clock: TestClock) -> Arc<AppService> {
        let parse: Arc<dyn memento_ports::ParsePort> = Arc::new(NoParse);
        Arc::new(
            AppService::open(&ts.ctx(), ts.root(), parse, None, Arc::new(AppClock(clock)))
                .await
                .expect("worker test app opens"),
        )
    }

    const ROTATION: StdDuration = StdDuration::from_secs(24 * 60 * 60);

    #[tokio::test]
    async fn prune_runs_once_per_rotation_window() {
        // T-091: prune per rotation — the window is clock-derived, so the
        // test advances virtual time instead of sleeping.
        let ts = TempStore::new();
        let clock = TestClock::default();
        let app = test_app(&ts, clock.clone()).await;
        let job = MaintenanceJob::new(app, ts.ctx(), Arc::new(clock.clone()), ROTATION);

        let first = job.run().await.expect("first run");
        assert_eq!(first["pruned"], true, "first run always prunes");

        let second = job.run().await.expect("second run");
        assert_eq!(second["pruned"], false, "window not elapsed → skip");
        assert_eq!(second["reason"], "not_due");

        clock.advance(chrono::Duration::days(1));
        let third = job.run().await.expect("third run");
        assert_eq!(third["pruned"], true, "window elapsed → prune again");

        clock.advance(chrono::Duration::hours(1));
        let fourth = job.run().await.expect("fourth run");
        assert_eq!(fourth["pruned"], false, "1h after prune → skip");
    }

    #[tokio::test]
    async fn prune_drops_old_versions_making_deleted_rows_unreachable() {
        // REQ-ML-004 posture: after the maintenance prune, only the current
        // version remains — the deleted chunk is unrecoverable (footprint
        // reduced = old versions gone).
        let ts = TempStore::new();
        let clock = TestClock::default();
        let app = test_app(&ts, clock.clone()).await;

        for text in ["primera memoria", "segunda memoria"] {
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

        // Delete one chunk directly at the store level (versions grow).
        let hits = app
            .store()
            .search(
                &ts.ctx(),
                SearchQuery::new("primera", 10, *ts.workspace_id()),
            )
            .await
            .expect("search");
        assert_eq!(hits.len(), 1);
        memento_lancedb::delete_chunks(app.store(), &ts.ctx(), &[hits[0].chunk_id])
            .await
            .expect("delete chunk");

        let versions_before = memento_lancedb::list_versions(app.store(), &ts.ctx())
            .await
            .expect("list versions");
        assert!(
            versions_before.len() > 1,
            "ingest + delete left multiple versions: {}",
            versions_before.len()
        );

        let job = MaintenanceJob::new(app.clone(), ts.ctx(), Arc::new(clock), ROTATION);
        let report = job.run().await.expect("maintenance run");
        assert_eq!(report["pruned"], true);

        let versions_after = memento_lancedb::list_versions(app.store(), &ts.ctx())
            .await
            .expect("list versions");
        assert_eq!(
            versions_after.len(),
            1,
            "prune keeps only the current version — footprint reduced"
        );

        // And the surviving chunk is still searchable.
        let hits = app
            .store()
            .search(
                &ts.ctx(),
                SearchQuery::new("segunda", 10, *ts.workspace_id()),
            )
            .await
            .expect("search");
        assert_eq!(hits.len(), 1);
    }
}
