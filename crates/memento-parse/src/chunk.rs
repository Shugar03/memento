//! Deterministic chunking (T-032, REQ-MC-003).
//!
//! Splits normalized Markdown into chunks of 256-300 tokens with 10-15%
//! overlap between consecutive chunks, sized by a Spanish-tuned HuggingFace
//! tokenizer (`dccuchile/bert-base-spanish-wwm-uncased`, embedded at
//! `assets/spanish-tokenizer.json`, Apache-2.0 — see `docs/dependencies.md`).
//!
//! Locked constraints (upstream decision + discovery 2574):
//!
//! * `text-splitter` 0.32 with the `tokenizers` sizer.
//! * **Truncation MUST be disabled on the tokenizer** before it is passed
//!   to the splitter: with truncation enabled the sizer reports artificially
//!   limited counts and chunks land wrong (discovery 2574).
//! * Overlap is **absolute tokens** (26-45 = 10-15% of 256-300); the config
//!   uses 35 (≈13% of the 270 target).
//! * Deterministic: identical input + embedded tokenizer bytes ⇒ identical
//!   chunks, across processes and machines.
//!
//! Chunk sizing semantics (text-splitter 0.32): `desired` = 270 (target),
//! `max` = 300. For texts whose sentences fit in `max`, every non-final
//! chunk lands in `[desired, desired + max_sentence]` ⊆ `[256, 300]`; the
//! final chunk may be shorter (REQ-MC-003 "final chunk MAY be shorter").
//! Overlap is applied at the last section boundary inside the previous
//! chunk whose size fits the configured overlap, so the measured overlap is
//! ≤ the configured 35 tokens and ≥ 1 token whenever sections are small.

use std::ops::RangeInclusive;

use memento_domain::DomainError;
use text_splitter::{ChunkSizer, TextSplitter};
use tokenizers::Tokenizer;

/// Target chunk size in tokens (design: target 256-270).
pub const TARGET_TOKENS: usize = 270;
/// Absolute ceiling per chunk (design: max 300).
pub const MAX_TOKENS: usize = 300;
/// Absolute overlap in tokens (design: 10-15% of 256-300 ⇒ 26-45; midpoint).
pub const OVERLAP_TOKENS: usize = 35;
/// Allowed target band (design: 256-270).
pub const TARGET_RANGE: RangeInclusive<usize> = 256..=270;
/// Allowed overlap band in absolute tokens (design: 10-15% of 256-300).
pub const OVERLAP_RANGE: RangeInclusive<usize> = 26..=45;

/// Embedded Spanish tokenizer (dccuchile bert-base-spanish-wwm-uncased).
/// Vendored so chunking is deterministic and offline; ~486 KiB.
pub const SPANISH_TOKENIZER_JSON: &[u8] = include_bytes!("../assets/spanish-tokenizer.json");

/// Local `ChunkSizer` adapter over `tokenizers::Tokenizer`.
///
/// text-splitter's own `tokenizers` feature is deliberately NOT enabled:
/// it drags tokenizers 0.23 + onig into the tree while the workspace pin is
/// 0.22 (fastembed 5.x alignment, see `docs/dependencies.md`), and its
/// `ChunkSizer` impl only exists for its own 0.23 copy. This wrapper mirrors
/// the upstream counting semantics (padding skipped, truncation overflow
/// accounted) against the single 0.22 copy — truncation is disabled, so the
/// count is the true token count (discovery 2574).
#[derive(Debug, Clone)]
pub struct SpanishTokenizer(Tokenizer);

