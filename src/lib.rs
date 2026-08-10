//! memento-e2e — workspace-level integration harness and benchmarks.
//!
//! This package carries no product code. It exists so the workspace root
//! can host cross-crate integration tests (the MCP↔CLI equivalence suite of
//! T-102 in `tests/equivalence.rs`) and the criterion benches of T-103
//! (`benches/*`). Everything here is dev-dependencies only and `publish =
//! false`.
