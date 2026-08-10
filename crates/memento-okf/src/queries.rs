//! Code search over the L1 concept set (T-043, REQ-CK-008).
//!
//! * **Literal** — substring ranking over symbol names, qualified names,
//!   signatures and descriptions; works under `--no-embeddings` (REQ-CK-008).
//! * **Semantic** — cosine ranking of the query embedding against
//!   name+signature vectors precomputed at index time (best-effort quality
//!   with the MVP embedding model, per REQ-CK-008).
//!
//! Both modes are deterministic: ties break by concept id, results are
//! id-sorted within each rank bucket.

use memento_domain::{ArtifactKind, KnowledgeArtifact, KnowledgeArtifactId};
use okf_parser::Concept;
use serde_json::json;
use std::collections::BTreeMap;

/// Rank a literal query against every concept; returns up to `limit`
/// artifacts (REQ-CK-008 literal mode). Ranking: exact name match →
/// name prefix → name substring → qualified-name substring → signature/
/// description substring; ties by concept id. An empty query returns
/// nothing.
pub fn search_literal(
    project_id: &str,
    concepts: &[Concept],
    query: &str,
    limit: usize,
) -> Vec<KnowledgeArtifact> {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() || limit == 0 {
        return Vec::new();
    }

    let rank = |c: &Concept| -> Option<u8> {
        let name = c.name.to_ascii_lowercase();
        if name == query {
            return Some(0);
        }
        if name.starts_with(&query) {
            return Some(1);
        }
        if name.contains(&query) {
            return Some(2);
        }
        if c.qualified_name.to_ascii_lowercase().contains(&query) {
            return Some(3);
        }
        let body = c
            .signature
            .iter()
            .chain(c.description.iter())
            .map(|s| s.to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(" ");
        body.contains(&query).then_some(4)
    };

    let mut scored: Vec<(u8, &Concept)> = concepts
        .iter()
        .filter_map(|c| rank(c).map(|r| (r, c)))
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.id.cmp(&b.1.id)));
    scored.truncate(limit);
    scored
        .into_iter()
        .map(|(_, c)| artifact(project_id, c))
        .collect()
}

/// Precomputed rows for the semantic sidecar: the text embedded per
/// concept (name + signature when present, else name).
pub fn semantic_rows(concepts: &[Concept]) -> Vec<(String, String)> {
    concepts
        .iter()
        .map(|c| {
            let text = match &c.signature {
                Some(sig) => format!("{} {}", c.name, sig),
                None => c.name.clone(),
            };
            (c.id.clone(), text)
        })
        .collect()
}

/// Rank concept ids by cosine similarity to the query vector; ties by id.
pub fn rank_by_cosine(
    query_vec: &[f32],
    vectors: &BTreeMap<String, Vec<f32>>,
) -> Vec<(String, f32)> {
    let mut out: Vec<(String, f32)> = vectors
        .iter()
        .map(|(id, vec)| (id.clone(), cosine(query_vec, vec)))
        .collect();
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    out
}

/// Cosine similarity of two non-empty vectors (0.0 when either is empty).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// Map ranked concept ids to artifacts, keeping only concepts known to
/// the caller (sidecar can outlive a re-analysis). Sorted by score desc.
pub fn artifacts_for_ranked(
    project_id: &str,
    concepts: &[Concept],
    ranked: &[(String, f32)],
    limit: usize,
) -> Vec<KnowledgeArtifact> {
    let by_id: BTreeMap<&str, &Concept> = concepts.iter().map(|c| (c.id.as_str(), c)).collect();
    ranked
        .iter()
        .filter_map(|(id, score)| by_id.get(id.as_str()).map(|c| (c, *score)))
        .take(limit)
        .map(|(c, score)| {
            let mut a = artifact(project_id, c);
            a.content["score"] = json!(score);
            a
        })
        .collect()
}

