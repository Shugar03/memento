//! LanceStore: per-tenant connection bootstrap and schema management (T-020).
//!
//! One [`LanceStore`] instance is bound to exactly one tenant directory
//! (design D8): `open` resolves `<root>/db/tenants/<tenant_id>/lancedb`, and
//! every later call re-validates the passed context against the bound tenant
//! (`TENANT_FORBIDDEN` on mismatch — defense in depth on top of the directory
//! boundary). Workspaces are NOT bound: they are per-call parameters
//! (mandatory workspace filter, REQ-MR-006).

use crate::schema::{ALL_TABLES, COL_TENANT_ID, COL_WORKSPACE_ID, schema_for};
use lancedb::arrow::arrow_array::{
    Array, RecordBatch,
    cast::AsArray,
    types::{Float32Type, TimestampNanosecondType},
};
use lancedb::connection::Connection;
use lancedb::table::Table;
use memento_domain::{
    AgentId, ChunkId, DocId, DomainError, MemoryChunk, Provenance, SourceKind, TenantContext,
    TenantId, WorkspaceId,
};
use memento_observability::EventSink;
use memento_ports::SearchHit;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// LanceDB storage adapter bound to one tenant directory.
pub struct LanceStore {
    connection: Connection,
    root: PathBuf,
    tenant_id: TenantId,
    /// The tenant's operational event sink (REQ-OBS-008, design D5): shared
    /// with the application via `with_events` so adapter-side events
    /// (`fts_build`) land in the same `logs/<tid>.events.jsonl` file.
    /// `None` when events are off (zero I/O).
    events: Option<Arc<EventSink>>,
}

