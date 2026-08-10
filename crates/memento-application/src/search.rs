//! Search use cases (T-061, REQ-MR-001/002/003/005/006).
//!
//! [`AppService::search`] composes the two retrieval modes:
//!
//! * **FTS only (default)** — BM25 via the store's `SearchPort` (REQ-MR-001:
//!   RRF off by default). The port validates `top_k` and returns hits with
//!   provenance.
//! * **Hybrid (RRF)** — behind the per-query `rrf_enabled` toggle
//!   (REQ-MR-002). The port deliberately rejects `rrf_enabled` (it cannot
//!   embed the query text), so this layer composes the vector leg
//!   ([`memento_lancedb::vector_search`]) with the FTS leg
//!   ([`memento_lancedb::full_text_search`]), fuses both ranked lists with
//!   RRF ([`memento_lancedb::rrf_fuse`], k=60) and materializes the top-k
//!   hits. No re-index is involved: vectors exist from day 1 (REQ-MC-004).
//!
//! # `--no-embeddings` (REQ-MR-003)
//!
//! Hybrid without an embedder is a structured error, never a silent
//! degradation: there is no vector leg to fuse, and returning FTS-only
//! results for a hybrid request would be wrong results.

use crate::AppService;
use memento_domain::{ChunkId, DomainError, MemoryChunk, TenantContext};
use memento_lancedb::{fetch_search_hits, full_text_search, rrf_fuse, vector_search};
use memento_ports::{SearchFilters, SearchHit, SearchPort, SearchQuery};
use std::collections::HashMap;

/// RRF fusion constant (standard k=60; discovery + design: rank-based sum).
const RRF_K: f32 = 60.0;

impl AppService {
    /// Search the workspace (REQ-MR-001/002/003): BM25 by default, RRF hybrid
    /// behind the per-query toggle.
    ///
    /// # Errors
    ///
    /// * `TopKExceeded` — `top_k` over the store maximum (100).
    /// * `InvalidInput` — `rrf_enabled` under `--no-embeddings` (REQ-MR-003).
    /// * Adapter errors are propagated stage-named.
    pub async fn search(
        &self,
        ctx: &TenantContext,
        query: SearchQuery,
    ) -> Result<Vec<SearchHit>, DomainError> {
        if query.rrf_enabled {
            self.hybrid_search(ctx, query).await
        } else {
            // Default mode: BM25 only (REQ-MR-001/002). The port validates
            // top_k and handles the empty-query no-match case.
            self.store.search(ctx, query).await
        }
    }

    /// Hybrid retrieval: embed the query → vector leg + FTS leg → RRF fuse
    /// → materialize the top-k hits (REQ-MR-002/003).
    async fn hybrid_search(
        &self,
        ctx: &TenantContext,
        query: SearchQuery,
    ) -> Result<Vec<SearchHit>, DomainError> {
        // REQ-MR-003: hybrid without embeddings is a structured error.
        let embedder = self
            .embedder
            .as_ref()
            .ok_or_else(|| DomainError::InvalidInput {
                message: "hybrid search requires the embedding model \
                      (runs with --no-embeddings; switch to FTS mode)"
                    .into(),
            })?;

        let vectors = embedder.embed(&[query.query.as_str()]).await?;
        let query_vec = vectors
            .into_iter()
            .next()
            .ok_or_else(|| DomainError::EmbeddingFailed {
                message: "embedder returned no vector for the query".into(),
            })?;

        // Both legs are tenant+workspace scoped (REQ-MR-006). The FTS leg
        // honors doc/source filters; the vector leg scopes workspace only
        // (the adapter's contract) — filters are re-applied post-fusion.
        let vec_leg = vector_search(
            &self.store,
            ctx,
            &query_vec,
            &query.workspace_id,
            query.top_k,
        )
        .await?;
        let fts_hits = full_text_search(
            &self.store,
            ctx,
            &query.query,
            &query.workspace_id,
            query.top_k,
            query.filters.as_ref(),
        )
        .await?;
        let fts_leg: Vec<(ChunkId, f32)> = fts_hits.iter().map(|h| (h.chunk_id, h.score)).collect();

        let fused = rrf_fuse(&vec_leg, &fts_leg, RRF_K);
        let scores: HashMap<ChunkId, f32> = fused.iter().copied().collect();
        let ids: Vec<ChunkId> = fused.iter().take(query.top_k).map(|(id, _)| *id).collect();

        let mut hits = fetch_search_hits(&self.store, ctx, &ids).await?;
        for hit in &mut hits {
            hit.score = scores.get(&hit.chunk_id).copied().unwrap_or(0.0);
        }
        if let Some(filters) = &query.filters {
            hits.retain(|h| filters_match(filters, h));
        }
        // Deterministic final ranking: fused score desc, id tiebreak.
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.chunk_id.to_string().cmp(&b.chunk_id.to_string()))
        });
        hits.truncate(query.top_k);
        Ok(hits)
    }

    /// Fetch one chunk by id within the bound tenant (REQ-MR-005): foreign
    /// ids resolve to `None` — no existence leak.
    pub async fn get_chunk(
        &self,
        ctx: &TenantContext,
        id: &ChunkId,
    ) -> Result<Option<MemoryChunk>, DomainError> {
        self.store.get_chunk(ctx, id).await
    }
}

