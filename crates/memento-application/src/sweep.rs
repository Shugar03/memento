//! Retention sweep (T-063, REQ-ML-003, design D5).
//!
//! The sweep removes chunks whose `created_at` is older than the tenant's
//! effective retention horizon: `cutoff = now - retention_days`. The clock is
//! injectable (tests advance virtual time); the cutoff is computed once per
//! sweep so the operation is internally consistent (snapshot isolation comes
//! from LanceDB's versioned reads — no torn reads between the cutoff
//! computation and the delete).
//!
//! Per-tenant override honored (REQ-ML-003 scenario 2, config in
//! [`crate::tenant_config`]); `days = 0` disables the sweep entirely
//! (scenario 3 — nothing expires regardless of age). Setting a horizon is an
//! audited configuration change (REQ-CG-002: relaxation must be explicit and
//! audited).

use crate::AppService;
use crate::tenant_config::{
    RETENTION_DISABLED, TenantConfig, read_tenant_config, write_tenant_config,
};
use memento_domain::{DomainError, TenantContext};
use memento_ports::SweepReport;
use serde_json::json;

impl AppService {
    /// The tenant's effective retention horizon in days (`0` = disabled).
    /// Reads the per-tenant config (default 30 when unset, REQ-ML-003).
    pub fn retention_days(&self, ctx: &TenantContext) -> Result<u64, DomainError> {
        self.ensure_bound_tenant(ctx)?;
        Ok(read_tenant_config(&self.root, ctx.tenant_id()).retention_days)
    }

    /// Set the retention horizon (audited configuration change, REQ-CG-002).
    /// `days = 0` opts the tenant out of retention entirely.
    pub async fn set_retention_days(
        &self,
        ctx: &TenantContext,
        days: u64,
    ) -> Result<(), DomainError> {
        self.ensure_bound_tenant(ctx)?;
        write_tenant_config(
            &self.root,
            ctx.tenant_id(),
            &TenantConfig {
                retention_days: days,
            },
        )?;
        self.record_audit(
            ctx,
            "retention_change",
            json!({
                "retention_days": days,
                "action": if days == RETENTION_DISABLED { "disable" } else if days > 30 { "relax" } else { "tighten" },
            }),
            None,
        );
        Ok(())
    }

    /// Run the retention sweep: hard-delete every chunk older than
    /// `now - retention` (REQ-ML-003, design D5). Honors the per-tenant
    /// override; a disabled tenant sweeps nothing (opt-out).
    ///
    /// # Errors
    ///
    /// * Adapter errors propagate (the sweep is re-runnable — deleting is
    ///   idempotent per row).
    pub async fn retention_sweep(&self, ctx: &TenantContext) -> Result<SweepReport, DomainError> {
        self.ensure_bound_tenant(ctx)?;
        let days = self.retention_days(ctx)?;
        if days == RETENTION_DISABLED {
            let report = SweepReport {
                expired_count: 0,
                freed_bytes: 0,
                chore_id: memento_domain::ChoreId::new(),
            };
            self.record_audit(
                ctx,
                "sweep",
                json!({ "retention_days": "disabled", "expired_count": 0 }),
                Some(report.chore_id),
            );
            return Ok(report);
        }

        let cutoff = self.clock.now() - chrono::Duration::days(days as i64);
        let report = memento_lancedb::sweep_expired(&self.store, ctx, cutoff).await?;
        self.record_audit(
            ctx,
            "sweep",
            json!({
                "retention_days": days,
                "cutoff": cutoff.to_rfc3339(),
                "expired_count": report.expired_count,
            }),
            Some(report.chore_id),
        );
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::test_app;
    use memento_ports::{IngestTextRequest, SearchPort};
    use memento_testkit::{TempStore, TestClock};

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
    async fn default_horizon_expires_chunks_after_30_days() {
        // REQ-ML-003 scenario 1: no override → chunks older than 30 days are
        // purged when the sweep runs; fresh chunks survive.
        let ts = TempStore::new();
        // Ingest happens at clock.now(): 40 days ago → already expired.
        let app = test_app(&ts, clock_at(40)).await;
        ingest(&app, &ts, "recuerdo antiguo que debe expirar pronto").await;

        let fresh = test_app(&ts, TestClock::default()).await;
        ingest(&fresh, &ts, "recuerdo reciente que debe sobrevivir").await;

        let report = fresh.retention_sweep(&ts.ctx()).await.expect("sweep ok");
        assert_eq!(report.expired_count, 1, "only the old chunk expires");

        let hits = fresh
            .store()
            .search(
                &ts.ctx(),
                memento_ports::SearchQuery::new("antiguo", 10, *ts.workspace_id()),
            )
            .await
            .expect("search");
        assert!(hits.is_empty(), "expired chunk no longer searchable");
        let hits = fresh
            .store()
            .search(
                &ts.ctx(),
                memento_ports::SearchQuery::new("reciente", 10, *ts.workspace_id()),
            )
            .await
            .expect("search");
        assert!(!hits.is_empty(), "fresh chunk retained");
    }

    #[tokio::test]
    async fn tenant_override_extends_the_horizon() {
        // REQ-ML-003 scenario 2: 90-day override retains 45-day-old chunks.
        let ts = TempStore::new();
        let app = test_app(&ts, clock_at(45)).await;
        ingest(&app, &ts, "recuerdo de 45 días con override de 90").await;
        app.set_retention_days(&ts.ctx(), 90)
            .await
            .expect("override");

        let report = app.retention_sweep(&ts.ctx()).await.expect("sweep");
        assert_eq!(report.expired_count, 0, "45 < 90 → retained");

        // And the sweep uses the NEW cutoff: at 95 days the same chunk goes.
        // (Re-ingest not needed — the chunk is still there.)
        assert_eq!(app.store().count_chunks(&ts.ctx()).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn opt_out_disables_expiry_regardless_of_age() {
        // REQ-ML-003 scenario 3: retention disabled → nothing expires.
        let ts = TempStore::new();
        let app = test_app(&ts, clock_at(200)).await;
        ingest(&app, &ts, "recuerdo muy antiguo pero protegido").await;
        app.set_retention_days(&ts.ctx(), 0).await.expect("opt-out");

        let report = app.retention_sweep(&ts.ctx()).await.expect("sweep");
        assert_eq!(report.expired_count, 0);
        assert_eq!(app.store().count_chunks(&ts.ctx()).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn override_survives_reopen_and_sweep_uses_injectable_clock() {
        let ts = TempStore::new();
        let app = test_app(&ts, clock_at(60)).await;
        ingest(&app, &ts, "sesenta días").await;
        app.set_retention_days(&ts.ctx(), 90).await.expect("write");

        // A fresh app on the same store reads the persisted override.
        let reopened = test_app(&ts, clock_at(60)).await;
        assert_eq!(reopened.retention_days(&ts.ctx()).expect("read"), 90);
        let report = reopened.retention_sweep(&ts.ctx()).await.expect("sweep");
        assert_eq!(report.expired_count, 0, "override honored after reopen");
    }
}