impl ChunkSizer for SpanishTokenizer {
    fn size(&self, chunk: &str) -> usize {
        let encoding = self
            .0
            .encode_fast(chunk, false)
            .expect("spanish tokenizer tokenizes utf-8");
        // Mirror upstream num_tokens_with_overflow: skip padding ids and
        // add overflow encodings (only present if truncation were enabled).
        let pad_id = self.0.get_padding().map(|p| p.pad_id);
        let count = |enc: &tokenizers::Encoding| {
            enc.get_ids()
                .iter()
                .skip_while(|&&id| pad_id.is_some_and(|p| id == p))
                .take_while(|&&id| pad_id.is_none_or(|p| id != p))
                .count()
        };
        let base = count(&encoding);
        let overflow: usize = encoding.get_overflowing().iter().map(count).sum();
        base + overflow
    }
}

impl SpanishTokenizer {
    /// Load from tokenizer JSON bytes with truncation disabled (mandatory
    /// before handing the tokenizer to the splitter — discovery 2574).
    fn from_bytes(bytes: &[u8]) -> Result<Self, DomainError> {
        let mut tokenizer = Tokenizer::from_bytes(bytes).map_err(|e| DomainError::Internal {
            message: format!("spanish tokenizer load failed: {e}"),
        })?;
        tokenizer
            .with_truncation(None)
            .map_err(|e| DomainError::Internal {
                message: format!("spanish tokenizer truncation disable failed: {e}"),
            })?;
        Ok(Self(tokenizer))
    }
}

/// A produced chunk: text plus byte offsets into the original input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub text: String,
    pub start: usize,
    pub end: usize,
}

/// The chunking configuration, exposed for contract tests and tooling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkerConfig {
    pub target_tokens: usize,
    pub max_tokens: usize,
    pub overlap_tokens: usize,
}

/// Deterministic Spanish chunker (REQ-MC-003).
#[derive(Debug)]
pub struct Chunker {
    splitter: TextSplitter<SpanishTokenizer>,
    counter: SpanishTokenizer,
    config: ChunkerConfig,
}

