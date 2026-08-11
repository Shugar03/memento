//! Code-knowledge facade (T-067): wires the `KnowledgePort` (implemented by
//! the okf adapter, batch 5) behind the application layer with the REQ-TA-005
//! context guard.
//!
//! Surfaces (MCP `code.*` tools in T-073, CLI in T-084) talk to this facade,
//! never to the adapter directly. The facade:
//!
//! * validates the caller's context against the process-bound tenant BEFORE
//!   any adapter work (defense in depth — the adapter also enforces it, but
//!   the guard fires here first, exactly like every other use case);
//! * constructs the [`OkfIndex`] lazily (first call) so `AppService::open`
//!   stays cheap and `--no-embeddings` mode passes `None` as the embedder;
//! * exposes the full port surface read-only (indexing is CLI-driven,
//!   REQ-CK-* design note).

use crate::AppService;
use memento_domain::{DomainError, KnowledgeArtifact, TenantContext};
use memento_okf::OkfIndex;
use memento_ports::{KnowledgePort, ProjectOverview};

/// The guarded code facade: one adapter instance, tenant-bound, with the
/// ctx guard applied on every call.
pub struct CodeFacade {
    index: OkfIndex,
}

impl CodeFacade {
    /// Open the facade bound to `ctx`'s tenant (callers pass the context
    /// they intend to bind; [`AppService::code`] enforces the match).
    pub async fn open(
        ctx: &TenantContext,
        root: impl AsRef<std::path::Path>,
        embedder: Option<std::sync::Arc<dyn memento_ports::EmbedPort>>,
    ) -> Result<Self, DomainError> {
        Ok(Self {
            index: OkfIndex::open(ctx, root, embedder).await?,
        })
    }

    fn guard(&self, ctx: &TenantContext) -> Result<(), DomainError> {
        if ctx.tenant_id() == self.index.tenant_id() {
            Ok(())
        } else {
            Err(DomainError::TenantForbidden)
        }
    }
}

#[async_trait::async_trait]
impl KnowledgePort for CodeFacade {
    async fn project_overview(
        &self,
        ctx: &TenantContext,
        project_id: &str,
    ) -> Result<ProjectOverview, DomainError> {
        self.guard(ctx)?;
        self.index.project_overview(ctx, project_id).await
    }

    async fn symbol_lookup(
        &self,
        ctx: &TenantContext,
        project_id: &str,
        symbol: &str,
    ) -> Result<Option<KnowledgeArtifact>, DomainError> {
        self.guard(ctx)?;
        self.index.symbol_lookup(ctx, project_id, symbol).await
    }

    async fn callers_of(
        &self,
        ctx: &TenantContext,
        project_id: &str,
        symbol: &str,
    ) -> Result<Vec<String>, DomainError> {
        self.guard(ctx)?;
        self.index.callers_of(ctx, project_id, symbol).await
    }

    async fn callees_of(
        &self,
        ctx: &TenantContext,
        project_id: &str,
        symbol: &str,
    ) -> Result<Vec<String>, DomainError> {
        self.guard(ctx)?;
        self.index.callees_of(ctx, project_id, symbol).await
    }

    async fn impact(
        &self,
        ctx: &TenantContext,
        project_id: &str,
        symbol: &str,
    ) -> Result<Vec<String>, DomainError> {
        self.guard(ctx)?;
        self.index.impact(ctx, project_id, symbol).await
    }

    async fn dependencies(
        &self,
        ctx: &TenantContext,
        project_id: &str,
    ) -> Result<Vec<String>, DomainError> {
        self.guard(ctx)?;
        self.index.dependencies(ctx, project_id).await
    }

    async fn search(
        &self,
        ctx: &TenantContext,
        project_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<KnowledgeArtifact>, DomainError> {
        self.guard(ctx)?;
        self.index.search(ctx, project_id, query, limit).await
    }

    async fn graph_dump(
        &self,
        ctx: &TenantContext,
        project_id: &str,
    ) -> Result<serde_json::Value, DomainError> {
        self.guard(ctx)?;
        self.index.graph_dump(ctx, project_id).await
    }
}

impl AppService {
    /// The bound code facade (lazily opened once per service; REQ-TA-005
    /// guard fires for any foreign context BEFORE adapter work).
    pub async fn code(
        &self,
        ctx: &TenantContext,
    ) -> Result<std::sync::Arc<dyn KnowledgePort>, DomainError> {
        // The guard fires here, before anything is opened or queried.
        self.ensure_bound_tenant(ctx)?;
        // Fast path (no await inside the guard).
        if let Some(facade) = self
            .code
            .lock()
            .map_err(|_| DomainError::Internal {
                message: "code facade lock poisoned".into(),
            })?
            .as_ref()
        {
            return Ok(facade.clone());
        }
        // Slow path: construct OUTSIDE the lock (never hold a MutexGuard
        // across an await point — a concurrent call would deadlock).
        let facade =
            std::sync::Arc::new(CodeFacade::open(ctx, &self.root, self.embedder.clone()).await?);
        let mut slot = self.code.lock().map_err(|_| DomainError::Internal {
            message: "code facade lock poisoned".into(),
        })?;
        if let Some(existing) = slot.as_ref() {
            return Ok(existing.clone());
        }
        *slot = Some(facade.clone());
        Ok(facade)
    }
}

#[cfg(test)]
mod tests {
    use crate::test_util::test_app;
    use memento_domain::TenantId;
    use memento_testkit::TempStore;

