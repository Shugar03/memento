//! T-020 acceptance: tempdir table creation + scope-expr tests.
//!
//! Real LanceDB on a `TempStore` (memento-testkit): per-tenant dirs, the four
//! tables, idempotent schema bootstrap, and reopen stability.

use memento_lancedb::LanceStore;
use memento_lancedb::schema::{ALL_TABLES, CHUNKS, DOCS, FEEDBACK, SYMBOLS};
use memento_testkit::TempStore;

#[tokio::test(flavor = "multi_thread")]
async fn tempdir_creates_per_tenant_tables() {
    let ts = TempStore::new();
    let store = LanceStore::open(&ts.ctx(), ts.root()).await.expect("open");

    store.ensure_schema().await.expect("ensure schema");

    let names = store.table_names().await.expect("table names");
    for table in ALL_TABLES {
        assert!(names.iter().any(|n| n == table), "missing table {table}");
    }

    // The tenant directory itself exists on disk (D8 layout).
    assert!(
        ts.lancedb_dir().is_dir(),
        "tenant lancedb dir must exist at {}",
        ts.lancedb_dir().display()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ensure_schema_is_idempotent() {
    let ts = TempStore::new();
    let store = LanceStore::open(&ts.ctx(), ts.root()).await.expect("open");

    store.ensure_schema().await.expect("first ensure");
    store
        .ensure_schema()
        .await
        .expect("second ensure must be a no-op");

    let names = store.table_names().await.expect("table names");
    let expected = ALL_TABLES.len();
    assert_eq!(
        names.len(),
        expected,
        "no duplicate tables after re-ensure: {names:?}"
    );
    for table in ALL_TABLES {
        assert!(names.iter().any(|n| n == table));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn schema_survives_reopen() {
    let ts = TempStore::new();
    let ctx = ts.ctx();

    {
        let store = LanceStore::open(&ctx, ts.root()).await.expect("open");
        store.ensure_schema().await.expect("ensure schema");
    }

    // Reopen on the same tenant root: tables must be detected, not recreated.
    let reopened = LanceStore::open(&ctx, ts.root()).await.expect("reopen");
    let names = reopened.table_names().await.expect("table names");
    assert_eq!(names.len(), ALL_TABLES.len(), "tables: {names:?}");
    assert!(names.contains(&CHUNKS.to_string()));
    assert!(names.contains(&DOCS.to_string()));
    assert!(names.contains(&FEEDBACK.to_string()));
    assert!(names.contains(&SYMBOLS.to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn tenants_get_isolated_directories() {
    let a = TempStore::new();
    let b = TempStore::new();

    let store_a = LanceStore::open(&a.ctx(), a.root()).await.expect("open a");
    let store_b = LanceStore::open(&b.ctx(), b.root()).await.expect("open b");
    store_a.ensure_schema().await.expect("schema a");
    store_b.ensure_schema().await.expect("schema b");

    // Structural isolation (D8): different tenant → different database dir.
    assert_ne!(a.lancedb_dir(), b.lancedb_dir());
    assert_eq!(store_a.tenant_id(), a.tenant_id());
    assert_eq!(store_b.tenant_id(), b.tenant_id());

    // Both tenants have their own complete table set.
    for (label, store) in [("a", &store_a), ("b", &store_b)] {
        let names = store.table_names().await.expect(label);
        for table in ALL_TABLES {
            assert!(names.iter().any(|n| n == table), "{label}: missing {table}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn foreign_context_is_rejected() {
    let ts = TempStore::new();
    let store = LanceStore::open(&ts.ctx(), ts.root()).await.expect("open");

    // A different tenant's context against this store → TENANT_FORBIDDEN
    // (defense in depth beyond the directory boundary).
    let other = TempStore::new();
    let err = store
        .count_chunks(&other.ctx())
        .await
        .expect_err("must reject");
    assert_eq!(err.code(), memento_domain::error::CODE_TENANT_FORBIDDEN);
}
