//! `symbols` table adapter: the LanceDB mirror of the L2 symbol map
//! (T-041, REQ-CK-*). The fast query path is the in-memory hashmap in
//! memento-okf; this mirror makes the same facts queryable from the
//! storage layer (cross-crate introspection, re-index diffing) and is
//! refreshed with replace semantics per project on every index run.

use crate::schema::{
    COL_CREATED_AT, COL_KIND, COL_LOCATION, COL_PROJECT_ID, COL_SIGNATURE, COL_SYMBOL_NAME,
    SYMBOLS, symbols_schema, tenant_scope, ts_to_nanos,
};
use crate::store::{LanceStore, map_error};
use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use lancedb::arrow::arrow_array::types::TimestampNanosecondType;
use lancedb::arrow::arrow_array::{
    RecordBatch, StringArray, TimestampNanosecondArray, cast::AsArray,
};
use lancedb::expr::{col, lit};
use lancedb::query::{ExecutableQuery, QueryBase};
use memento_domain::{DomainError, TenantContext};
use std::sync::Arc;

/// One symbol row to mirror (the store stamps `created_at` so callers
/// never depend on wall-clock).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolInput {
    pub symbol_name: String,
    pub project_id: String,
    /// ConceptKind as_str (e.g. "Function").
    pub kind: String,
    /// `file#Lstart-Lend` display form of the definition location.
    pub location: String,
    pub signature: Option<String>,
}

/// A materialized mirror row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRow {
    pub symbol_name: String,
    pub project_id: String,
    pub kind: String,
    pub location: String,
    pub signature: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Replace one project's mirror rows: delete every row of `project_id`
/// for the bound tenant, then add the new set in one atomic `table.add`.
/// Re-indexing therefore never leaves stale or duplicated symbols behind.
pub async fn replace_symbols(
    store: &LanceStore,
    ctx: &TenantContext,
    project_id: &str,
    rows: &[SymbolInput],
) -> Result<(), DomainError> {
    store.ensure_tenant(ctx)?;
    let table = store.table(SYMBOLS).await?;
    let scope = tenant_scope(ctx.tenant_id()).and(col(COL_PROJECT_ID).eq(lit(project_id)));
    table
        .delete(&scope)
        .await
        .map_err(|err| map_error("replace_symbols.delete", err))?;
    if rows.is_empty() {
        return Ok(());
    }
    let batch = symbols_to_batch(ctx, project_id, rows)?;
    table
        .add(batch)
        .execute()
        .await
        .map_err(|err| map_error("replace_symbols.add", err))?;
    Ok(())
}

/// Look up mirror rows for a tenant + project, optionally filtered by
/// symbol name. Deterministic order (symbol_name, then location).
pub async fn lookup_symbols(
    store: &LanceStore,
    ctx: &TenantContext,
    project_id: &str,
    symbol: Option<&str>,
) -> Result<Vec<SymbolRow>, DomainError> {
    store.ensure_tenant(ctx)?;
    let table = store.table(SYMBOLS).await?;
    let mut filter = tenant_scope(ctx.tenant_id()).and(col(COL_PROJECT_ID).eq(lit(project_id)));
    if let Some(name) = symbol {
        filter = filter.and(col(COL_SYMBOL_NAME).eq(lit(name)));
    }
    let stream = table
        .query()
        .only_if_expr(filter)
        .execute()
        .await
        .map_err(|err| map_error("lookup_symbols", err))?;
    let batches: Vec<RecordBatch> = stream
        .try_collect()
        .await
        .map_err(|err| map_error("lookup_symbols", err))?;

    let mut rows = Vec::new();
    for batch in &batches {
        for row in 0..batch.num_rows() {
            rows.push(row_to_symbol(batch, row)?);
        }
    }
    rows.sort_by(|a, b| {
        a.symbol_name
            .cmp(&b.symbol_name)
            .then_with(|| a.location.cmp(&b.location))
    });
    Ok(rows)
}

/// Count mirror rows for a tenant + project (used by index reports).
pub async fn count_symbols(
    store: &LanceStore,
    ctx: &TenantContext,
    project_id: &str,
) -> Result<u64, DomainError> {
    store.ensure_tenant(ctx)?;
    let table = store.table(SYMBOLS).await?;
    let filter = tenant_scope(ctx.tenant_id()).and(col(COL_PROJECT_ID).eq(lit(project_id)));
    let stream = table
        .query()
        .only_if_expr(filter)
        .execute()
        .await
        .map_err(|err| map_error("count_symbols", err))?;
    let batches: Vec<RecordBatch> = stream
        .try_collect()
        .await
        .map_err(|err| map_error("count_symbols", err))?;
    Ok(batches.iter().map(|b| b.num_rows() as u64).sum())
}

/// Build a `RecordBatch` with the `symbols` schema.
fn symbols_to_batch(
    ctx: &TenantContext,
    project_id: &str,
    rows: &[SymbolInput],
) -> Result<RecordBatch, DomainError> {
    let schema = symbols_schema();
    let now = Utc::now();
    let created_at = ts_to_nanos(now);
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|r| r.symbol_name.as_str())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|_| ctx.tenant_id().to_string())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|_| project_id).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r.location.as_str()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|r| r.signature.as_deref().unwrap_or(""))
                    .collect::<Vec<_>>(),
            )),
            Arc::new(TimestampNanosecondArray::from(vec![created_at; rows.len()])),
        ],
    )
    .map_err(|err| map_error("symbols_to_batch", lancedb::Error::Arrow { source: err }))
}