    #[tokio::test]
    async fn facade_opens_and_forwards_lazily() {
        let ts = TempStore::new();
        let clock = memento_testkit::TestClock::default();
        let app = test_app(&ts, clock).await;

        let port = app.code(&ts.ctx()).await.expect("facade opens");
        // Unindexed project → the okf adapter's structured bilingual error
        // (REQ-CK-003), proving the facade forwards to the real adapter.
        let err = port
            .project_overview(&ts.ctx(), "0000000000000000")
            .await
            .expect_err("unindexed");
        assert_eq!(err.code(), "NOT_FOUND");
    }

    #[tokio::test]
    async fn guard_fires_without_context() {
        // REQ-TA-005: a foreign tenant context is rejected by the facade
        // BEFORE any adapter work.
        let ts = TempStore::new();
        let clock = memento_testkit::TestClock::default();
        let app = test_app(&ts, clock).await;

        let foreign = memento_domain::TenantContext::new_for_tests(
            TenantId::new(),
            memento_domain::WorkspaceId::new(),
            memento_domain::AgentId::new("intruder"),
        );
        let err = match app.code(&foreign).await {
            Err(err) => err,
            Ok(_) => panic!("guard must fire for a foreign context"),
        };
        assert_eq!(err.code(), "TENANT_FORBIDDEN");
    }

    #[tokio::test]
    async fn facade_is_single_instance_per_service() {
        let ts = TempStore::new();
        let clock = memento_testkit::TestClock::default();
        let app = test_app(&ts, clock).await;

        let a = app.code(&ts.ctx()).await.expect("a");
        let b = app.code(&ts.ctx()).await.expect("b");
        // Same Arc back — one adapter per bound tenant.
        assert!(std::sync::Arc::ptr_eq(&a, &b), "cached facade");
    }

    // ---------- B3 fix: embedder pre-warmed at tenant open --------------

    #[tokio::test]
    async fn b3_warm_embedder_runs_exactly_once_on_open() {
        // B3 fix (obs 2663): the first code.search call used to pay ~5 s
        // of ONNX cold-start. AppService::open() now eagerly warms the
        // embedder so the first user-facing search is fast. The warmup
        // runs EXACTLY ONCE per service — re-opening or re-warming must
        // not re-invoke the embedder.
        use std::sync::atomic::{AtomicUsize, Ordering as AOrd};
        use std::sync::Arc;

        struct CountingEmbed {
            warm_calls: AtomicUsize,
        }
        #[async_trait::async_trait]
        impl memento_ports::EmbedPort for CountingEmbed {
            async fn embed(
                &self,
                texts: &[&str],
            ) -> Result<Vec<Vec<f32>>, memento_domain::DomainError> {
                // The warm-up call passes a single empty string. Real
                // search calls pass the user query.
                if texts.len() == 1 && texts[0].is_empty() {
                    self.warm_calls.fetch_add(1, AOrd::SeqCst);
                }
                Ok(texts
                    .iter()
                    .map(|t| memento_testkit::deterministic_embed(t, 768))
                    .collect())
            }
        }

        let embed = Arc::new(CountingEmbed {
            warm_calls: AtomicUsize::new(0),
        });
        let ts = TempStore::new();
        let clock = memento_testkit::TestClock::default();
        let app = crate::AppService::open(
            &ts.ctx(),
            ts.root(),
            crate::test_util::real_fallback_parse(),
            Some(embed.clone() as Arc<dyn memento_ports::EmbedPort>),
            Arc::new(clock),
        )
        .await
        .expect("test app opens");
        assert_eq!(
            embed.warm_calls.load(AOrd::SeqCst),
            1,
            "AppService::open warms the embedder exactly once"
        );

        // Re-warming must not re-invoke.
        app.warm_embedder().await.expect("warm is idempotent");
        app.warm_embedder().await.expect("warm is idempotent");
        assert_eq!(
            embed.warm_calls.load(AOrd::SeqCst),
            1,
            "subsequent warm-up calls are no-ops"
        );
    }

    #[tokio::test]
    async fn b3_open_without_embedder_succeeds_and_warm_is_a_noop() {
        // B3 fix: --no-embeddings mode (embedder is None) skips the
        // warm-up entirely. AppService::open() must succeed and the
        // facade must still serve code.* tools in literal mode.
        let ts = TempStore::new();
        let app = crate::test_util::test_app_no_embed(&ts).await;
        // No exception thrown, no panic, no error.
        app.warm_embedder().await.expect("warm with no embedder");
    }
}
