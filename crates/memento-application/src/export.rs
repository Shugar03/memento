//! Tenant export (T-065, REQ-CG-005): ALL tenant data in an open, documented
//! format — JSONL streams packed into a `tar.gz` artifact.
//!
//! # Artifact layout (`<root>/exports/<tid>-<ts>.tar.gz`)
//!
//! ```text
//! manifest.json     {"schema_version":1,"tenant_id":...,"exported_at":...,
//!                    "counts":{"chunks":N,"docs":M,"feedback":K}}
//! chunks.jsonl      one object per chunk (full text + vector + provenance)
//! docs.jsonl        one object per docs-table row (ingest metadata + hash)
//! feedback.jsonl    one object per feedback signal (attribution included)
//! config.toml       the tenant's raw config file (or absent when none)
//! ```
//!
//! Chunk line schema (documented contract): `{chunk_id, doc_id, text,
//! vector (null when absent), created_at, provenance{tenant_id,
//! workspace_id, agent_id, source, doc_id, chunk_id, created_at,
//! embedding_model_version}}`. Re-import is OUT of scope (spec non-goal);
//! the artifact is for the data subject's portability request.

use crate::AppService;
use memento_domain::{DomainError, MemoryChunk, TenantContext};
use memento_lancedb::{all_chunks, all_docs, all_feedback};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Export artifact schema version (documented; parsed by consumers).
pub const EXPORT_SCHEMA_VERSION: u32 = 1;

/// Outcome of an export.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportReport {
    /// Absolute path of the `.tar.gz` artifact.
    pub path: std::path::PathBuf,
    pub chunk_count: usize,
    pub feedback_count: usize,
    pub exported_at: chrono::DateTime<chrono::Utc>,
}

impl AppService {
    /// Export ALL tenant data (chunks + provenance + feedback + config) as
    /// JSONL in a tar.gz artifact (REQ-CG-005). The event is audited; the
    /// artifact contains no credentials and no key material.
    ///
    /// # Errors
    ///
    /// * `Io` — the artifact cannot be written.
    pub async fn export_tenant(&self, ctx: &TenantContext) -> Result<ExportReport, DomainError> {
        self.ensure_bound_tenant(ctx)?;
        let exported_at = self.clock.now();

        let chunks = all_chunks(&self.store, ctx).await?;
        let docs = all_docs(&self.store, ctx).await?;
        let feedback = all_feedback(&self.store, ctx).await?;

        let mut chunks_jsonl = String::new();
        for chunk in &chunks {
            chunks_jsonl.push_str(&serde_json::to_string(&chunk_line(chunk)).map_err(|err| {
                DomainError::Internal {
                    message: format!("serialize chunk export: {err}"),
                }
            })?);
            chunks_jsonl.push('\n');
        }
        let mut docs_jsonl = String::new();
        for doc in &docs {
            docs_jsonl.push_str(
                &serde_json::to_string(&json!({
                    "doc_id": doc.doc_id,
                    "tenant_id": doc.tenant_id,
                    "workspace_id": doc.workspace_id,
                    "agent_id": doc.agent_id,
                    "title": doc.title,
                    "source": doc.source,
                    "created_at": doc.created_at,
                    "content_hash": doc.content_hash,
                }))
                .map_err(|err| DomainError::Internal {
                    message: format!("serialize doc export: {err}"),
                })?,
            );
            docs_jsonl.push('\n');
        }
        let mut feedback_jsonl = String::new();
        for row in &feedback {
            feedback_jsonl.push_str(
                &serde_json::to_string(&json!({
                    "chunk_id": row.chunk_id,
                    "tenant_id": row.tenant_id,
                    "workspace_id": row.workspace_id,
                    "agent_id": row.agent_id,
                    "score": row.score,
                    "comment": row.comment,
                    "created_at": row.created_at,
                }))
                .map_err(|err| DomainError::Internal {
                    message: format!("serialize feedback export: {err}"),
                })?,
            );
            feedback_jsonl.push('\n');
        }

        let manifest = json!({
            "schema_version": EXPORT_SCHEMA_VERSION,
            "tenant_id": ctx.tenant_id(),
            "exported_at": exported_at,
            "counts": {
                "chunks": chunks.len(),
                "docs": docs.len(),
                "feedback": feedback.len(),
            },
        });
        let config_raw = std::fs::read_to_string(crate::tenant_config::tenant_config_path(
            &self.root,
            ctx.tenant_id(),
        ))
        .ok();

        // tar.gz in memory (bounded by tenant data; MVP sizes).
        let tar_bytes = build_export_archive(
            &manifest,
            &chunks_jsonl,
            &docs_jsonl,
            &feedback_jsonl,
            config_raw.as_deref(),
        )?;

        let ts = exported_at.format("%Y%m%dT%H%M%SZ");
        let exports_dir = self.root.join("exports");
        std::fs::create_dir_all(&exports_dir)?;
        let path = exports_dir.join(format!("{}-{ts}.tar.gz", ctx.tenant_id()));
        std::fs::write(&path, &tar_bytes)?;

        self.record_audit(
            ctx,
            "export",
            json!({
                "artifact": path.file_name().map(|n| n.to_string_lossy().to_string()),
                "chunk_count": chunks.len(),
                "feedback_count": feedback.len(),
            }),
            None,
        );
        Ok(ExportReport {
            path,
            chunk_count: chunks.len(),
            feedback_count: feedback.len(),
            exported_at,
        })
    }
}

