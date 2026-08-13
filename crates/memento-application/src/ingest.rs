//! Ingest use cases (T-060, REQ-MC-001/002/005/007).
//!
//! [`AppService::ingest_text`] and [`AppService::ingest_document`] run the
//! full pipeline exactly as the design prescribes:
//!
//! ```text
//! validate (10MB blob / 10k chunks) → chore id → dedup probe (REQ-MC-005)
//!   → parse (document only; single normalization boundary) → chunk
//!   → embed (batch 64; --no-embeddings → None) → single batch add (atomic)
//!   → docs row (idempotency key) → audit
//! ```
//!
//! # Async pipeline (B2)
//!
//! The chunk → embed tail is pipelined: text-splitter's lazy `chunk_indices`
//! iterator streams chunks from a blocking-pool producer into a bounded
//! `tokio::sync::mpsc` channel (depth 16), while the embed consumer runs ONNX
//! batches of 64 on the calling task. The two expensive stages overlap
//! instead of serializing (chunk tokenization is CPU-bound; embedding is
//! async inference). Writes still land in ONE `table.add()` call, so the
//! atomic-visibility contract (REQ-MC-007) is unchanged.
//!
//! # Idempotency (REQ-MC-005)
//!
//! The dedup probe hashes the RAW INPUT (`sha256(tenant ‖ NUL ‖ content)`),
//! tenant-scoped by construction AND by query scope. A duplicate ingest
//! returns the existing chunk ids + the stored doc id — zero new chunks.
//! Scope is tenant-wide (locked decision): re-ingesting identical content in
//! a different workspace of the SAME tenant still dedups.
//!
//! # Atomicity (REQ-MC-007)
//!
//! Writes happen only after every fallible stage (parse/chunk/embed) has
//! succeeded, and the chunks land in ONE `table.add()` call — visible all or
//! not at all. The docs row is written AFTER the chunk commit; if it fails
//! the ingest still succeeded and only the dedup probe for that doc degrades
//! (traced loudly).

use crate::{AppService, EMBED_BATCH, MAX_BLOB_BYTES, MAX_CHUNKS_PER_DOC, embedding_model_version};
use memento_domain::{
    ChoreId, ChunkId, DocId, DomainError, MemoryChunk, Provenance, SourceKind, TenantContext,
};
use memento_lancedb::{
    DocRecord, add_chunks_batch, chunk_ids_by_doc, find_doc_by_hash, upsert_doc,
};
use memento_parse::chunk::MAX_TOKENS;
use memento_ports::{IngestDocumentRequest, IngestResult, IngestTextRequest};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::Instrument;

/// Bounded in-flight buffer between the chunk producer and the embed
/// consumer (B2). The embedder processes one 64-chunk ONNX batch at a time
/// while the chunker tokenizes ahead on the blocking pool; 16 keeps the
/// chunker ahead across batch boundaries without buffering a whole document.
const PIPELINE_BUFFER: usize = 16;

/// One chunk produced by the pipeline's chunk stage: the id is assigned at
/// production time so ordering and provenance stay deterministic while
/// embedding runs asynchronously ahead of the chunker.
struct PipelineChunk {
    id: ChunkId,
    text: String,
}

/// Pre-chunk quota guard (REQ-MC-007, design MC Q4).
///
/// text-splitter 0.32's token sizer probes prefix ranges per chunk, which is
/// super-linear on very large inputs: without this guard an over-quota ingest
/// (e.g. a 3M-token text) would spend minutes chunking before the count check
/// in `stage_chunks` could reject it. Counting tokens is ONE O(n) encode, and
/// every chunk is at most [`MAX_TOKENS`] tokens, so
/// `tokens > MAX_CHUNKS_PER_DOC * MAX_TOKENS` proves the ingest would exceed
/// the 10k-chunk limit — exact, not a heuristic (overlap only makes the real
/// chunk count higher, so the bound stays conservative in the right direction).
fn over_chunk_quota(app: &AppService, text: &str) -> bool {
    app.chunker.token_count(text) > MAX_CHUNKS_PER_DOC * MAX_TOKENS
}