/// One search hit artifact: `kind: Symbol`, id = symbol name, content =
/// definition facts (REQ-CK-004/008 "returned with locations").
fn artifact(project_id: &str, c: &Concept) -> KnowledgeArtifact {
    KnowledgeArtifact {
        project_id: project_id.to_string(),
        artifact_id: KnowledgeArtifactId::new(&c.name),
        kind: ArtifactKind::Symbol,
        content: json!({
            "id": c.id,
            "name": c.name,
            "kind": c.kind.as_str(),
            "file": c.location.file,
            "start_line": c.location.start_line,
            "end_line": c.location.end_line,
            "signature": c.signature,
            "is_public": c.is_public,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layers::test_util::sample_concepts;

    #[test]
    fn literal_finds_exact_name_first() {
        let hits = search_literal("pid", &sample_concepts(), "alpha", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].artifact_id.as_str(), "alpha");
        assert_eq!(hits[0].kind, ArtifactKind::Symbol);
        assert_eq!(hits[0].content["file"], "src/lib.rs");
        assert_eq!(hits[0].content["start_line"], 1);
        assert_eq!(hits[0].project_id, "pid");
    }

    #[test]
    fn literal_is_case_insensitive_and_substring() {
        let hits = search_literal("pid", &sample_concepts(), "ALP", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].artifact_id.as_str(), "alpha");
    }

    #[test]
    fn literal_respects_limit() {
        let concepts = sample_concepts();
        let hits = search_literal("pid", &concepts, "a", 1);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn empty_or_blank_query_returns_nothing() {
        let concepts = sample_concepts();
        assert!(search_literal("pid", &concepts, "", 10).is_empty());
        assert!(search_literal("pid", &concepts, "   ", 10).is_empty());
        assert!(search_literal("pid", &concepts, "alpha", 0).is_empty());
    }

    #[test]
    fn no_match_returns_nothing() {
        assert!(search_literal("pid", &sample_concepts(), "zzz_nope", 10).is_empty());
    }

    #[test]
    fn cosine_similarity_orders_identical_above_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![1.0, 0.0];
        let c = vec![0.0, 1.0];
        assert_eq!(cosine(&a, &b), 1.0);
        assert_eq!(cosine(&a, &c), 0.0);
        assert_eq!(cosine(&a, &[]), 0.0);
    }

    #[test]
    fn rank_by_cosine_is_deterministic() {
        let mut vectors = BTreeMap::new();
        vectors.insert("f/alpha".to_string(), vec![1.0, 0.0]);
        vectors.insert("f/beta".to_string(), vec![0.0, 1.0]);
        let ranked = rank_by_cosine(&[1.0, 0.0], &vectors);
        assert_eq!(ranked[0].0, "f/alpha");
        assert_eq!(ranked[1].0, "f/beta");
        let again = rank_by_cosine(&[1.0, 0.0], &vectors);
        assert_eq!(ranked, again);
    }

    #[test]
    fn artifacts_for_ranked_skips_unknown_ids() {
        let concepts = sample_concepts();
        let ranked = vec![
            ("functions/alpha".to_string(), 0.9),
            ("functions/ghost".to_string(), 0.8),
            ("modules/lib".to_string(), 0.7),
        ];
        let hits = artifacts_for_ranked("pid", &concepts, &ranked, 10);
        assert_eq!(hits.len(), 2, "ghost id dropped");
        assert_eq!(hits[0].artifact_id.as_str(), "alpha");
        let top_score = hits[0].content["score"].as_f64().unwrap();
        assert!(
            (top_score - 0.9).abs() < 1e-6,
            "score preserved: {top_score}"
        );
        assert!(hits[1].content["score"].as_f64().unwrap() < top_score);
    }

    #[test]
    fn semantic_rows_embed_name_and_signature() {
        let concepts = sample_concepts();
        let rows = semantic_rows(&concepts);
        let alpha = rows.iter().find(|(id, _)| id == "functions/alpha").unwrap();
        assert_eq!(alpha.1, "alpha pub fn alpha() -> u32");
    }
}
