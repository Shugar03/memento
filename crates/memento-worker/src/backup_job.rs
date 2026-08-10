//! Backup job (T-092, REQ-ML-005, design D8/D4).
//!
//! A thin wrapper over `AppService::backup`, which already runs the full
//! D8 sequence: compact → copy tenant dirs (keys/ excluded) → encrypt the
//! tar with a fresh per-backup AES-256-GCM key → wrap that key with the
//! tenant master key. Artifacts land at `backups/<tid>/<ts>/`.
//!
//! The job exists so the backup participates in the rotation (and in the
//! `--now` one-shot) with the same failure semantics as every other job:
//! a failed backup is a failed job, reported loudly (REQ-OP-002 fail-loudly
//! posture — a cron entry sees a non-zero exit) and re-runnable on the next
//! tick (backup artifacts are timestamped; a retry simply creates a new one).
//!
//! Restore stays a standalone operation (`memento_application::backup::
//! restore_backup`) by design: it requires a quiesced store, so it can never
//! run inside the live worker process.

use crate::scheduler::Job;
use memento_application::AppService;
use memento_domain::{DomainError, TenantContext};
use serde_json::{Value, json};
use std::sync::Arc;

/// The backup job: compact→copy→encrypt per-backup key (T-092).
pub struct BackupJob {
    app: Arc<AppService>,
    ctx: TenantContext,
}

impl BackupJob {
    /// A backup job for the bound tenant's app.
    pub fn new(app: Arc<AppService>, ctx: TenantContext) -> Self {
        Self { app, ctx }
    }
}

#[async_trait::async_trait]
impl Job for BackupJob {
    fn name(&self) -> &'static str {
        "backup"
    }

    async fn run(&self) -> Result<Value, DomainError> {
        let report = self.app.backup(&self.ctx).await?;
        Ok(json!({
            "backup_dir": report.path.file_name().map(|n| n.to_string_lossy().to_string()),
            "chunk_count": report.chunk_count,
            "created_at": report.created_at.to_rfc3339(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memento_application::backup::restore_backup;
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

    #[tokio::test]
    async fn backup_job_produces_decryptable_artifact_and_restores() {
        // T-092 acceptance: the artifact is decryptable and the restore
        // drill passes (search equivalence after restore, REQ-ML-005).
        let ts = TempStore::new();
        let app = test_app(&ts, TestClock::default()).await;
        app.ingest_text(
            &ts.ctx(),
            IngestTextRequest {
                text: "la memoria persiste a través del trabajo de respaldo".into(),
                doc_id: None,
                metadata: None,
            },
        )
        .await
        .expect("ingest");

        let job = BackupJob::new(app.clone(), ts.ctx());
        let report = job.run().await.expect("backup job runs");

        assert_eq!(report["chunk_count"], 1);
        let backup_dir = ts.root().join("backups").join(ts.tenant_id().to_string());
        let dirs: Vec<_> = std::fs::read_dir(&backup_dir)
            .expect("backups dir")
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(dirs.len(), 1, "one artifact dir");
        assert_eq!(
            dirs[0].file_name().unwrap().to_string_lossy().to_string(),
            report["backup_dir"].as_str().unwrap()
        );
        assert!(dirs[0].join("backup.enc").exists());
        assert!(dirs[0].join("backup.key.json").exists());

        drop(app); // release the store before wiping

        // Wipe the live data dirs (keys/ survives — decryption root).
        let tenant_dir = ts.lancedb_dir().parent().unwrap().to_path_buf();
        for entry in ["lancedb", "okf-bundles", "conversation", "config.toml"] {
            let path = tenant_dir.join(entry);
            if path.is_dir() {
                std::fs::remove_dir_all(&path).unwrap();
            } else if path.exists() {
                std::fs::remove_file(&path).unwrap();
            }
        }

        let restored = restore_backup(ts.root(), ts.tenant_id(), &dirs[0])
            .await
            .expect("restore drill");
        assert_eq!(restored.chunk_count, 1);

        let reopened = test_app(&ts, TestClock::default()).await;
        let hits = reopened
            .store()
            .search(
                &ts.ctx(),
                SearchQuery::new("memoria", 10, *ts.workspace_id()),
            )
            .await
            .expect("search after restore");
        assert!(!hits.is_empty(), "searchable after restore");
        assert!(
            hits.iter()
                .all(|h| h.provenance.tenant_id == *ts.tenant_id()),
            "provenance intact"
        );
    }

    #[tokio::test]
    async fn backup_job_failure_is_a_failed_job() {
        // The job surfaces the underlying error: a backup against a corrupt
        // master key fails loudly instead of silently skipping.
        let ts = TempStore::new();
        let app = test_app(&ts, TestClock::default()).await;

        // Corrupt the master key (invalid length) so the backup fails.
        let master = app.tenant_dir().join("keys").join("master.key");
        std::fs::create_dir_all(master.parent().unwrap()).unwrap();
        std::fs::write(&master, b"too-short").unwrap();

        let job = BackupJob::new(app, ts.ctx());
        let err = job.run().await.expect_err("backup must fail");
        assert_eq!(err.code(), "BACKUP_CORRUPT");
    }
}
