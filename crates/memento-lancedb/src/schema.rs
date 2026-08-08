//! LanceDB schema: per-tenant tables and type-safe scope expressions (T-020).
//!
//! Every tenant gets its own database directory (design D8), and every row in
//! every table also carries `tenant_id`/`workspace_id` columns so that all
//! queries are scoped by construction. The scope helpers below build the
//! DataFusion expressions used for filtering, deleting and counting.
//!
//! # Storage layout (D8)
//!
//! ```text
//! <root>/db/tenants/<tenant_id>/lancedb/
//! ├── chunks    — memory chunks (+ provenance fields, vector, FTS text)
//! ├── docs      — ingested document metadata
//! ├── feedback  — per-chunk feedback (REQ-ML-001)
//! └── symbols   — code-knowledge symbol mirror (batch 5, T-041)
//! ```

use chrono::{DateTime, Utc};
use datafusion_common::ScalarValue;
use lancedb::arrow::arrow_schema::{DataType, Field, Schema, SchemaRef, TimeUnit};
use lancedb::expr::{DfExpr, col, lit};
use memento_domain::{TenantId, WorkspaceId};
use std::sync::Arc;

// --- table names -------------------------------------------------------------

/// Memory chunks (the hot store; FTS + vector indexes live here).
pub const CHUNKS: &str = "chunks";
/// Ingested document metadata.
pub const DOCS: &str = "docs";
/// Per-chunk feedback (REQ-ML-001).
pub const FEEDBACK: &str = "feedback";
/// Code-knowledge symbol mirror (REQ-CK-*).
pub const SYMBOLS: &str = "symbols";
/// All tenant tables, in creation order.
pub const ALL_TABLES: [&str; 4] = [CHUNKS, DOCS, FEEDBACK, SYMBOLS];

// --- column names ------------------------------------------------------------

pub const COL_CHUNK_ID: &str = "chunk_id";
pub const COL_TENANT_ID: &str = "tenant_id";
pub const COL_WORKSPACE_ID: &str = "workspace_id";
pub const COL_AGENT_ID: &str = "agent_id";
pub const COL_DOC_ID: &str = "doc_id";
pub const COL_TEXT: &str = "text";
pub const COL_SOURCE: &str = "source_json";
pub const COL_VECTOR: &str = "vector";
pub const COL_EMBEDDING_MODEL: &str = "embedding_model_version";
pub const COL_CREATED_AT: &str = "created_at";
pub const COL_TITLE: &str = "title";
pub const COL_SCORE: &str = "score";
pub const COL_COMMENT: &str = "comment";
pub const COL_SYMBOL_NAME: &str = "symbol_name";
pub const COL_PROJECT_ID: &str = "project_id";
pub const COL_KIND: &str = "kind";
pub const COL_LOCATION: &str = "location";
pub const COL_SIGNATURE: &str = "signature";
/// Content hash of the ingested input, tenant-scoped (REQ-MC-005 idempotency
/// probe; lives on the docs table so `MemoryChunk` stays domain-clean).
pub const COL_CONTENT_HASH: &str = "content_hash";

/// Embedding dimension for the chunks vector column (E5-small, 384d).
pub const EMBEDDING_DIM: usize = 384;

/// FTS index name over [`COL_TEXT`] (idempotent index creation).
pub const FTS_INDEX_NAME: &str = "chunks_text_fts";
/// Vector index name over [`COL_VECTOR`] (idempotent index creation).
pub const VECTOR_INDEX_NAME: &str = "chunks_vector_ivf";

// --- schema builders ----------------------------------------------------------

fn text() -> DataType {
    DataType::Utf8
}

fn ts() -> DataType {
    // UTC epoch-nanosecond timestamps; no timezone metadata (all values UTC).
    DataType::Timestamp(TimeUnit::Nanosecond, None)
}

fn vector_field() -> Field {
    Field::new(
        COL_VECTOR,
        DataType::FixedSizeList(
            Arc::new(Field::new("item", DataType::Float32, true)),
            EMBEDDING_DIM as i32,
        ),
        true,
    )
}

/// `chunks` table schema: every REQ-MC-006 provenance field is a column.
pub fn chunks_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new(COL_CHUNK_ID, text(), false),
        Field::new(COL_TENANT_ID, text(), false),
        Field::new(COL_WORKSPACE_ID, text(), false),
        Field::new(COL_AGENT_ID, text(), false),
        Field::new(COL_DOC_ID, text(), false),
        Field::new(COL_TEXT, text(), false),
        // serde_json of SourceKind (Text/Markdown/Document(ext)); searchable filter.
        Field::new(COL_SOURCE, text(), false),
        // `None` in --no-embeddings mode (REQ-MC-004).
        vector_field(),
        Field::new(COL_EMBEDDING_MODEL, text(), false),
        Field::new(COL_CREATED_AT, ts(), false),
    ]))
}

/// `docs` table schema: document metadata written by the ingest pipeline.
/// `content_hash` (REQ-MC-005) is the tenant-scoped idempotency key: the
/// dedup probe scans this column and re-ingests reference the stored rows.
pub fn docs_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new(COL_DOC_ID, text(), false),
        Field::new(COL_TENANT_ID, text(), false),
        Field::new(COL_WORKSPACE_ID, text(), false),
        Field::new(COL_AGENT_ID, text(), false),
        Field::new(COL_TITLE, text(), true),
        Field::new(COL_SOURCE, text(), false),
        Field::new(COL_CREATED_AT, ts(), false),
        Field::new(COL_CONTENT_HASH, text(), false),
    ]))
}