/// Reconstruct a [`SymbolRow`] from one batch row.
fn row_to_symbol(batch: &RecordBatch, row: usize) -> Result<SymbolRow, DomainError> {
    let text = |column: &str| -> Result<String, DomainError> {
        Ok(batch
            .column_by_name(column)
            .ok_or_else(|| DomainError::Internal {
                message: format!("symbols table missing column {column}"),
            })?
            .as_string::<i32>()
            .value(row)
            .to_string())
    };

    let signature = text(COL_SIGNATURE)?;
    let created_at_ns = batch
        .column_by_name(COL_CREATED_AT)
        .ok_or_else(|| DomainError::Internal {
            message: "symbols table missing created_at".into(),
        })?
        .as_primitive::<TimestampNanosecondType>()
        .value(row);

    Ok(SymbolRow {
        symbol_name: text(COL_SYMBOL_NAME)?,
        project_id: text(COL_PROJECT_ID)?,
        kind: text(COL_KIND)?,
        location: text(COL_LOCATION)?,
        signature: (!signature.is_empty()).then_some(signature),
        created_at: crate::schema::nanos_to_ts(created_at_ns),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use memento_domain::TenantId;
    use memento_testkit::TempStore;

    fn rows() -> Vec<SymbolInput> {
        vec![
            SymbolInput {
                symbol_name: "add".into(),
                project_id: "abc123".into(),
                kind: "Function".into(),
                location: "src/lib.rs#L1-L3".into(),
                signature: Some("pub fn add(a: i32, b: i32) -> i32".into()),
            },
            SymbolInput {
                symbol_name: "main".into(),
                project_id: "abc123".into(),
                kind: "Function".into(),
                location: "src/main.rs#L4-L6".into(),
                signature: None,
            },
        ]
    }

    #[tokio::test]
    async fn replace_and_lookup_round_trip() {
        let ts = TempStore::new();
        let store = LanceStore::open(&ts.ctx(), ts.root()).await.unwrap();
        store.ensure_schema().await.unwrap();
        let ctx = ts.ctx();

        replace_symbols(&store, &ctx, "abc123", &rows())
            .await
            .unwrap();
        let found = lookup_symbols(&store, &ctx, "abc123", None).await.unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].symbol_name, "add");
        assert_eq!(
            found[0].signature.as_deref(),
            Some("pub fn add(a: i32, b: i32) -> i32")
        );
        assert_eq!(found[1].symbol_name, "main");
        assert_eq!(found[1].signature, None);
        assert_eq!(count_symbols(&store, &ctx, "abc123").await.unwrap(), 2);
    }

    #[tokio::test]
    async fn lookup_filters_by_symbol_name() {
        let ts = TempStore::new();
        let store = LanceStore::open(&ts.ctx(), ts.root()).await.unwrap();
        store.ensure_schema().await.unwrap();
        let ctx = ts.ctx();

        replace_symbols(&store, &ctx, "abc123", &rows())
            .await
            .unwrap();
        let found = lookup_symbols(&store, &ctx, "abc123", Some("main"))
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].symbol_name, "main");
        let none = lookup_symbols(&store, &ctx, "abc123", Some("nope"))
            .await
            .unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn replace_clears_stale_rows() {
        let ts = TempStore::new();
        let store = LanceStore::open(&ts.ctx(), ts.root()).await.unwrap();
        store.ensure_schema().await.unwrap();
        let ctx = ts.ctx();

        replace_symbols(&store, &ctx, "abc123", &rows())
            .await
            .unwrap();
        replace_symbols(&store, &ctx, "abc123", &rows()[..1])
            .await
            .unwrap();
        let found = lookup_symbols(&store, &ctx, "abc123", None).await.unwrap();
        assert_eq!(found.len(), 1, "stale rows replaced, not duplicated");
        assert_eq!(found[0].symbol_name, "add");
    }

    #[tokio::test]
    async fn projects_are_isolated_within_tenant() {
        let ts = TempStore::new();
        let store = LanceStore::open(&ts.ctx(), ts.root()).await.unwrap();
        store.ensure_schema().await.unwrap();
        let ctx = ts.ctx();

        replace_symbols(&store, &ctx, "proj-a", &rows())
            .await
            .unwrap();
        assert!(
            lookup_symbols(&store, &ctx, "proj-b", None)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(count_symbols(&store, &ctx, "proj-b").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn foreign_tenant_context_is_forbidden() {
        let ts = TempStore::new();
        let store = LanceStore::open(&ts.ctx(), ts.root()).await.unwrap();
        store.ensure_schema().await.unwrap();

        let other = TenantContext::new_for_tests(
            TenantId::new(),
            *ts.workspace_id(),
            ts.agent_id().clone(),
        );
        let err = replace_symbols(&store, &other, "abc123", &rows())
            .await
            .unwrap_err();
        assert_eq!(err.code(), "TENANT_FORBIDDEN");
    }

    #[tokio::test]
    async fn empty_replace_is_a_clean_delete() {
        let ts = TempStore::new();
        let store = LanceStore::open(&ts.ctx(), ts.root()).await.unwrap();
        store.ensure_schema().await.unwrap();
        let ctx = ts.ctx();

        replace_symbols(&store, &ctx, "abc123", &rows())
            .await
            .unwrap();
        replace_symbols(&store, &ctx, "abc123", &[]).await.unwrap();
        assert!(
            lookup_symbols(&store, &ctx, "abc123", None)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
