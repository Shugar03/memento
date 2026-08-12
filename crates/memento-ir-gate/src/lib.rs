//! memento-ir-gate — IR golden-set CI gate.
//!
//! Deterministic retrieval-quality regression tests over a small synthetic
//! corpus. The gate ingests [`crate::CORPUS_DIR`] fixture docs into a fresh
//! temp tenant, runs every query in [`crate::GOLDEN_SET`] through the RRF
//! hybrid path (`top_k = 5`) and scores the results with the keyword-substring
//! relevance proxy used by the Python golden-set script (NFD accent folding).
//!
//! Two modes:
//!
//! * **Stub mode** (default, no env): [`memento_testkit::StubEmbedPort`]
//!   embeddings — deterministic, no ONNX download. Enforces relaxed
//!   thresholds (MRR@5 ≥ 0.70, Recall@5 ≥ 0.90) because token-hash vectors
//!   are weaker than the real model (documented tradeoff).
//! * **Real mode** (`MEMENTO_IR_GATE=1`): the shipped int8 MultilingualE5Base
//!   model (provisioned by `scripts/provision_int8.py`). Enforces the strict
//!   thresholds (MRR@5 ≥ 0.90, Recall@5 ≥ 0.93). Fails loudly if the model
//!   file is missing — the gate never silently skips.

use serde::Deserialize;
use std::path::Path;
use unicode_normalization::UnicodeNormalization;

/// Is `c` a Unicode combining mark (General_Category Mn/Mc/Me)?
///
/// Portable on the pinned toolchain (1.85), where `char::is_combining_mark`
/// is not yet stable (stabilized in 1.87). Covers the combining ranges that
/// appear after NFD in the corpus (Spanish/English accents): Combining
/// Diacritical Marks, Extended, Supplement, for Symbols, Half Marks, and the
/// kana combining marks. Everything outside these ranges is a base char and
/// stays in the folded text.
fn is_combining_mark(c: char) -> bool {
    matches!(c,
        '\u{0300}'..='\u{036F}'   // Combining Diacritical Marks
        | '\u{1AB0}'..='\u{1AFF}' // Combining Diacritical Marks Extended
        | '\u{1DC0}'..='\u{1DFF}' // Combining Diacritical Marks Supplement
        | '\u{20D0}'..='\u{20FF}' // Combining Diacritical Marks for Symbols
        | '\u{3099}'..='\u{309A}' // combining kana
        | '\u{FE20}'..='\u{FE2F}' // Combining Half Marks
    )
}

/// Directory (relative to the crate) holding the synthetic corpus.
pub const CORPUS_DIR: &str = "tests/fixtures/ir-corpus";

/// Path (relative to the crate) of the golden set.
pub const GOLDEN_SET: &str = "tests/golden-set.json";

/// Relaxed thresholds enforced with stub embeddings.
///
/// The deterministic `StubEmbedPort` is a token-hash vector fake: it cannot
/// do cross-lingual or semantic matching, so the ES-paraphrase queries (the
/// golden set's weak spot) land at rank 2-5 or miss in stub mode. Measured
/// stub baseline on the current 50-query set: MRR@5 ≈ 0.79, Recall@5 ≈ 0.96.
/// The thresholds sit below that baseline with margin so the fast test job is
/// a deterministic pipeline smoke test; QUALITY is enforced by the real-model
/// gate (`MEMENTO_IR_GATE=1`, [`REAL_MRR5`]/[`REAL_RECALL5`]).
pub const STUB_MRR5: f32 = 0.70;
pub const STUB_RECALL5: f32 = 0.90;

/// Strict thresholds enforced with the real int8 model (`MEMENTO_IR_GATE=1`).
pub const REAL_MRR5: f32 = 0.90;
pub const REAL_RECALL5: f32 = 0.93;

/// One golden query with the fragment keywords that prove relevance.
#[derive(Debug, Clone, Deserialize)]
pub struct GoldenQuery {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "source_doc")]
    pub source_doc: String,
    pub query: String,
    #[serde(rename = "expected_fragments")]
    pub expected_fragments: Vec<String>,
}

