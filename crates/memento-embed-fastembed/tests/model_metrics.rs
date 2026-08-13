//! Embed cache metrics wiring (REQ-OBS-006, design D2).
//!
//! Lives in its OWN test binary (own process) on purpose: the Prometheus
//! recorder is process-global and the embed cache counters are unlabeled
//! (the adapter has no tenant — documented scope note in design D5), so
//! exact values are only deterministic in a process no other test shares.
//! Unit tests in `model.rs` stay free of recorder-state races.

use memento_embed_fastembed::{EmbeddingBackend, ModelLoader};
use memento_testkit::deterministic_embed;
use std::path::PathBuf;
use std::sync::Arc;

struct StubBackend;

impl EmbeddingBackend for StubBackend {
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, memento_domain::DomainError> {
        Ok(texts
            .iter()
            .map(|text| deterministic_embed(text, 768))
            .collect())
    }

    fn model_version(&self) -> &'static str {
        "stub-e5"
    }
}

#[test]
fn embed_cache_hits_and_misses_are_recorded_when_metrics_enabled() {
    // SAFETY: test-only env mutation; this test binary owns the process.
    unsafe { std::env::set_var("MEMENTO_METRICS", "1") };
    // First render installs the recorder on the empty registry (REQ-OBS-006:
    // zero metrics work while the var is unset — the gate lives in
    // `ensure_recorder`, metrics.rs).
    let _ = memento_observability::metrics::render();

    let loader = ModelLoader::from_backend(PathBuf::new(), Arc::new(StubBackend));

    // Two fresh texts → two cache misses, zero hits.
    let out = loader
        .embed(&["hola", "mundo"])
        .expect("embed ok")
        .expect("enabled");
    assert_eq!(out.len(), 2);
    let render = memento_observability::metrics::render();
    assert!(
        render.contains("memento_embed_cache_misses_total 2"),
        "misses recorded: {render}"
    );
    assert!(
        !render.contains("memento_embed_cache_hits_total"),
        "no hits yet: {render}"
    );

    // Same texts again → two hits; misses unchanged.
    let _ = loader
        .embed(&["hola", "mundo"])
        .expect("embed ok")
        .expect("enabled");
    let render = memento_observability::metrics::render();
    assert!(
        render.contains("memento_embed_cache_hits_total 2"),
        "hits recorded: {render}"
    );
    assert!(
        render.contains("memento_embed_cache_misses_total 2"),
        "misses unchanged: {render}"
    );

    // Triangulation: one cached + one fresh text → hit and miss both advance.
    let _ = loader
        .embed(&["hola", "otro"])
        .expect("embed ok")
        .expect("enabled");
    let render = memento_observability::metrics::render();
    assert!(
        render.contains("memento_embed_cache_hits_total 3"),
        "third hit recorded: {render}"
    );
    assert!(
        render.contains("memento_embed_cache_misses_total 3"),
        "third miss recorded: {render}"
    );
}