impl Chunker {
    /// Build the chunker from tokenizer JSON bytes. Truncation is disabled
    /// BEFORE the tokenizer is handed to the splitter (discovery 2574) —
    /// this is the correctness-critical step.
    ///
    /// # Errors
    ///
    /// * `Internal` — the tokenizer bytes cannot be parsed.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DomainError> {
        let tokenizer = SpanishTokenizer::from_bytes(bytes)?;
        let counter = tokenizer.clone();
        let config = ChunkerConfig {
            target_tokens: TARGET_TOKENS,
            max_tokens: MAX_TOKENS,
            overlap_tokens: OVERLAP_TOKENS,
        };
        let splitter = TextSplitter::new(
            text_splitter::ChunkConfig::new(
                text_splitter::ChunkCapacity::new(config.target_tokens)
                    .with_max(config.max_tokens)
                    .expect("max >= target by construction"),
            )
            .with_overlap(config.overlap_tokens)
            .expect("overlap < target by construction")
            .with_sizer(tokenizer)
            .with_trim(true),
        );
        Ok(Self {
            splitter,
            counter,
            config,
        })
    }

    /// The default chunker: the embedded Spanish tokenizer.
    ///
    /// # Errors
    ///
    /// * `Internal` — embedded tokenizer bytes are corrupt.
    pub fn embedded() -> Result<Self, DomainError> {
        Self::from_bytes(SPANISH_TOKENIZER_JSON)
    }

    /// Split text into chunks (deterministic for identical input).
    /// Empty or whitespace-only text yields an empty vector.
    pub fn chunk(&self, text: &str) -> Vec<Chunk> {
        self.splitter
            .chunk_indices(text)
            .map(|(start, chunk_text)| Chunk {
                text: chunk_text.to_string(),
                start,
                end: start + chunk_text.len(),
            })
            .collect()
    }

    /// Token count of `text` using the chunker's own tokenizer
    /// (truncation off — the real count; also the D6 context_fit budget
    /// tokenizer).
    pub fn token_count(&self, text: &str) -> usize {
        self.counter.size(text)
    }
    /// The active chunking configuration (contract surface).
    pub fn config(&self) -> ChunkerConfig {
        self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memento_testkit::fixtures::{long_spanish_doc, spanish_corpus};

    /// Builds a long Spanish text with moderate sentences (~8-12 tokens
    /// each): newline-free, period+capital sentence boundaries. Section
    /// sizes stay small, so chunk sizes are bounded by construction
    /// (see module docs: non-final chunks land in [desired, desired+max_section]).
    fn long_spanish_paragraph(sentences: usize) -> String {
        let pool = spanish_corpus();
        let mut out = String::new();
        for i in 0..sentences {
            // Corpus sentences already end with a period: strip it so the
            // builder owns the sentence-boundary punctuation.
            let sentence = pool[i % pool.len()]
                .trim()
                .trim_end_matches(['.', '!', '?', ';']);
            // Capitalize the first char (sentence boundary marker for UAX#29).
            let mut chars = sentence.chars();
            if let Some(first) = chars.next() {
                out.push_str(&first.to_uppercase().to_string());
                out.push_str(chars.as_str());
            }
            out.push_str(". ");
        }
        out
    }

    fn chunker() -> Chunker {
        Chunker::embedded().expect("embedded spanish tokenizer loads")
    }

    fn tokens(chunker: &Chunker, text: &str) -> usize {
        chunker.token_count(text)
    }

    #[test]
    fn config_contract_matches_design() {
        // REQ-MC-003 band: target 256-270, max 300, overlap 10-15%.
        let c = chunker().config();
        assert!(
            TARGET_RANGE.contains(&c.target_tokens),
            "target: {}",
            c.target_tokens
        );
        assert_eq!(c.max_tokens, 300);
        assert!(
            OVERLAP_RANGE.contains(&c.overlap_tokens),
            "overlap: {}",
            c.overlap_tokens
        );
        // The percentage reading of the band (10-15% of 256-300).
        let ratio = c.overlap_tokens as f64 / c.target_tokens as f64;
        assert!((0.10..=0.15).contains(&ratio), "ratio: {ratio:.3}");
    }

    #[test]
    fn chunks_short_text_as_one() {
        // REQ-MC-003: a document under 300 tokens produces exactly one chunk.
        let chunker = chunker();
        let text = spanish_corpus().join(" ");
        assert!(tokens(&chunker, &text) < 300, "fixture must be short");
        let chunks = chunker.chunk(&text);
        assert_eq!(chunks.len(), 1, "single chunk for short text");
        assert_eq!(chunks[0].text, text.trim(), "passthrough");
        assert_eq!(chunks[0].start, 0);
        // Truncation off: the single chunk carries the FULL token count.
        assert_eq!(tokens(&chunker, &chunks[0].text), tokens(&chunker, &text));
    }

    #[test]
    fn chunks_text_at_band_edge_as_one() {
        // ~256-300 tokens → still a single chunk (fits the capacity band).
        let chunker = chunker();
        let text = long_spanish_paragraph(14);
        assert!(
            (256..=300).contains(&tokens(&chunker, &text)),
            "fixture must sit in the band, tokens: {}",
            tokens(&chunker, &text)
        );
        let chunks = chunker.chunk(&text);
        assert_eq!(
            chunks.len(),
            1,
            "band-edge text stays whole: {}",
            chunks.len()
        );
    }

    #[test]
    fn chunks_long_text_within_bounds() {
        // REQ-MC-003: every chunk in [256,300]; final MAY be shorter.
        let chunker = chunker();
        let text = long_spanish_paragraph(240);
        assert!(tokens(&chunker, &text) > 1500, "fixture must be long");
        let chunks = chunker.chunk(&text);
        assert!(chunks.len() >= 4, "long text yields multiple chunks");

        for (i, chunk) in chunks.iter().enumerate() {
            let n = tokens(&chunker, &chunk.text);
            assert!(n <= 300, "chunk {i} over max: {n} tokens");
            let is_final = i + 1 == chunks.len();
            assert!(
                is_final || n >= 256,
                "non-final chunk {i} under 256: {n} tokens"
            );
            // Trimming: chunks never start or end with whitespace.
            assert!(
                !chunk.text.starts_with(char::is_whitespace),
                "chunk {i} trimmed start"
            );
            assert!(
                !chunk.text.ends_with(char::is_whitespace),
                "chunk {i} trimmed end"
            );
        }
    }

    #[test]
    fn chunks_long_text_with_overlap() {
        // Overlap is measured exactly: the next chunk starts at `b.start`
        // inside the previous chunk, so the duplicated region is the slice
        // [b.start, a.end) of the ORIGINAL text. Bound: never more than the
        // design ceiling (configured overlap + one section of slack stays
        // under 45 for this corpus), always at least 1 token.
        let chunker = chunker();
        let text = long_spanish_paragraph(240);
        let chunks = chunker.chunk(&text);

        for pair in chunks.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            assert!(
                b.start < a.end,
                "chunks must overlap: a=[{},{}), b starts at {}",
                a.start,
                a.end,
                b.start
            );
            let overlap_tokens = tokens(&chunker, &text[b.start..a.end]);
            assert!(
                (1..=*OVERLAP_RANGE.end()).contains(&overlap_tokens),
                "overlap {overlap_tokens} tokens out of design ceiling"
            );
        }
    }

    #[test]
    fn chunks_deterministic() {
        // REQ-MC-003: identical input ⇒ identical boundaries.
        let chunker = chunker();
        let text = long_spanish_doc();
        let a = chunker.chunk(text);
        let b = chunker.chunk(text);
        assert_eq!(a, b, "double chunk must be identical");
    }

    #[test]
    fn no_truncation_artifact() {
        // Discovery 2574: truncation must be OFF. If the tokenizer truncated
        // (e.g. at 128/512), the sizer would undercount and chunks would
        // cap well below the real text size.
        let chunker = chunker();
        let text = long_spanish_paragraph(400);
        let full_tokens = tokens(&chunker, &text);
        assert!(
            full_tokens > 2000,
            "fixture must exceed any truncation cap: {full_tokens}"
        );

        let chunks = chunker.chunk(&text);
        // With overlap, the sum of chunk tokens exceeds the full count —
        // only possible when no content was dropped.
        let sum: usize = chunks.iter().map(|c| tokens(&chunker, &c.text)).sum();
        assert!(
            sum >= full_tokens,
            "recovered tokens ({sum}) must cover the full text ({full_tokens})"
        );
        // And every non-final chunk is sized by the real count (≥256), which
        // a truncated sizer can never produce.
        for (i, c) in chunks.iter().enumerate() {
            if i + 1 != chunks.len() {
                assert!(tokens(&chunker, &c.text) >= 256);
            }
        }
    }

    #[test]
    fn empty_text_yields_no_chunks() {
        let chunker = chunker();
        assert!(chunker.chunk("").is_empty(), "empty text → no chunks");
        assert!(
            chunker.chunk("   \n\t  ").is_empty(),
            "whitespace-only → no chunks"
        );
    }

    #[test]
    fn offsets_are_byte_accurate() {
        let chunker = chunker();
        let text = long_spanish_paragraph(240);
        let chunks = chunker.chunk(&text);
        for chunk in &chunks {
            assert_eq!(
                &text[chunk.start..chunk.end],
                chunk.text,
                "byte offsets must slice the original text"
            );
        }
        // Monotonic, non-overlapping start offsets.
        for pair in chunks.windows(2) {
            assert!(pair[0].start < pair[1].start);
            assert!(pair[0].end <= pair[1].end);
        }
    }

    #[test]
    fn token_count_rounds_out_accented_text() {
        // The Spanish tokenizer must actually handle accented input
        // (fixtures are accented by design).
        let chunker = chunker();
        for sentence in spanish_corpus() {
            assert!(
                tokens(&chunker, sentence) >= 3,
                "accented sentence tokenizes: {sentence}"
            );
        }
    }
}