impl std::fmt::Debug for LanceStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Connection` has no Debug impl; log the identity-relevant fields.
        f.debug_struct("LanceStore")
            .field("root", &self.root)
            .field("tenant_id", &self.tenant_id)
            .finish()
    }
}

impl LanceStore {
    /// Open (creating if needed) the LanceDB database for `ctx`'s tenant under
    /// `root`, following the D8 layout: `<root>/db/tenants/<tid>/lancedb`.
    ///
    /// The path is resolved from `root` — never from the process CWD — so
    /// callers control exactly where data lands (tests use temp dirs).
    pub async fn open(ctx: &TenantContext, root: impl AsRef<Path>) -> Result<Self, DomainError> {
        let root = root.as_ref().to_path_buf();
        let tenant_id = *ctx.tenant_id();
        let lancedb_dir = root
            .join("db")
            .join("tenants")
            .join(tenant_id.to_string())
            .join("lancedb");

        std::fs::create_dir_all(&lancedb_dir).map_err(|source| DomainError::Io { source })?;

        // Raw local path, NO `file://` scheme: on Windows the url crate turns
        // `file://C:/...` into `file:///C:/...` (host-less form), which the
        // local filesystem layer then strips to `/C:/...` — an invalid path
        // (os error 123). Scheme-less URIs are treated as local paths by
        // LanceDB, so pass the plain path and let the adapter join it.
        let uri = lancedb_dir.display().to_string();
        tracing::debug!(tenant = %tenant_id, path = %lancedb_dir.display(), "opening lancedb store");
        let connection = lancedb::connect(&uri)
            .execute()
            .await
            .map_err(|err| map_error("connect", err))?;

        Ok(Self {
            connection,
            root,
            tenant_id,
            events: None,
        })
    }

    /// Attach the tenant's operational event sink (REQ-OBS-008, design D5):
    /// adapter-side events (`fts_build`) then append to the SAME
    /// `logs/<tid>.events.jsonl` as the application's events. The builder
    /// keeps `open`'s signature unchanged, so existing callers and tests
    /// stay green with `None`.
    pub fn with_events(mut self, events: Option<Arc<EventSink>>) -> Self {
        self.events = events;
        self
    }

    /// The bound event sink, if any (the application shares it).
    pub(crate) fn events(&self) -> Option<&Arc<EventSink>> {
        self.events.as_ref()
    }

    /// The root directory this store was opened against.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The tenant this store is bound to.
    pub fn tenant_id(&self) -> &TenantId {
        &self.tenant_id
    }

    /// Guard: every operation must carry the bound tenant's context
    /// (REQ-TA-004/005 — the context cannot be swapped per request).
    pub(crate) fn ensure_tenant(&self, ctx: &TenantContext) -> Result<(), DomainError> {
        if ctx.tenant_id() != &self.tenant_id {
            return Err(DomainError::TenantForbidden);
        }
        Ok(())
    }

    /// Idempotently create every missing table (`chunks`, `docs`, `feedback`,
    /// `symbols`). Safe to call on every open; already-existing tables are
    /// left untouched.
    pub async fn ensure_schema(&self) -> Result<(), DomainError> {
        let names: Vec<String> = self
            .connection
            .table_names()
            .execute()
            .await
            .map_err(|err| map_error("table_names", err))?;

        for table in ALL_TABLES {
            if names.iter().any(|n| n == table) {
                continue;
            }
            let schema = schema_for(table).expect("schema for known table");
            tracing::debug!(table, "creating table");
            self.connection
                .create_empty_table(table, schema)
                .execute()
                .await
                .map_err(|err| map_error(&format!("create {table}"), err))?;
        }
        Ok(())
    }

    /// Table names currently present in this tenant's database.
    pub async fn table_names(&self) -> Result<Vec<String>, DomainError> {
        self.connection
            .table_names()
            .execute()
            .await
            .map_err(|err| map_error("table_names", err))
    }

    /// Open a table by name (must exist; call [`Self::ensure_schema`] first).
    pub(crate) async fn table(&self, name: &str) -> Result<Table, DomainError> {
        self.connection
            .open_table(name)
            .execute()
            .await
            .map_err(|err| map_error(name, err))
    }

    /// Tenant-scoped chunk count (diagnostics / stats, REQ-CL-006).
    pub async fn count_chunks(&self, ctx: &TenantContext) -> Result<u64, DomainError> {
        self.ensure_tenant(ctx)?;
        let table = self.table(crate::schema::CHUNKS).await?;
        let filter = format!("{COL_TENANT_ID} = '{}'", ctx.tenant_id());
        let count = table
            .count_rows(Some(filter))
            .await
            .map_err(|err| map_error("count_rows", err))?;
        Ok(count as u64)
    }

    /// Chunk count scoped to one workspace of the bound tenant (stats,
    /// REQ-CL-006 scenario: "chunk counts per workspace").
    pub async fn count_chunks_workspace(
        &self,
        ctx: &TenantContext,
        workspace_id: &WorkspaceId,
    ) -> Result<u64, DomainError> {
        self.ensure_tenant(ctx)?;
        let table = self.table(crate::schema::CHUNKS).await?;
        let filter = format!(
            "{COL_TENANT_ID} = '{}' AND {COL_WORKSPACE_ID} = '{workspace_id}'",
            ctx.tenant_id()
        );
        let count = table
            .count_rows(Some(filter))
            .await
            .map_err(|err| map_error("count_rows", err))?;
        Ok(count as u64)
    }
}

// --- row materialization (storage → domain) -----------------------------------

pub(crate) fn string_at(
    batch: &RecordBatch,
    column: &str,
    row: usize,
) -> Result<String, DomainError> {
    Ok(batch
        .column_by_name(column)
        .ok_or_else(|| missing_column(column))?
        .as_string::<i32>()
        .value(row)
        .to_owned())
}

pub(crate) fn id_at<T>(batch: &RecordBatch, column: &str, row: usize) -> Result<T, DomainError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let raw = string_at(batch, column, row)?;
    raw.parse().map_err(|err| DomainError::Internal {
        message: format!("corrupt {column} in store: {err}"),
    })
}

pub(crate) fn missing_column(name: &str) -> DomainError {
    DomainError::Internal {
        message: format!("result set missing column {name}"),
    }
}