/// NFD-fold a text: lowercase + strip combining marks. Mirrors the Python
/// script's `text.lower()` + NFD normalization and the engine's
/// `ascii_folding`, so `información` matches `informacion`.
pub fn fold(text: &str) -> String {
    text.nfkd()
        .filter(|c| !is_combining_mark(*c))
        .collect::<String>()
        .to_lowercase()
}

/// Keyword-substring relevance proxy: any folded fragment inside the folded
/// hit text proves relevance.
pub fn is_relevant(hit_text: &str, fragments: &[String]) -> bool {
    let folded = fold(hit_text);
    fragments.iter().any(|f| folded.contains(&fold(f)))
}

/// MRR@5 for one query: reciprocal of the first relevant hit's rank (1-based),
/// 0 when no relevant hit lands in the top 5.
pub fn mrr_at_5(hit_texts: &[&str], fragments: &[String]) -> f32 {
    for (i, text) in hit_texts.iter().take(5).enumerate() {
        if is_relevant(text, fragments) {
            return 1.0 / (i as f32 + 1.0);
        }
    }
    0.0
}

/// Load the golden set from the crate-relative path.
pub fn load_golden_set() -> Vec<GoldenQuery> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(GOLDEN_SET);
    let raw = std::fs::read_to_string(&path).expect("golden set must be readable");
    serde_json::from_str(&raw).expect("golden set must be valid JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_strips_accents_and_case() {
        assert_eq!(fold("Información"), "informacion");
        assert_eq!(fold("Área bajo la curva"), "area bajo la curva");
        assert_eq!(fold("B2B SaaS"), "b2b saas");
    }

    #[test]
    fn relevance_ignores_accents() {
        let frags = vec!["area bajo la curva".to_string()];
        assert!(is_relevant("el área bajo la curva", &frags));
        assert!(is_relevant("el AREA bajo la CURVA", &frags));
        assert!(!is_relevant("la antiderivada", &frags));
    }

    #[test]
    fn mrr_scores_first_relevant_rank() {
        let frags = vec!["audacity".to_string()];
        let hits = ["some other text", "about audacity here", "audacity again"];
        assert!((mrr_at_5(&hits, &frags) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn mrr_zero_when_not_in_top5() {
        let frags = vec!["zzz".to_string()];
        let hits = ["a", "b", "c", "d", "e", "zzz"];
        assert_eq!(mrr_at_5(&hits, &frags), 0.0);
    }

    #[test]
    fn golden_set_schema_parses() {
        let golden = load_golden_set();
        assert!(golden.len() >= 50, "golden set must grow to ~50 queries");
        for q in &golden {
            assert!(!q.id.is_empty());
            assert!(!q.query.is_empty());
            assert!(!q.expected_fragments.is_empty());
        }
    }

    /// Every expected fragment must be an ACTUAL substring of the folded
    /// corpus. This is the gate's own ground truth: a fragment that never
    /// occurs in any fixture doc makes the query untestable (and usually
    /// means it was split by a line break, like the old "incremental
    /// reindexing" fixture).
    #[test]
    fn every_fragment_occurs_in_the_corpus() {
        let corpus_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(CORPUS_DIR);
        let mut corpus = String::new();
        for entry in std::fs::read_dir(&corpus_dir).expect("corpus dir") {
            let path = entry.expect("entry").path();
            if path.extension().map(|e| e == "md").unwrap_or(false) {
                corpus.push_str(&std::fs::read_to_string(&path).expect("read doc"));
                corpus.push('\n');
            }
        }
        let folded = fold(&corpus);
        for q in load_golden_set() {
            for frag in &q.expected_fragments {
                let f = fold(frag);
                assert!(
                    folded.contains(&f),
                    "query {} fragment {:?} does not occur in the corpus",
                    q.id,
                    frag
                );
            }
        }
    }
}
