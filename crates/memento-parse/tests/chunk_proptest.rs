//! Proptest chunk invariants (T-101, REQ-MC-003 verification notes).
//!
//! Property-style checks over the Spanish corpus (testkit fixtures):
//!
//! 1. **Bounds** — every chunk ≤ 300 tokens; every non-final chunk ≥ 256
//!    (the final chunk MAY be shorter, REQ-MC-003).
//! 2. **Overlap** — consecutive chunks share an overlap region of 1..=45
//!    absolute tokens (10-15% of 256-300 ⇒ 26-45 configured; measured
//!    overlap lands on sentence boundaries and can be smaller).
//! 3. **Determinism** — identical input ⇒ identical chunks.
//! 4. **No truncation artifact** (discovery 2574) — with truncation OFF the
//!    recovered token sum ≥ the full text count (overlaps only add); a
//!    truncated sizer would undercount and violate bounds 1-2.
//!
//! The generator builds texts from the corpus sentences (each 10-20 tokens)
//! joined with period+capital sentence boundaries. Section sizes stay far
//! below the 300-token ceiling, which makes invariants 1-2 hold by
//! construction of text-splitter's capacity search (see chunk.rs module
//! docs): every non-final chunk lands in [desired, desired + max_section]
//! ⊆ [256, 300].

use memento_parse::chunk::{Chunker, OVERLAP_RANGE};
use memento_testkit::fixtures::SPANISH_CORPUS;
use proptest::prelude::*;

/// The Spanish corpus sentences (accented, 10-20 tokens each).
const SENTENCE_POOL: [&str; 5] = SPANISH_CORPUS;

/// Generates a Spanish text of 20-90 corpus sentences, newline-free, with
/// period+capital sentence boundaries (UAX #29 markers so text-splitter
/// operates at sentence level).
fn corpus_text() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(&SENTENCE_POOL), 20..=90).prop_map(|sentences| {
        let mut out = String::new();
        for sentence in sentences {
            let sentence = sentence.trim().trim_end_matches(['.', '!', '?', ';']);
            let mut chars = sentence.chars();
            if let Some(first) = chars.next() {
                out.push_str(&first.to_uppercase().to_string());
                out.push_str(chars.as_str());
            }
            out.push_str(". ");
        }
        out
    })
}

fn chunker() -> Chunker {
    Chunker::embedded().expect("embedded spanish tokenizer loads")
}

fn tokens(chunker: &Chunker, text: &str) -> usize {
    chunker.token_count(text)
}

proptest! {
    // 64 cases: the invariants hold by construction for this generator
    // (sections stay far below the capacity ceiling); the proptest guards
    // against splitter/tokenizer regressions. 128 cases ≈ 60s in debug CI.
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn chunk_invariants(text in corpus_text()) {
        let chunker = chunker();
        let full_tokens = tokens(&chunker, &text);
        assert!(full_tokens >= 100, "generated text must be substantial");

        let chunks = chunker.chunk(&text);
        assert!(!chunks.is_empty(), "non-empty text must yield chunks");

        // Determinism (REQ-MC-003): identical input ⇒ identical chunks.
        assert_eq!(chunks, chunker.chunk(&text), "chunking must be deterministic");

        // Bounds [256, 300]; final chunk MAY be shorter.
        let mut sum = 0usize;
        for (i, chunk) in chunks.iter().enumerate() {
            let n = tokens(&chunker, &chunk.text);
            assert!(n <= 300, "chunk {i} exceeds max: {n} tokens");
            let is_final = i + 1 == chunks.len();
            if !is_final {
                assert!(n >= 256, "non-final chunk {i} under 256: {n} tokens");
            }
            sum += n;
        }

        // No truncation artifact: recovered token sum ≥ full count.
        // (With truncation ON the sizer undercounts, chunks would exceed
        // the max bound and the sum would fall short of the full count.)
        assert!(
            sum >= full_tokens,
            "recovered {sum} tokens < full {full_tokens} — truncation artifact"
        );

        // Overlap between consecutive chunks: present and within ceiling.
        if chunks.len() > 1 {
            for pair in chunks.windows(2) {
                let (a, b) = (&pair[0], &pair[1]);
                assert!(b.start < a.end, "chunks must overlap");
                let overlap = tokens(&chunker, &text[b.start..a.end]);
                assert!(
                    (1..=*OVERLAP_RANGE.end()).contains(&overlap),
                    "overlap {overlap} tokens out of ceiling"
                );
            }
        }
    }
}
