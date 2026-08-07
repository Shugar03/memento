//! LanceStore: per-tenant connection bootstrap and schema management (T-020).
//!
//! One [`LanceStore`] instance is bound to exactly one tenant directory
//! (design D8): `open` resolves `<root>/db/tenants/<tenant_id>/lancedb`, and
//! every later call re-validates the passed context against the bound tenant
//! (`TENANT_FORBIDDEN` on mismatch — defense in depth on top of the directory
//! boundary). Workspaces are NOT bound: they are per-call parameters
//! (mandatory workspace filter, REQ-MR-006).

use crate::schema::{ALL_TABLES, COL_TENANT_ID, schema_for};
use lancedb::connection::Connection;
use lancedb::table::Table;
use memento_domain::{DomainError, TenantContext, TenantId};
use std::path::{Path, PathBuf};

/// LanceDB storage adapter bound to one tenant directory.
pub struct LanceStore {
    connection: Connection,
    root: PathBuf,
    tenant_id: TenantId,
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

        std::fs::create_dir_all(&lancedb_dir)
            .map_err(|source| DomainError::Io { source })?;

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
        })
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
    fn ensure_tenant(&self, ctx: &TenantContext) -> Result<(), DomainError> {
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
}

/// Map a `lancedb::Error` onto the domain taxonomy (stable codes; D7).
///
/// IO-ish failures (object store, HTTP, retries, timeouts, dir creation)
/// become `IO`; validation/schema problems become `INVALID_INPUT`; missing
/// tables become `NOT_FOUND`; everything else is `INTERNAL` (logged).
pub fn map_error(context: &str, err: lancedb::Error) -> DomainError {
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
}
