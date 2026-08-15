//! memento-testkit — shared test infrastructure (design D2).
//!
//! Handwritten fakes and fixtures shared by every crate's test suites
//! (`mockall` is intentionally NOT used — the port traits are ours, the fakes
//! are honest, and they double as criterion fixtures):
//!
//! * [`stub_embed`] — deterministic, hash-bucketed embeddings (same text →
//!   same vector) that need no ONNX runtime.
//! * [`temp_store`] — scratch per-tenant stores laid out like production
//!   (design D8) on a `tempfile::TempDir`.
//! * [`clock`] — injectable clock for retention-sweep tests (REQ-ML-003,
//!   design D5).
//! * [`fixtures`] — Spanish corpus fixtures (accented text, long documents).
//!
//! The crate itself is never a production dependency: only `[dev-dependencies]`
//! of adapter/application crates pull it in.

pub mod clock;
pub mod daemon_fixture;
pub mod fixtures;
pub mod pipe_client_fixture;
pub mod stub_embed;
pub mod temp_store;

pub use clock::TestClock;
pub use daemon_fixture::{DaemonFixture, DaemonFixtureClient, DaemonFixtureOptions};
pub use fixtures::{SPANISH_CORPUS, accent_pairs, long_spanish_doc, spanish_corpus};
pub use pipe_client_fixture::PipeClientFixture;
pub use stub_embed::{StubEmbedPort, deterministic_embed};
pub use temp_store::TempStore;
