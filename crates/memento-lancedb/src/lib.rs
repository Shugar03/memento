//! memento-lancedb — Memento RS LanceDB storage adapter (design D8).
//!
//! One process-bound tenant per database directory; every table carries
//! `tenant_id`/`workspace_id` columns so all queries are scoped by
//! construction. See [`schema`] for the layout and [`store`] for the
//! connection bootstrap. Vector/FTS search and the purge chain land in
//! [`vector`], [`fts`] and [`maintenance`].

pub mod fts;
pub mod maintenance;
pub mod schema;
pub mod store;
pub mod vector;

pub use schema::{
    CHUNKS, DOCS, FEEDBACK, SYMBOLS, chunks_scope, tenant_scope, workspace_scope,
};
pub use store::{LanceStore, map_error};