/// One documented chunk line: full content + provenance (REQ-CG-005 schema).
fn chunk_line(chunk: &MemoryChunk) -> Value {
    json!({
        "chunk_id": chunk.id,
        "doc_id": chunk.doc_id,
        "text": chunk.text,
        "vector": chunk.vector,
        "created_at": chunk.created_at,
        "provenance": {
            "tenant_id": chunk.provenance.tenant_id,
            "workspace_id": chunk.provenance.workspace_id,
            "agent_id": chunk.provenance.agent_id,
            "source": chunk.provenance.source,
            "doc_id": chunk.provenance.doc_id,
            "chunk_id": chunk.provenance.chunk_id,
            "created_at": chunk.provenance.created_at,
            "embedding_model_version": chunk.provenance.embedding_model_version,
        },
    })
}

/// Pack the export payload into a gzipped tar.
fn build_export_archive(
    manifest: &Value,
    chunks_jsonl: &str,
    docs_jsonl: &str,
    feedback_jsonl: &str,
    config_raw: Option<&str>,
) -> Result<Vec<u8>, DomainError> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tar::{Builder, Header};

    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut builder = Builder::new(encoder);

    fn add_text(
        builder: &mut Builder<GzEncoder<Vec<u8>>>,
        name: &str,
        content: &str,
    ) -> Result<(), DomainError> {
        let mut header = Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, name, content.as_bytes())
            .map_err(|err| DomainError::Io { source: err })
    }

    add_text(&mut builder, "manifest.json", &manifest.to_string())?;
    add_text(&mut builder, "chunks.jsonl", chunks_jsonl)?;
    add_text(&mut builder, "docs.jsonl", docs_jsonl)?;
    add_text(&mut builder, "feedback.jsonl", feedback_jsonl)?;
    if let Some(config) = config_raw {
        add_text(&mut builder, "config.toml", config)?;
    }
    builder
        .finish()
        .map_err(|err| DomainError::Io { source: err })?;
    let encoder = builder
        .into_inner()
        .map_err(|err| DomainError::Io { source: err })?;
    encoder
        .finish()
        .map_err(|err| DomainError::Io { source: err })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::test_app;
    use memento_ports::{IngestTextRequest, SearchPort, SearchQuery};
    use memento_testkit::{TempStore, TestClock};
    use std::io::Read;

    #[tokio::test]
    async fn export_contains_all_chunks_with_full_provenance() {
        // REQ-CG-005 scenario: the artifact parses against the documented
        // schema (manifest + chunks.jsonl with provenance).
        let ts = TempStore::new();
        let clock = TestClock::default();
        let app = test_app(&ts, clock).await;
        app.ingest_text(
            &ts.ctx(),
            IngestTextRequest {
                text: "la memoria exportada viaja con su procedencia completa".into(),
                doc_id: None,
                metadata: None,
            },
        )
        .await
        .expect("ingest");
        app.feedback(
            &ts.ctx(),
            app.store()
                .search(
                    &ts.ctx(),
                    SearchQuery::new("memoria", 5, *ts.workspace_id()),
                )
                .await
                .expect("search")[0]
                .chunk_id,
            true,
            None,
        )
        .await
        .expect("feedback");

        let report = app.export_tenant(&ts.ctx()).await.expect("export");
        assert_eq!(report.chunk_count, 1);
        assert_eq!(report.feedback_count, 1);
        assert!(report.path.exists());

        // Unpack and validate the documented schema.
        let gz = std::fs::File::open(&report.path).expect("open artifact");
        let mut decoder = flate2::read::GzDecoder::new(gz);
        let mut tar_bytes = Vec::new();
        decoder.read_to_end(&mut tar_bytes).expect("gunzip");
        let mut archive = tar::Archive::new(&tar_bytes[..]);
        let mut seen = std::collections::HashMap::new();
        for entry in archive.entries().expect("entries") {
            let mut entry = entry.expect("entry");
            let name = entry.path().expect("name").to_string_lossy().to_string();
            let mut content = String::new();
            entry.read_to_string(&mut content).expect("read");
            seen.insert(name, content);
        }

        let manifest: Value = serde_json::from_str(&seen["manifest.json"]).expect("manifest");
        assert_eq!(manifest["schema_version"], EXPORT_SCHEMA_VERSION);
        assert_eq!(manifest["counts"]["chunks"], 1);

        let line: Value = serde_json::from_str(seen["chunks.jsonl"].lines().next().expect("line"))
            .expect("chunk line");
        assert_eq!(line["provenance"]["tenant_id"], ts.tenant_id().to_string());
        assert_eq!(line["provenance"]["chunk_id"], line["chunk_id"]);
        assert!(line["provenance"]["embedding_model_version"].is_string());
        assert_eq!(
            line["vector"].as_array().map(|v| v.len()),
            Some(768),
            "vector exported"
        );
        assert!(
            seen["chunks.jsonl"].contains("viaja con su procedencia"),
            "text exported"
        );
    }

    #[tokio::test]
    async fn export_is_audited() {
        let ts = TempStore::new();
        let clock = TestClock::default();
        let app = test_app(&ts, clock).await;
        app.export_tenant(&ts.ctx()).await.expect("empty export");
        let raw = std::fs::read_to_string(app.audit_log_path()).expect("audit");
        let lines: Vec<Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).expect("json"))
            .collect();
        assert!(
            lines
                .iter()
                .any(|l| l["action"] == "export" && l["outcome"] == "ok")
        );
        assert_eq!(lines[0]["target"]["chunk_count"], 0);
    }

    #[tokio::test]
    async fn export_never_contains_credentials_or_key_material() {
        let ts = TempStore::new();
        let clock = TestClock::default();
        let app = test_app(&ts, clock).await;
        app.ingest_text(
            &ts.ctx(),
            IngestTextRequest {
                text: "la memoria no viaja con secretos memo_abc123".into(),
                doc_id: None,
                metadata: None,
            },
        )
        .await
        .expect("ingest");
        let report = app.export_tenant(&ts.ctx()).await.expect("export");

        let gz = std::fs::File::open(&report.path).expect("open");
        let mut decoder = flate2::read::GzDecoder::new(gz);
        let mut tar_bytes = Vec::new();
        decoder.read_to_end(&mut tar_bytes).expect("gunzip");
        let raw = String::from_utf8(tar_bytes).expect("utf8");
        // The artifact never contains key material or credential files.
        assert!(!raw.contains("master.key"), "no key material in export");
        assert!(
            !raw.contains("credentials.toml"),
            "no credentials in export"
        );
        assert!(!raw.contains("auth/"), "no auth dir in export");
    }
}