/// Reconstruct a [`MemoryChunk`] from one row of the `chunks` table. The
/// stored row carries every REQ-MC-006 provenance field as a column, so no
/// information is lost in the round trip.
pub(crate) fn row_to_chunk(batch: &RecordBatch, row: usize) -> Result<MemoryChunk, DomainError> {
    let chunk_id: ChunkId = id_at(batch, crate::schema::COL_CHUNK_ID, row)?;
    let doc_id: DocId = id_at(batch, crate::schema::COL_DOC_ID, row)?;
    let tenant_id: TenantId = id_at(batch, crate::schema::COL_TENANT_ID, row)?;
    let workspace_id: WorkspaceId = id_at(batch, crate::schema::COL_WORKSPACE_ID, row)?;
    let agent_id = AgentId::new(string_at(batch, crate::schema::COL_AGENT_ID, row)?);
    let source: SourceKind =
        serde_json::from_str(&string_at(batch, crate::schema::COL_SOURCE, row)?).map_err(
            |err| DomainError::Internal {
                message: format!("corrupt source_json in store: {err}"),
            },
        )?;
    let created_at = crate::schema::nanos_to_ts(
        batch
            .column_by_name(crate::schema::COL_CREATED_AT)
            .ok_or_else(|| missing_column(crate::schema::COL_CREATED_AT))?
            .as_primitive::<TimestampNanosecondType>()
            .value(row),
    );

    let vector = batch
        .column_by_name(crate::schema::COL_VECTOR)
        .ok_or_else(|| missing_column(crate::schema::COL_VECTOR))?
        .as_fixed_size_list();
    let vector = if vector.is_null(row) {
        None
    } else {
        Some(
            vector
                .value(row)
                .as_primitive::<Float32Type>()
                .values()
                .to_vec(),
        )
    };

    let embedding_model_version = string_at(batch, crate::schema::COL_EMBEDDING_MODEL, row)?;
    let text = string_at(batch, crate::schema::COL_TEXT, row)?;

    let provenance = Provenance {
        source,
        doc_id,
        chunk_id,
        created_at,
        embedding_model_version,
        tenant_id,
        workspace_id,
        agent_id: agent_id.clone(),
    };

    Ok(MemoryChunk {
        id: chunk_id,
        tenant_id,
        workspace_id,
        agent_id,
        doc_id,
        text,
        vector,
        created_at,
        provenance,
    })
}

/// Reconstruct a [`SearchHit`] (chunk + retrieval score) from one row.
pub(crate) fn row_to_search_hit(
    batch: &RecordBatch,
    row: usize,
    score: f32,
) -> Result<SearchHit, DomainError> {
    let chunk = row_to_chunk(batch, row)?;
    Ok(SearchHit {
        chunk_id: chunk.id,
        text: chunk.text,
        score,
        provenance: chunk.provenance,
    })
}

/// Map a `lancedb::Error` onto the domain taxonomy (stable codes; D7).
///
/// IO-ish failures (object store, HTTP, retries, timeouts, dir creation)
/// become `IO`; validation/schema problems become `INVALID_INPUT`; missing
/// tables become `NOT_FOUND`; everything else is `INTERNAL` (logged).
///
/// REQ-DAEMON-009: a genuine store-lock conflict (another holder — daemon
/// vs worker, or a concurrent writer) surfaces the `STORE_LOCKED` tier,
/// never a generic IO/INTERNAL. LanceDB 0.33 / lance 9.0 use optimistic
/// concurrency (no connection-level lock), so lock conflicts materialize
/// as lock-shaped messages or Windows sharing violations at write time.
pub fn map_error(context: &str, err: lancedb::Error) -> DomainError {
    if is_lock_conflict(&err) {
        return DomainError::StoreLocked {
            message: format!("{context}: {err}"),
        };
    }
    match err {
        lancedb::Error::TableNotFound { name, .. }
        | lancedb::Error::DatabaseNotFound { name }
        | lancedb::Error::IndexNotFound { name } => DomainError::NotFound {
            what: format!("{context}: {name}"),
        },
        lancedb::Error::TableAlreadyExists { .. }
        | lancedb::Error::DatabaseAlreadyExists { .. } => DomainError::AlreadyExists {
            message: context.to_string(),
        },
        lancedb::Error::InvalidTableName { name, reason } => DomainError::InvalidInput {
            message: format!("{context}: invalid table name {name}: {reason}"),
        },
        lancedb::Error::InvalidInput { message }
        | lancedb::Error::Schema { message }
        | lancedb::Error::EmbeddingFunctionNotFound { name: message, .. } => {
            DomainError::InvalidInput {
                message: format!("{context}: {message}"),
            }
        }
        e @ (lancedb::Error::CreateDir { .. }
        | lancedb::Error::ObjectStore { .. }
        | lancedb::Error::Timeout { .. }) => DomainError::Io {
            source: std::io::Error::other(format!("{context}: {e}")),
        },
        other => DomainError::Internal {
            message: format!("{context}: {other}"),
        },
    }
}