impl AppService {
    /// Ingest raw text (REQ-MC-001): chunk → embed → store, returning the
    /// produced chunk ids plus the chore id that makes the operation
    /// observable (REQ-MC-007).
    ///
    /// # Errors
    ///
    /// * `InvalidInput` — empty or whitespace-only text (nothing stored).
    /// * `QuotaExceeded` — the ingest produces more than
    ///   [`MAX_CHUNKS_PER_DOC`] chunks (nothing stored).
    /// * `EmbeddingFailed` / adapter errors — propagated stage-named
    ///   (REQ-MC-007, zero visible chunks).
    pub async fn ingest_text(
        &self,
        ctx: &TenantContext,
        req: IngestTextRequest,
    ) -> Result<IngestResult, DomainError> {
        // REQ-OBS-003: the ingest span carries the tenant/agent/workspace
        // context; the chore_id slot opens empty and is recorded as soon as
        // the id exists (record-if-Some, never "None").
        let span = crate::ingest_span(ctx, *ctx.workspace_id());
        let record_chore = span.clone();
        async {
            // REQ-OBS-006: operation counter (labels carry ids only — no
            // content or secrets; no-op without a recorder).
            metrics::counter!(
                "memento_ingest_requests_total",
                "tenant_id" => ctx.tenant_id().to_string()
            )
            .increment(1);
            if req.text.trim().is_empty() {
                return Err(DomainError::InvalidInput {
                    message: "ingest_text requires non-empty text".into(),
                });
            }
            let chore_id = ChoreId::new();
            record_chore.record("chore_id", chore_id.to_string());
            let hash = self.content_hash(req.text.as_bytes());

            // Dedup probe (REQ-MC-005): identical content in this tenant already
            // ingested → reference the existing records, write nothing.
            if let Some(doc) = find_doc_by_hash(&self.store, ctx, &hash).await? {
                // REQ-OBS-006 "cache hit (dedup)": the content was ingested
                // before — no chunk/embed/write work runs.
                metrics::counter!(
                    "memento_ingest_dedup_hits_total",
                    "tenant_id" => ctx.tenant_id().to_string()
                )
                .increment(1);
                let ids = chunk_ids_by_doc(&self.store, ctx, &doc.doc_id).await?;
                self.record_audit(
                    ctx,
                    "ingest",
                    json!({
                        "doc_id": doc.doc_id,
                        "chunks": ids.len(),
                        "duplicate": true,
                        "source": "text",
                    }),
                    Some(chore_id),
                );
                return Ok(IngestResult {
                    chunk_ids: ids,
                    doc_id: doc.doc_id,
                    chore_id: Some(chore_id),
                });
            }

            let doc_id = req.doc_id.unwrap_or_default();
            let title = title_of(req.metadata.as_ref());
            let created_at = self.clock.now();

            self.stage_chunks(
                ctx,
                req.text,
                StageSpec {
                    doc_id,
                    source: SourceKind::Text,
                    title,
                    content_hash: hash,
                    created_at,
                    chore_id,
                },
            )
            .await
        }
        .instrument(span)
        .await
    }

    /// Ingest a document blob (REQ-MC-002): normalize to Markdown through
    /// the single normalization boundary, then run the same pipeline as
    /// text ingest.
    ///
    /// # Errors
    ///
    /// * `InvalidInput` — empty blob, or the normalized Markdown is empty.
    /// * `QuotaExceeded` — blob over [`MAX_BLOB_BYTES`] or more than
    ///   [`MAX_CHUNKS_PER_DOC`] chunks.
    /// * `Parse` / subprocess codes — normalization failed (zero writes).
    pub async fn ingest_document(
        &self,
        ctx: &TenantContext,
        req: IngestDocumentRequest,
    ) -> Result<IngestResult, DomainError> {
        // REQ-OBS-003: same span contract as ingest_text — the chore id is
        // recorded as soon as it exists.
        let span = crate::ingest_span(ctx, *ctx.workspace_id());
        let record_chore = span.clone();
        async {
            metrics::counter!(
                "memento_ingest_requests_total",
                "tenant_id" => ctx.tenant_id().to_string()
            )
            .increment(1);
            if req.blob.is_empty() {
                return Err(DomainError::InvalidInput {
                    message: "ingest_document requires a non-empty blob".into(),
                });
            }
            if req.blob.len() as u64 > MAX_BLOB_BYTES {
                return Err(DomainError::QuotaExceeded {
                    message: format!(
                        "document blob is {} bytes, limit is {MAX_BLOB_BYTES}",
                        req.blob.len()
                    ),
                });
            }
            let chore_id = ChoreId::new();
            record_chore.record("chore_id", chore_id.to_string());
            let hash = self.content_hash(&req.blob);

            // Dedup probe on the raw blob (REQ-MC-005).
            if let Some(doc) = find_doc_by_hash(&self.store, ctx, &hash).await? {
                metrics::counter!(
                    "memento_ingest_dedup_hits_total",
                    "tenant_id" => ctx.tenant_id().to_string()
                )
                .increment(1);
                let ids = chunk_ids_by_doc(&self.store, ctx, &doc.doc_id).await?;
                self.record_audit(
                    ctx,
                    "ingest",
                    json!({
                        "doc_id": doc.doc_id,
                        "chunks": ids.len(),
                        "duplicate": true,
                        "source": source_label(&doc.source),
                    }),
                    Some(chore_id),
                );
                return Ok(IngestResult {
                    chunk_ids: ids,
                    doc_id: doc.doc_id,
                    chore_id: Some(chore_id),
                });
            }

            // Single normalization boundary (REQ-MC-002); failures are
            // stage-named by the adapter (REQ-MC-007) and write nothing.
            let parsed = self.parse.parse(&req.blob, req.source_hint.clone()).await?;
            if parsed.markdown.trim().is_empty() {
                return Err(DomainError::InvalidInput {
                    message: "document normalized to empty content".into(),
                });
            }

            let doc_id = req.doc_id.unwrap_or_default();
            let title = title_of(req.metadata.as_ref());
            let created_at = self.clock.now();

            self.stage_chunks(
                ctx,
                parsed.markdown,
                StageSpec {
                    doc_id,
                    source: parsed.source_kind,
                    title,
                    content_hash: hash,
                    created_at,
                    chore_id,
                },
            )
            .await
        }
        .instrument(span)
        .await
    }

