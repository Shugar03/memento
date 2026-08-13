//! Tokenizer parity golden test (perf/tokenizer-patch-vendored).
//!
//! The workspace patches `tokenizers` to a vendored 0.22.2 whose unigram trie
//! was compacted (per-node `AHashMap` children -> `Vec`, huggingface PR #2039).
//! This test proves tokenization output is byte/token identical to the
//! unpatched crate, so no tenant re-embed is required after the swap:
//!
//! - `tokenizer_parity_golden.json` was captured with the UNPATCHED registry
//!   `tokenizers 0.22.2` (see `tokenizer_version` field) over
//!   `tokenizer_parity_texts.json` (representative real + edge-case texts,
//!   ES/EN/ja/ru/CJK/emoji/code/punctuation/whitespace).
//! - This test replays the same tokenizer file + texts through the PATCHED
//!   tokenizers and asserts token-identical ids for every text.
//!
//! Golden regeneration recipe (only needed if the input texts change):
//! build a throwaway bin outside this workspace against `tokenizers = "=0.22.2"`
//! (registry), tokenize `tokenizer_parity_texts.json` with the e5 tokenizer and
//! write `tokenizer_parity_golden.json` in the same shape.

use serde_json::Value;
use std::path::PathBuf;
use tokenizers::Tokenizer;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn model_tokenizer_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../models/int8/multilingual-e5-base-int8/tokenizer.json")
}

#[test]
fn patched_tokenizer_matches_unpatched_golden() {
    let fixtures = fixtures_dir();
    let texts: Vec<String> = serde_json::from_str(
        &std::fs::read_to_string(fixtures.join("tokenizer_parity_texts.json"))
            .expect("read texts fixture"),
    )
    .expect("parse texts fixture");
    assert!(!texts.is_empty(), "fixture must contain texts");

    let golden: Value = serde_json::from_str(
        &std::fs::read_to_string(fixtures.join("tokenizer_parity_golden.json"))
            .expect("read golden fixture"),
    )
    .expect("parse golden fixture");
    assert_eq!(
        golden["texts"].as_array().map(|a| a.len()),
        Some(texts.len())
    );

    let tokenizer = Tokenizer::from_file(model_tokenizer_path()).expect("load e5 tokenizer");
    assert_eq!(
        tokenizer.get_vocab_size(true),
        golden["vocab_size"].as_u64().expect("golden vocab_size") as usize,
        "vocab size must match unpatched golden"
    );

    for (i, text) in texts.iter().enumerate() {
        let entry = &golden["entries"][i];
        let golden_ids: Vec<u32> = entry["ids"]
            .as_array()
            .unwrap_or_else(|| panic!("golden entry {i} ids"))
            .iter()
            .map(|v| v.as_u64().expect("id as u64") as u32)
            .collect();
        let enc = tokenizer
            .encode(text.as_str(), false)
            .unwrap_or_else(|e| panic!("encode text {i} {text:?}: {e}"));
        assert_eq!(
            enc.get_ids(),
            golden_ids,
            "token ids differ at text {i}: {text:?}"
        );
    }
}