/// `feedback` table schema (REQ-ML-001: feedback persisted with attribution).
pub fn feedback_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new(COL_CHUNK_ID, text(), false),
        Field::new(COL_TENANT_ID, text(), false),
        Field::new(COL_WORKSPACE_ID, text(), false),
        Field::new(COL_AGENT_ID, text(), false),
        Field::new(COL_SCORE, DataType::Float32, false),
        Field::new(COL_COMMENT, text(), true),
        Field::new(COL_CREATED_AT, ts(), false),
    ]))
}

/// `symbols` table schema: LanceDB mirror of the L2 symbol map (T-041).
pub fn symbols_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new(COL_SYMBOL_NAME, text(), false),
        Field::new(COL_TENANT_ID, text(), false),
        Field::new(COL_PROJECT_ID, text(), false),
        Field::new(COL_KIND, text(), false),
        Field::new(COL_LOCATION, text(), false),
        Field::new(COL_SIGNATURE, text(), true),
        Field::new(COL_CREATED_AT, ts(), false),
    ]))
}

/// Look up the schema for a table name.
pub fn schema_for(table: &str) -> Option<SchemaRef> {
    match table {
        CHUNKS => Some(chunks_schema()),
        DOCS => Some(docs_schema()),
        FEEDBACK => Some(feedback_schema()),
        SYMBOLS => Some(symbols_schema()),
        _ => None,
    }
}

// --- tenant-scoped expressions -------------------------------------------------

/// `tenant_id = <id>` (the process-bound tenant, REQ-TA-001/002).
pub fn tenant_scope(tenant_id: &TenantId) -> DfExpr {
    col(COL_TENANT_ID).eq(lit(tenant_id.to_string()))
}

/// `workspace_id = <id>` (mandatory workspace filter, REQ-MR-006).
pub fn workspace_scope(workspace_id: &WorkspaceId) -> DfExpr {
    col(COL_WORKSPACE_ID).eq(lit(workspace_id.to_string()))
}

/// Combined chunks scope: tenant AND workspace (the default retrieval window).
pub fn chunks_scope(tenant_id: &TenantId, workspace_id: &WorkspaceId) -> DfExpr {
    tenant_scope(tenant_id).and(workspace_scope(workspace_id))
}

/// `created_at < cutoff` (retention sweep boundary, REQ-ML-003).
pub fn created_before(cutoff: DateTime<Utc>) -> DfExpr {
    let ns = cutoff.timestamp_nanos_opt().expect("timestamp in range");
    col(COL_CREATED_AT).lt(lit(ScalarValue::TimestampNanosecond(Some(ns), None)))
}

// --- timestamp conversions ------------------------------------------------------

/// `DateTime<Utc>` → epoch nanoseconds (storage representation).
pub fn ts_to_nanos(dt: DateTime<Utc>) -> i64 {
    dt.timestamp_nanos_opt().expect("timestamp in range")
}

/// Epoch nanoseconds → `DateTime<Utc>` (storage representation).
pub fn nanos_to_ts(ns: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(
        ns.div_euclid(1_000_000_000),
        ns.rem_euclid(1_000_000_000) as u32,
    )
    .expect("valid timestamp")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lancedb::expr::expr_to_sql_string;

    #[test]
    fn scope_exprs_encode_tenant_and_workspace() {
        let tenant = TenantId::new();
        let workspace = WorkspaceId::new();

        let t_sql = expr_to_sql_string(&tenant_scope(&tenant)).expect("sql");
        assert!(
            t_sql.contains("tenant_id") && t_sql.contains(&tenant.to_string()),
            "tenant scope: {t_sql}"
        );

        let w_sql = expr_to_sql_string(&workspace_scope(&workspace)).expect("sql");
        assert!(
            w_sql.contains("workspace_id") && w_sql.contains(&workspace.to_string()),
            "workspace scope: {w_sql}"
        );

        let both = expr_to_sql_string(&chunks_scope(&tenant, &workspace)).expect("sql");
        assert!(both.contains("AND"), "scope conjunction: {both}");
        assert!(both.contains(&tenant.to_string()) && both.contains(&workspace.to_string()));
    }

    #[test]
    fn created_before_encodes_cutoff() {
        let cutoff = Utc::now();
        let sql = expr_to_sql_string(&created_before(cutoff)).expect("sql");
        assert!(sql.contains("created_at"), "cutoff predicate: {sql}");
    }

    #[test]
    fn every_table_has_a_schema() {
        for table in ALL_TABLES {
            assert!(schema_for(table).is_some(), "{table} schema missing");
        }
        assert!(schema_for("nope").is_none());
    }

    #[test]
    fn chunks_schema_has_provenance_columns() {
        let schema = chunks_schema();
        for col in [
            COL_CHUNK_ID,
            COL_TENANT_ID,
            COL_WORKSPACE_ID,
            COL_AGENT_ID,
            COL_DOC_ID,
            COL_TEXT,
            COL_SOURCE,
            COL_VECTOR,
            COL_EMBEDDING_MODEL,
            COL_CREATED_AT,
        ] {
            assert!(schema.field_with_name(col).is_ok(), "column {col} missing");
        }
    }

    #[test]
    fn timestamp_round_trip() {
        let now = Utc::now();
        assert_eq!(nanos_to_ts(ts_to_nanos(now)), now);
    }
}