/// Whether a `lancedb::Error` is a genuine store-lock conflict
/// (REQ-DAEMON-009 STORE_LOCKED tier). Token-based matching on the
/// rendered message — precise enough to avoid false positives on words
/// like "blocked" or "clock":
///
/// * lock messages from lance / object_store ("already locked",
///   "dataset is locked", "lock not acquired", "waiting for lock");
/// * Windows sharing violations ("being used by another process",
///   "sharing violation" — os error 32 from a second holder).
fn is_lock_conflict(err: &lancedb::Error) -> bool {
    let message = err.to_string().to_lowercase();
    const LOCK_TOKENS: [&str; 6] = [
        "already locked",
        "is locked",
        "lock not acquired",
        "waiting for lock",
        "being used by another process",
        // Windows ERROR_SHARING_VIOLATION (os error 32).
        "sharing violation",
    ];
    LOCK_TOKENS.iter().any(|token| message.contains(token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use memento_domain::error::{
        CODE_ALREADY_EXISTS, CODE_INTERNAL, CODE_INVALID_INPUT, CODE_IO, CODE_NOT_FOUND,
    };

    #[test]
    fn map_error_keeps_stable_codes() {
        let not_found = lancedb::Error::TableNotFound {
            name: "chunks".into(),
            source: "gone".into(),
        };
        assert_eq!(map_error("t", not_found).code(), CODE_NOT_FOUND);

        let exists = lancedb::Error::TableAlreadyExists {
            name: "chunks".into(),
        };
        assert_eq!(map_error("t", exists).code(), CODE_ALREADY_EXISTS);

        let invalid = lancedb::Error::InvalidInput {
            message: "bad sql".into(),
        };
        assert_eq!(map_error("t", invalid).code(), CODE_INVALID_INPUT);

        let io = lancedb::Error::Timeout {
            message: "hung".into(),
        };
        assert_eq!(map_error("t", io).code(), CODE_IO);

        let internal = lancedb::Error::Runtime {
            message: "weird".into(),
        };
        assert_eq!(map_error("t", internal).code(), CODE_INTERNAL);
    }

    #[test]
    fn lock_conflict_errors_map_to_store_locked() {
        // REQ-DAEMON-009 STORE_LOCKED tier: genuine lock conflicts from
        // lance / object_store map to STORE_LOCKED, never a generic IO or
        // INTERNAL.
        use memento_domain::error::CODE_STORE_LOCKED;

        // lance "dataset is locked" shape (Runtime wrapper).
        let runtime_lock = lancedb::Error::Runtime {
            message: "dataset is locked by another process".into(),
        };
        let mapped = map_error("connect", runtime_lock);
        assert_eq!(mapped.code(), CODE_STORE_LOCKED);
        assert_eq!(mapped.exit_code(), 23, "REQ-CL-005 exit code");

        // object_store lock-wait timeout shape.
        let timeout_lock = lancedb::Error::Timeout {
            message: "timed out waiting for lock on table".into(),
        };
        assert_eq!(map_error("connect", timeout_lock).code(), CODE_STORE_LOCKED);

        // Windows sharing violation (os error 32) — rendered message from
        // an ObjectStore-wrapped io error.
        let sharing = lancedb::Error::Runtime {
            message:
                "object_store error: Generic local: The process cannot access the file because it is being used by another process (os error 32)"
                    .into(),
        };
        assert_eq!(map_error("connect", sharing).code(), CODE_STORE_LOCKED);
    }

    #[test]
    fn lock_detection_has_no_false_positives() {
        // Words containing the "lock" substring (blocked, clock) must NOT
        // trip the STORE_LOCKED tier — the token matcher is precise.
        use memento_domain::error::CODE_INTERNAL;

        let blocked = lancedb::Error::Runtime {
            message: "operation blocked by schema evolution".into(),
        };
        assert_eq!(map_error("t", blocked).code(), CODE_INTERNAL);

        let clock = lancedb::Error::Runtime {
            message: "clock skew detected".into(),
        };
        assert_eq!(map_error("t", clock).code(), CODE_INTERNAL);
    }
}