/// Post-fusion doc/source filter for the hybrid path (the vector leg does
/// not carry filters).
fn filters_match(filters: &SearchFilters, hit: &SearchHit) -> bool {
    let doc_ok = filters
        .doc_id
        .is_none_or(|doc_id| hit.provenance.doc_id == doc_id);
    let source_ok = filters
        .source
        .as_ref()
        .is_none_or(|source| *source == hit.provenance.source);
    doc_ok && source_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{other_workspace_ctx, test_app, test_app_no_embed};
    use memento_domain::DocId;
    use memento_ports::{IngestTextRequest, SearchFilters};
    use memento_testkit::{TempStore, TestClock, spanish_corpus};

    fn text_request(text: &str) -> IngestTextRequest {
        IngestTextRequest {
            text: text.to_string(),
            doc_id: None,
            metadata: None,
        }
    }

    /// Ingest a two-doc corpus and return the app:
    /// doc A talks about "memoria" + "río", doc B about "tecnología".
    async fn corpus_app(ts: &TempStore) -> AppService {
        let clock = TestClock::default();
        let app = test_app(ts, clock).await;
        app.ingest_text(
            &ts.ctx(),
            text_request("La memoria es un río subterráneo que nunca deja de fluir."),
        )
        .await
        .expect("doc a");
        app.ingest_text(
            &ts.ctx(),
            text_request("La tecnología transforma la manera en que trabajamos cada día."),
        )
        .await
        .expect("doc b");
        app
    }

    #[tokio::test]
    async fn default_search_is_fts_only_and_ranked() {
        // REQ-MR-001/002: default mode ranks by BM25, no vector fusion.
        let ts = TempStore::new();
        let app = corpus_app(&ts).await;
        let q = SearchQuery::new("memoria río", 10, *ts.workspace_id());
        assert!(!q.rrf_enabled, "toggle off by default");

        let hits = app.search(&ts.ctx(), q).await.expect("search ok");
        assert!(!hits.is_empty());
        assert_eq!(hits[0].chunk_id, hits[0].provenance.chunk_id, "provenance");
        assert!(
            hits.iter()
                .all(|h| h.provenance.tenant_id == *ts.tenant_id()),
            "REQ-MC-006 provenance on every hit"
        );
    }

    #[tokio::test]
    async fn no_match_returns_empty_not_error() {
        // REQ-MR-001 scenario 2.
        let ts = TempStore::new();
        let app = corpus_app(&ts).await;
        let hits = app
            .search(
                &ts.ctx(),
                SearchQuery::new("zzzqqqxw", 10, *ts.workspace_id()),
            )
            .await
            .expect("no error");
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn hybrid_reranks_when_enabled() {
        // REQ-MR-002: with the toggle on, both legs fuse (stub embedder
        // returns a deterministic vector). Results carry fused scores and
        // still respect the workspace scope.
        let ts = TempStore::new();
        let app = corpus_app(&ts).await;
        let mut q = SearchQuery::new("memoria río", 10, *ts.workspace_id());
        q.rrf_enabled = true;

        let hits = app.search(&ts.ctx(), q).await.expect("hybrid ok");
        assert!(!hits.is_empty(), "fused results");
        assert!(
            hits.iter()
                .all(|h| h.provenance.workspace_id == *ts.workspace_id()),
            "workspace scope holds (REQ-MR-006)"
        );
    }

    #[tokio::test]
    async fn hybrid_without_embeddings_is_structured_error() {
        // REQ-MR-003: --no-embeddings + explicit hybrid → structured error.
        let ts = TempStore::new();
        let app = test_app_no_embed(&ts).await;
        app.ingest_text(&ts.ctx(), text_request(&spanish_corpus().join(" ")))
            .await
            .expect("ingest ok");

        let mut q = SearchQuery::new("memoria", 5, *ts.workspace_id());
        q.rrf_enabled = true;
        let err = app.search(&ts.ctx(), q).await.expect_err("must fail");
        assert_eq!(err.code(), "INVALID_INPUT", "structured, not silent");
    }

    #[tokio::test]
    async fn filters_doc_id_applies_in_both_modes() {
        let ts = TempStore::new();
        let app = corpus_app(&ts).await;
        let doc_a = app
            .store()
            .search(
                &ts.ctx(),
                SearchQuery::new("memoria", 5, *ts.workspace_id()),
            )
            .await
            .expect("probe")[0]
            .provenance
            .doc_id;

        let fts = app
            .search(
                &ts.ctx(),
                SearchQuery {
                    query: "memoria".into(),
                    top_k: 10,
                    workspace_id: *ts.workspace_id(),
                    rrf_enabled: false,
                    filters: Some(SearchFilters {
                        doc_id: Some(doc_a),
                        source: None,
                    }),
                },
            )
            .await
            .expect("fts filter");
        assert!(
            fts.iter().all(|h| h.provenance.doc_id == doc_a),
            "FTS leg honors doc filter"
        );

        let hybrid = app
            .search(
                &ts.ctx(),
                SearchQuery {
                    query: "memoria".into(),
                    top_k: 10,
                    workspace_id: *ts.workspace_id(),
                    rrf_enabled: true,
                    filters: Some(SearchFilters {
                        doc_id: Some(doc_a),
                        source: None,
                    }),
                },
            )
            .await
            .expect("hybrid filter");
        assert!(
            hybrid.iter().all(|h| h.provenance.doc_id == doc_a),
            "post-fusion filter holds"
        );
    }

    #[tokio::test]
    async fn workspace_isolation_holds_in_both_modes() {
        // REQ-MR-006: data in W2 never leaks into W1 searches.
        let ts = TempStore::new();
        let app = corpus_app(&ts).await;
        let other_ws = other_workspace_ctx(&ts);

        for rrf in [false, true] {
            let hits = app
                .search(
                    &other_ws,
                    SearchQuery {
                        query: "memoria río tecnología".into(),
                        top_k: 10,
                        workspace_id: *other_ws.workspace_id(),
                        rrf_enabled: rrf,
                        filters: None,
                    },
                )
                .await
                .expect("search ok");
            assert!(hits.is_empty(), "rrf={rrf}: no W1 chunks in W2");
        }
    }

    #[tokio::test]
    async fn get_chunk_is_tenant_scoped() {
        // REQ-MR-005: a foreign chunk id resolves to None (no leak).
        let ts = TempStore::new();
        let app = corpus_app(&ts).await;
        let hit = app
            .search(
                &ts.ctx(),
                SearchQuery::new("memoria", 5, *ts.workspace_id()),
            )
            .await
            .expect("search")[0]
            .chunk_id;
        let chunk = app
            .get_chunk(&ts.ctx(), &hit)
            .await
            .expect("read")
            .expect("own chunk");
        assert_eq!(chunk.id, hit);

        // Unknown id → None (not an error).
        assert!(
            app.get_chunk(&ts.ctx(), &ChunkId::new())
                .await
                .expect("read")
                .is_none()
        );
        // Doc ids are independent namespaces: an unknown doc id is fine.
        let _ = DocId::new();
    }
}
