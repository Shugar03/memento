//! L4 layer: pre-computed architectural summary (T-043, REQ-CK-003).
//!
//! [`compute`] derives a deterministic Markdown summary from the analyzed
//! concepts + L3 graph: concept counts by kind, top modules by fan-in,
//! top functions by call-site count (complexity proxy), dependency cycles,
//! and the module list. Written as `summary.md` at index time so
//! `code.project_overview` is a file read — the ~100 ms order of magnitude
//! holds by construction.

use crate::layers::l3::L3Graph;
use memento_domain::DomainError;
use okf_parser::{Concept, ConceptKind, RelationKind};
use std::collections::BTreeMap;
use std::path::Path;

/// The pre-computed summary for one project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct L4Summary {
    pub markdown: String,
    /// Number of concepts summarized (the overview's artifact count).
    pub artifact_count: usize,
}

const TOP_N: usize = 5;

/// Compute the deterministic summary. Every section is sorted; ties are
/// broken by id, so two runs over the same bundle produce identical text.
pub fn compute(concepts: &[Concept], graph: &L3Graph) -> L4Summary {
    // Counts by kind (only kinds present, sorted).
    let mut by_kind: BTreeMap<&'static str, usize> = BTreeMap::new();
    for concept in concepts {
        *by_kind.entry(concept.kind.as_str()).or_default() += 1;
    }

    // Fan-in: internal Imports targeting each module/package id.
    let mut fan_in: BTreeMap<&str, usize> = BTreeMap::new();
    for concept in concepts {
        for rel in &concept.relationships {
            if rel.kind == RelationKind::Imports {
                *fan_in.entry(rel.target.as_str()).or_default() += 1;
            }
        }
    }
    let mut top_modules: Vec<(&str, usize)> = fan_in.iter().map(|(k, v)| (*k, *v)).collect();
    top_modules.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));

    // Complexity proxy: distinct call sites per function/method.
    let mut out_degree: BTreeMap<&str, usize> = BTreeMap::new();
    for concept in concepts {
        if !matches!(concept.kind, ConceptKind::Function | ConceptKind::Method) {
            continue;
        }
        let calls = concept
            .relationships
            .iter()
            .filter(|r| r.kind == RelationKind::Calls)
            .count();
        if calls > 0 {
            out_degree.insert(concept.id.as_str(), calls);
        }
    }
    let mut top_functions: Vec<(&str, usize)> = out_degree.iter().map(|(k, v)| (*k, *v)).collect();
    top_functions.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));

    let cycles = graph.dependency_cycles();
    let modules: Vec<String> = graph.module_ids().map(str::to_string).collect();

    let mut markdown = String::new();
    markdown.push_str("# Architectural summary\n\n");
    markdown.push_str(&format!(
        "{} artifacts across {} modules, {} dependency cycles.\n\n",
        concepts.len(),
        modules.len(),
        cycles.len()
    ));

    markdown.push_str("## Concepts by kind\n\n| Kind | Count |\n|---|---|\n");
    for (kind, count) in &by_kind {
        markdown.push_str(&format!("| {kind} | {count} |\n"));
    }

    markdown.push_str("\n## Top modules by fan-in\n\n");
    if top_modules.is_empty() {
        markdown.push_str("- none\n");
    } else {
        for (id, count) in top_modules.iter().take(TOP_N) {
            markdown.push_str(&format!("1. `{id}` ({count} imports)\n"));
        }
    }

    markdown.push_str("\n## Top functions by call sites\n\n");
    if top_functions.is_empty() {
        markdown.push_str("- none\n");
    } else {
        for (id, count) in top_functions.iter().take(TOP_N) {
            markdown.push_str(&format!("1. `{id}` ({count} calls)\n"));
        }
    }

    markdown.push_str("\n## Dependency cycles\n\n");
    if cycles.is_empty() {
        markdown.push_str("- none\n");
    } else {
        for cycle in &cycles {
            markdown.push_str(&format!("- {cycle}\n"));
        }
    }

    markdown.push_str("\n## Modules\n\n");
    for module in &modules {
        markdown.push_str(&format!("- {module}\n"));
    }

    L4Summary {
        markdown,
        artifact_count: concepts.len(),
    }
}

/// Write the summary as `summary.md` (index-time artifact, REQ-CK-003).
pub fn write_markdown(summary: &L4Summary, path: &Path) -> Result<(), DomainError> {
    std::fs::write(path, &summary.markdown).map_err(|source| DomainError::Io { source })
}

/// Read a previously written `summary.md` (the overview's text source).
pub fn read_markdown(path: &Path) -> Result<String, DomainError> {
    std::fs::read_to_string(path).map_err(|source| DomainError::Io { source })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layers::test_util::sample_concepts;
    use okf_parser::{Language, Location, Relationship};

    fn fn_concept(id: &str, name: &str, rels: Vec<Relationship>) -> Concept {
        Concept {
            id: id.into(),
            kind: ConceptKind::Function,
            language: Language::Rust,
            name: name.into(),
            qualified_name: name.into(),
            description: None,
            location: Location {
                file: format!("{name}.rs"),
                start_line: 1,
                end_line: 2,
            },
            signature: None,
            tags: vec![],
            is_public: true,
            generated_at: None,
            relationships: rels,
        }
    }

    #[test]
    fn summary_covers_counts_cycles_and_modules() {
        let concepts = vec![
            fn_concept(
                "f/alpha",
                "alpha",
                vec![Relationship {
                    kind: RelationKind::Calls,
                    target: "f/beta".into(),
                    target_display: "beta".into(),
                }],
            ),
            fn_concept("f/beta", "beta", vec![]),
        ];
        let graph = L3Graph::from_concepts(&concepts);
        let summary = compute(&concepts, &graph);

        assert_eq!(summary.artifact_count, 2);
        assert!(summary.markdown.contains("2 artifacts across 0 modules"));
        assert!(summary.markdown.contains("| Function | 2 |"));
        assert!(summary.markdown.contains("`f/alpha` (1 calls)"));
        assert!(summary.markdown.contains("## Dependency cycles"));
        assert!(summary.markdown.contains("- none"));
    }

    #[test]
    fn summary_is_deterministic() {
        let concepts = sample_concepts();
        let graph = L3Graph::from_concepts(&concepts);
        let a = compute(&concepts, &graph);
        let b = compute(&concepts, &graph);
        assert_eq!(a.markdown, b.markdown);
    }

    #[test]
    fn write_read_round_trip() {
        let concepts = sample_concepts();
        let graph = L3Graph::from_concepts(&concepts);
        let summary = compute(&concepts, &graph);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("summary.md");
        write_markdown(&summary, &path).unwrap();
        assert_eq!(read_markdown(&path).unwrap(), summary.markdown);
    }

    #[test]
    fn top_n_is_bounded_and_sorted() {
        let concepts: Vec<Concept> = (0..10)
            .map(|i| {
                fn_concept(
                    &format!("f/f{i}"),
                    &format!("f{i}"),
                    vec![Relationship {
                        kind: RelationKind::Calls,
                        target: "f/beta".into(),
                        target_display: "beta".into(),
                    }],
                )
            })
            .collect();
        let graph = L3Graph::from_concepts(&concepts);
        let summary = compute(&concepts, &graph);
        let lines: Vec<&str> = summary
            .markdown
            .lines()
            .filter(|l| l.contains("(1 calls)"))
            .collect();
        assert_eq!(lines.len(), TOP_N, "top-N bounded: {lines:?}");
    }
}