    /// The shared pipeline tail (B2): chunk-count pre-check → pipelined
    /// chunk → embed (batch 64) → single atomic batch add → docs row →
    /// audit (REQ-MC-007).
    ///
    /// The chunk stage streams lazily from a blocking-pool producer into a
    /// bounded [`mpsc`] channel (depth [`PIPELINE_BUFFER`]); the embed stage
    /// consumes batches of [`EMBED_BATCH`] on the calling task, so ONNX
    /// inference overlaps with the tokenizer work of the producer. Writes
    /// still happen in ONE `add_chunks_batch` call — atomic visibility is
    /// unchanged — and every fallible stage fails the whole ingest with
    /// zero writes.
    ///
    /// The spec bundles everything the staged write needs besides the
    /// normalized text itself (keeps the signature at 3 args).
    async fn stage_chunks(
        &self,
        ctx: &TenantContext,
        markdown: String,
        spec: StageSpec,
    ) -> Result<IngestResult, DomainError> {
        // REQ-OBS-006: pipeline latency (chunk → embed → single atomic write).
        let started = std::time::Instant::now();
        // Quota pre-check BEFORE the splitter (see `over_chunk_quota`).
        if over_chunk_quota(self, &markdown) {
            return Err(DomainError::QuotaExceeded {
                message: format!(
                    "ingest would produce more than {MAX_CHUNKS_PER_DOC} chunks \
                     ({MAX_CHUNKS_PER_DOC} x {MAX_TOKENS} token limit)"
                ),
            });
        }

        // The chunk producer runs on the blocking pool: `chunk_iter` is a
        // lazy iterator, so each tokenizer probe happens inside `next()` and
        // chunks stream into the bounded channel while the embed consumer
        // below awaits ONNX inference — the two expensive stages overlap
        // instead of serializing.
        let (tx, mut rx) = mpsc::channel::<PipelineChunk>(PIPELINE_BUFFER);
        let chunker = Arc::clone(&self.chunker);
        let producer = tokio::task::spawn_blocking(move || {
            for chunk in chunker.chunk_iter(&markdown) {
                if tx
                    .blocking_send(PipelineChunk {
                        id: ChunkId::new(),
                        text: chunk.text,
                    })
                    .is_err()
                {
                    // Consumer dropped the channel (embed failure / quota
                    // abort): stop producing; the failed send is the signal.
                    return;
                }
            }
        });

        let mut chunks: Vec<MemoryChunk> = Vec::new();
        let mut pending: Vec<(ChunkId, String)> = Vec::with_capacity(EMBED_BATCH);
        while let Some(chunk) = rx.recv().await {
            // Hard chunk-count backstop (REQ-MC-007). The O(n) token
            // pre-check above makes this unreachable in practice, but the
            // pipeline must never write past the limit.
            if chunks.len() + pending.len() >= MAX_CHUNKS_PER_DOC {
                return Err(DomainError::QuotaExceeded {
                    message: format!("ingest produced more than {MAX_CHUNKS_PER_DOC} chunks"),
                });
            }
            pending.push((chunk.id, chunk.text));
            if pending.len() == EMBED_BATCH {
                chunks.extend(self.embed_and_build(ctx, &pending, &spec).await?);
                pending.clear();
            }
        }
        if !pending.is_empty() {
            chunks.extend(self.embed_and_build(ctx, &pending, &spec).await?);
        }

        // The producer finished cleanly (channel closed after the last
        // send). A panicked producer must not leave a partial write behind.
        producer.await.map_err(|err| DomainError::Internal {
            message: format!("chunk pipeline panicked: {err}"),
        })?;

        // ONE add call → atomic visibility (REQ-MC-007).
        add_chunks_batch(&self.store, ctx, &chunks).await?;

        // Idempotency key row — after the commit; a failure here degrades
        // the dedup probe for this doc only (see module docs).
        let doc = DocRecord {
            doc_id: spec.doc_id,
            tenant_id: *ctx.tenant_id(),
            workspace_id: *ctx.workspace_id(),
            agent_id: ctx.agent_id().clone(),
            title: spec.title.clone(),
            source: spec.source.clone(),
            created_at: spec.created_at,
            content_hash: spec.content_hash.to_string(),
        };
        if let Err(err) = upsert_doc(&self.store, ctx, &doc).await {
            tracing::error!(%err, doc = %spec.doc_id,
                "docs row upsert failed; idempotency probe degraded for this doc");
        }

        self.record_audit(
            ctx,
            "ingest",
            json!({
                "doc_id": spec.doc_id,
                "chunks": chunks.len(),
                "duplicate": false,
                "source": source_label(&spec.source),
            }),
            Some(spec.chore_id),
        );
        // REQ-OBS-006: produced chunk count + pipeline latency (labeled by
        // tenant id; no-op without a recorder).
        metrics::counter!(
            "memento_ingest_chunks_total",
            "tenant_id" => ctx.tenant_id().to_string()
        )
        .increment(chunks.len() as u64);
        metrics::histogram!(
            "memento_ingest_duration_ms",
            "tenant_id" => ctx.tenant_id().to_string()
        )
        .record(started.elapsed().as_secs_f64() * 1000.0);
        Ok(IngestResult {
            chunk_ids: chunks.iter().map(|c| c.id).collect(),
            doc_id: spec.doc_id,
            chore_id: Some(spec.chore_id),
        })
    }

