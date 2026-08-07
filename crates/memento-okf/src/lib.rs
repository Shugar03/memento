//! memento-okf — Memento RS code-knowledge adapter (REQ-CK-*).
//!
//! Indexes Rust + Python repositories through okf-rs and exposes the four
//! knowledge layers (design): L1 bundles (source of truth), L2 symbol
//! map, L3 relationship graph, L4 architectural summaries. Layer
//! modules and the `KnowledgePort` implementation land across the batch-5
//! commits (T-040 index pipeline → T-041 L2 → T-042 L3 → T-043 L4 +
//! queries + port).

pub mod index;
pub mod layers;
pub mod project_id;

pub use index::{IndexReport, SUPPORTED_LANGUAGES, SkipEntry, index_project};
pub use project_id::{is_valid_project_id, project_id_from_path};