    /// Embed one pipeline batch and build the [`MemoryChunk`] rows for it.
    ///
    /// Ids arrive pre-assigned from the producer, so batch alignment stays
    /// deterministic even though embedding runs ahead of chunking. Under
    /// `--no-embeddings` every chunk carries an explicitly absent vector
    /// (REQ-MC-004) and the pipeline does not fail.
    async fn embed_and_build(
        &self,
        ctx: &TenantContext,
        pending: &[(ChunkId, String)],
        spec: &StageSpec,
    ) -> Result<Vec<MemoryChunk>, DomainError> {
        let texts: Vec<String> = pending.iter().map(|(_, text)| text.clone()).collect();
        let vectors = self.embed_batch(&texts).await?;
        Ok(pending
            .iter()
            .zip(vectors)
            .map(|((id, text), vector)| {
                let id = *id;
                MemoryChunk {
                    id,
                    tenant_id: *ctx.tenant_id(),
                    workspace_id: *ctx.workspace_id(),
                    agent_id: ctx.agent_id().clone(),
                    doc_id: spec.doc_id,
                    text: text.clone(),
                    vector,
                    created_at: spec.created_at,
                    // REQ-MC-006: complete provenance at write, matching the
                    // execution context.
                    provenance: Provenance {
                        source: spec.source.clone(),
                        doc_id: spec.doc_id,
                        chunk_id: id,
                        created_at: spec.created_at,
                        embedding_model_version: embedding_model_version().to_string(),
                        tenant_id: *ctx.tenant_id(),
                        workspace_id: *ctx.workspace_id(),
                        agent_id: ctx.agent_id().clone(),
                    },
                }
            })
            .collect())
    }
}

/// Everything `stage_chunks` needs besides the chunked texts.
struct StageSpec {
    doc_id: DocId,
    source: SourceKind,
    title: Option<String>,
    content_hash: String,
    created_at: chrono::DateTime<chrono::Utc>,
    chore_id: ChoreId,
}

/// Metadata → docs title: only `metadata["title"]` is honored (the rest of
/// the metadata map is not persisted in the MVP).
fn title_of(metadata: Option<&memento_ports::Metadata>) -> Option<String> {
    metadata
        .and_then(|m| m.0.get("title"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// Stable label for audit targets (`text`, `markdown`, `document:docx`).
fn source_label(source: &SourceKind) -> String {
    match source {
        SourceKind::Text => "text".to_string(),
        SourceKind::Markdown => "markdown".to_string(),
        SourceKind::Document(ext) => format!("document:{ext}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{
        test_app, test_app_failing_embed, test_app_failing_parse, test_app_no_embed,
    };
    use memento_domain::SourceKind;
    use memento_ports::{Metadata, SearchPort, SearchQuery};
    use memento_testkit::{TempStore, TestClock, spanish_corpus};
    use serde_json::json;

    fn text_request(text: &str) -> IngestTextRequest {
        IngestTextRequest {
            text: text.to_string(),
            doc_id: None,
            metadata: None,
        }
    }

    async fn ingest_one(ts: &TempStore, text: &str) -> IngestResult {
        let clock = TestClock::default();
        let app = test_app(ts, clock).await;
        app.ingest_text(&ts.ctx(), text_request(text))
            .await
            .expect("ingest ok")
    }

    #[tokio::test]
    async fn short_text_ingests_one_chunk_with_chore_and_provenance() {
        // REQ-MC-001 scenario 1: ~200-token Spanish text → one chunk id,
        // a chore id, and the chunk is searchable (REQ-MR-001).
        let ts = TempStore::new();
        let result = ingest_one(&ts, &spanish_corpus().join(" ")).await;
        assert_eq!(result.chunk_ids.len(), 1, "short text → single chunk");
        assert!(
            result.chore_id.is_some(),
            "chore id observable (REQ-MC-007)"
        );

        let clock = TestClock::default();
        let app = test_app(&ts, clock).await;
        let chunk = app
            .store()
            .get_chunk(&ts.ctx(), &result.chunk_ids[0])
            .await
            .expect("read back")
            .expect("chunk exists");
        assert_eq!(chunk.doc_id, result.doc_id);
        // REQ-MC-006: complete provenance matching the execution context.
        assert_eq!(chunk.provenance.tenant_id, *ts.tenant_id());
        assert_eq!(chunk.provenance.workspace_id, *ts.workspace_id());
        assert_eq!(chunk.provenance.agent_id, *ts.agent_id());
        assert_eq!(chunk.provenance.source, SourceKind::Text);
        assert_eq!(chunk.provenance.chunk_id, chunk.id);
        assert!(!chunk.provenance.embedding_model_version.is_empty());
        // REQ-MC-004: vector populated from day 1 (stub embedder).
        assert_eq!(chunk.vector.as_ref().expect("vector present").len(), 768);

        // Searchable immediately (atomic visibility, REQ-MC-007).
        let hits = app
            .store()
            .search(
                &ts.ctx(),
                SearchQuery::new("memoria", 5, *ts.workspace_id()),
            )
            .await
            .expect("search ok");
        assert!(!hits.is_empty(), "chunk searchable after ingest");
    }

    #[tokio::test]
    async fn empty_and_whitespace_text_rejected_with_nothing_stored() {
        // REQ-MC-001 scenario 2: structured validation error, zero writes.
        let ts = TempStore::new();
        let clock = TestClock::default();
        let app = test_app(&ts, clock).await;
        let ctx = ts.ctx();

        for bad in ["", "   ", "\n\t  "] {
            let err = app
                .ingest_text(&ctx, text_request(bad))
                .await
                .expect_err("rejected");
            assert_eq!(err.code(), "INVALID_INPUT", "for {bad:?}");
        }
        assert_eq!(app.store().count_chunks(&ctx).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn duplicate_ingest_returns_existing_ids() {
        // REQ-MC-005 scenario 1: re-ingesting identical text creates no new
        // chunks and the response references the existing records.
        let ts = TempStore::new();
        let text = spanish_corpus().join(" ");
        let first = ingest_one(&ts, &text).await;
        let second = ingest_one(&ts, &text).await;

        let clock = TestClock::default();
        let app = test_app(&ts, clock).await;
        assert_eq!(app.store().count_chunks(&ts.ctx()).await.unwrap(), 1);
        assert_eq!(first.chunk_ids, second.chunk_ids, "same ids returned");
        assert_eq!(first.doc_id, second.doc_id, "same doc referenced");
        assert!(first.chore_id.is_some() && second.chore_id.is_some());
    }

    #[tokio::test]
    async fn duplicate_with_explicit_doc_id_still_dedups() {
        // The probe is content-based: a different doc_id on the same content
        // must NOT create a new copy.
        let ts = TempStore::new();
        let text = spanish_corpus().join(" ");
        let first = ingest_one(&ts, &text).await;
        let clock = TestClock::default();
        let app = test_app(&ts, clock).await;
        let second = app
            .ingest_text(
                &ts.ctx(),
                IngestTextRequest {
                    text,
                    doc_id: Some(DocId::new()),
                    metadata: None,
                },
            )
            .await
            .expect("ingest ok");
        assert_eq!(first.chunk_ids, second.chunk_ids);
        assert_eq!(app.store().count_chunks(&ts.ctx()).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn same_content_different_tenant_is_independent() {
        // REQ-MC-005 scenario 2: identical content in T2 creates an
        // independent copy — no cross-tenant dedup.
        let ts1 = TempStore::new();
        let ts2 = TempStore::new();
        let text = spanish_corpus().join(" ");
        let r1 = ingest_one(&ts1, &text).await;
        let r2 = ingest_one(&ts2, &text).await;

        let clock = TestClock::default();
        let app2 = test_app(&ts2, clock).await;
        assert_eq!(app2.store().count_chunks(&ts2.ctx()).await.unwrap(), 1);
        assert_ne!(r1.chunk_ids, r2.chunk_ids, "independent copies");
        // The same content in T2 is fully searchable there.
        let hits = app2
            .store()
            .search(
                &ts2.ctx(),
                SearchQuery::new("memoria", 5, *ts2.workspace_id()),
            )
            .await
            .expect("search ok");
        assert!(!hits.is_empty());
    }

    #[tokio::test]
    async fn document_ingest_runs_the_real_fallback_boundary() {
        // REQ-MC-002: a Markdown blob normalizes through the real fallback
        // parser (no subprocess), chunked, stored, source recorded.
        let ts = TempStore::new();
        let clock = TestClock::default();
        let app = test_app(&ts, clock).await;
        let markdown = format!("# Notas\n\n{}", spanish_corpus().join(" "));

        let result = app
            .ingest_document(
                &ts.ctx(),
                IngestDocumentRequest {
                    blob: markdown.as_bytes().to_vec(),
                    source_hint: SourceKind::Markdown,
                    doc_id: None,
                    metadata: Some(Metadata(
                        json!({"title": "Notas de prueba"})
                            .as_object()
                            .unwrap()
                            .clone(),
                    )),
                },
            )
            .await
            .expect("ingest ok");

        let chunk = app
            .store()
            .get_chunk(&ts.ctx(), &result.chunk_ids[0])
            .await
            .expect("read")
            .expect("chunk");
        assert_eq!(chunk.provenance.source, SourceKind::Markdown);
        assert!(chunk.text.contains("memoria"), "markdown content chunked");

        // The docs row carried the title through metadata.
        let docs = memento_lancedb::all_docs(app.store(), &ts.ctx())
            .await
            .expect("docs");
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].title.as_deref(), Some("Notas de prueba"));
    }

    #[tokio::test]
    async fn mid_normalization_failure_leaves_zero_visible_chunks() {
        // REQ-MC-007: a failing parse → structured stage-named error and
        // zero chunks visible anywhere.
        let ts = TempStore::new();
        let app = test_app_failing_parse(&ts).await;
        let err = app
            .ingest_document(
                &ts.ctx(),
                IngestDocumentRequest {
                    blob: b"corrupt".to_vec(),
                    source_hint: SourceKind::Document("docx".into()),
                    doc_id: None,
                    metadata: None,
                },
            )
            .await
            .expect_err("parse fails");
        assert_eq!(err.code(), "PARSE");
        assert_eq!(app.store().count_chunks(&ts.ctx()).await.unwrap(), 0);
        // Nothing searchable either.
        let hits = app
            .store()
            .search(
                &ts.ctx(),
                SearchQuery::new("memoria", 5, *ts.workspace_id()),
            )
            .await
            .expect("search ok");
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn embed_failure_leaves_zero_visible_chunks() {
        // REQ-MC-007 variant: the embed stage fails → structured error,
        // zero writes (chunks are only added after embedding).
        let ts = TempStore::new();
        let app = test_app_failing_embed(&ts).await;
        let err = app
            .ingest_text(&ts.ctx(), text_request("la memoria es un río."))
            .await
            .expect_err("embed fails");
        assert_eq!(err.code(), "EMBEDDING_FAILED");
        assert_eq!(app.store().count_chunks(&ts.ctx()).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn no_embeddings_mode_stores_searchable_chunks_without_vectors() {
        // REQ-MC-004: --no-embeddings → chunks stored, FTS-searchable,
        // vectors explicitly absent, pipeline does not fail.
        let ts = TempStore::new();
        let app = test_app_no_embed(&ts).await;
        let result = app
            .ingest_text(&ts.ctx(), text_request(&spanish_corpus().join(" ")))
            .await
            .expect("ingest ok without embeddings");

        let chunk = app
            .store()
            .get_chunk(&ts.ctx(), &result.chunk_ids[0])
            .await
            .expect("read")
            .expect("chunk");
        assert!(chunk.vector.is_none(), "explicitly absent vector");
        let hits = app
            .store()
            .search(
                &ts.ctx(),
                SearchQuery::new("memoria", 5, *ts.workspace_id()),
            )
            .await
            .expect("FTS works");
        assert!(!hits.is_empty());
    }

    #[tokio::test]
    async fn oversized_document_blob_is_rejected() {
        let ts = TempStore::new();
        let clock = TestClock::default();
        let app = test_app(&ts, clock).await;
        let blob = vec![b'x'; (MAX_BLOB_BYTES + 1) as usize];
        let err = app
            .ingest_document(
                &ts.ctx(),
                IngestDocumentRequest {
                    blob,
                    source_hint: SourceKind::Markdown,
                    doc_id: None,
                    metadata: None,
                },
            )
            .await
            .expect_err("too big");
        assert_eq!(err.code(), "QUOTA_EXCEEDED");
        assert_eq!(app.store().count_chunks(&ts.ctx()).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn chunk_overflow_is_rejected_before_embedding() {
        // Design MC Q4: >10k chunks/doc → QUOTA_EXCEEDED with zero writes.
        // The sentence ≈ 12-13 Spanish tokens; 250k repeats ≈ 3.1M tokens
        // ≈ 11.5k chunks — safely over the 10k limit. The blob limit does
        // not apply (it guards documents; text has no byte cap).
        let ts = TempStore::new();
        let clock = TestClock::default();
        let app = test_app(&ts, clock).await;
        let sentence = "la memoria es un río subterráneo que nunca deja de fluir. ";
        let text = sentence.repeat(250_000);

        let err = app
            .ingest_text(&ts.ctx(), text_request(&text))
            .await
            .expect_err("too many chunks");
        assert_eq!(err.code(), "QUOTA_EXCEEDED");
        assert_eq!(app.store().count_chunks(&ts.ctx()).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn ingest_stamps_injectable_clock_time() {
        // The retention sweep (T-063) measures age from created_at, so the
        // ingest timestamp must come from the injectable clock (design D5).
        let ts = TempStore::new();
        let start = chrono::Utc::now() - chrono::Duration::days(40);
        let clock = TestClock::new(start);
        let app = test_app(&ts, clock).await;
        let result = app
            .ingest_text(&ts.ctx(), text_request("recuerdo antiguo que debe expirar"))
            .await
            .expect("ingest ok");
        let chunk = app
            .store()
            .get_chunk(&ts.ctx(), &result.chunk_ids[0])
            .await
            .expect("read")
            .expect("chunk");
        assert_eq!(chunk.created_at, start, "clock stamped into provenance");
        assert_eq!(chunk.provenance.created_at, start);
    }

    #[tokio::test]
    async fn embed_batches_at_64() {
        // Design step 5: the app batches embeddings at EMBED_BATCH (the
        // fastembed adapter errors on oversized batches). Ingesting text
        // that yields >64 chunks exercises several batches.
        let ts = TempStore::new();
        let clock = TestClock::default();
        let app = test_app(&ts, clock).await;
        let sentence = "la memoria es un río subterráneo que nunca deja de fluir. ";
        let text = sentence.repeat(3_000);
        let result = app
            .ingest_text(&ts.ctx(), text_request(&text))
            .await
            .expect("ingest ok");
        assert!(result.chunk_ids.len() > 64, "fixture spans batches");
        // Every chunk carries a 768-d vector (stub) — batch boundaries did
        // not corrupt alignment.
        for id in result.chunk_ids.iter().take(70) {
            let chunk = app
                .store()
                .get_chunk(&ts.ctx(), id)
                .await
                .expect("read")
                .expect("exists");
            assert_eq!(chunk.vector.expect("vector").len(), 768);
        }
    }

    #[tokio::test]
    async fn ingest_is_observable_in_audit() {
        let ts = TempStore::new();
        let clock = TestClock::default();
        let app = test_app(&ts, clock).await;
        app.ingest_text(&ts.ctx(), text_request("auditoría de ingesta"))
            .await
            .expect("ok");
        let raw = std::fs::read_to_string(app.audit_log_path()).expect("audit file");
        let line = raw.lines().next().expect("one line");
        let value: serde_json::Value = serde_json::from_str(line).expect("json");
        assert_eq!(value["action"], "ingest");
        assert_eq!(value["outcome"], "ok");
        assert_eq!(value["target"]["duplicate"], false);
        // Content never enters the audit: the ingested text is absent.
        assert!(!raw.contains("auditoría"), "no content in audit: {raw}");
    }

    #[tokio::test]
    async fn pipeline_streams_chunks_without_loss_or_reordering() {
        // B2: the chunk → embed pipeline must reproduce the exact chunker
        // output — deterministic boundaries, production order preserved —
        // for a document large enough to span several embed batches.
        let ts = TempStore::new();
        let clock = TestClock::default();
        let app = test_app(&ts, clock).await;
        let sentence = "la memoria es un río subterráneo que nunca deja de fluir. ";
        let text = sentence.repeat(2_000);
        let expected = app.chunker.chunk(&text);
        assert!(expected.len() > 64, "fixture spans embed batches");

        let result = app
            .ingest_text(&ts.ctx(), text_request(&text))
            .await
            .expect("ingest ok");
        assert_eq!(result.chunk_ids.len(), expected.len());

        let mut actual: Vec<String> = Vec::with_capacity(expected.len());
        for id in &result.chunk_ids {
            let chunk = app
                .store()
                .get_chunk(&ts.ctx(), id)
                .await
                .expect("read")
                .expect("exists");
            actual.push(chunk.text);
        }
        let expected_texts: Vec<String> = expected.iter().map(|c| c.text.clone()).collect();
        assert_eq!(
            actual, expected_texts,
            "streaming chunking is deterministic and ordered"
        );
    }

    #[tokio::test]
    async fn metrics_ingest_records_requests_chunks_dedup_and_duration() {
        // REQ-OBS-006: with MEMENTO_METRICS=1 an ingest records its request
        // counter, the produced chunk count, and the pipeline latency; a
        // duplicate ingest records the dedup hit (REQ-MC-005) instead of
        // re-running the pipeline.
        let _guard = crate::test_util::METRICS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // SAFETY: test-only env mutation, serialized by METRICS_ENV_LOCK.
        unsafe { std::env::set_var("MEMENTO_METRICS", "1") };
        let ts = TempStore::new();
        let clock = TestClock::default();
        let app = test_app(&ts, clock).await;
        let tenant = ts.tenant_id().to_string();
        let text = spanish_corpus().join(" ");

        app.ingest_text(&ts.ctx(), text_request(&text))
            .await
            .expect("ingest");
        app.ingest_text(&ts.ctx(), text_request(&text))
            .await
            .expect("dedup");

        let render = memento_observability::metrics::render();
        assert!(
            render.contains(&format!(
                "memento_ingest_requests_total{{tenant_id=\"{tenant}\"}} 2"
            )),
            "both ingests recorded: {render}"
        );
        assert!(
            render.contains(&format!(
                "memento_ingest_dedup_hits_total{{tenant_id=\"{tenant}\"}} 1"
            )),
            "second ingest is a dedup hit: {render}"
        );
        assert!(
            render.contains(&format!(
                "memento_ingest_duration_ms_count{{tenant_id=\"{tenant}\"}} 1"
            )),
            "pipeline latency observed once (dedup skips the pipeline): {render}"
        );
        let chunks = crate::test_util::metric_value(
            &render,
            &format!("memento_ingest_chunks_total{{tenant_id=\"{tenant}\"}}"),
        )
        .expect("chunk-count line present");
        assert!(
            chunks >= 1,
            "produced chunks counted (got {chunks}): {render}"
        );
        // SAFETY: test-only env mutation, serialized by METRICS_ENV_LOCK.
        unsafe { std::env::remove_var("MEMENTO_METRICS") };
    }
}
