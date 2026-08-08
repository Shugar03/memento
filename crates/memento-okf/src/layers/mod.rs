//! The four knowledge layers (design: L1 bundles → L2 symbol map → L3
//! relationship graph → L4 architectural summary). L1 is the source of
//! truth; L2/L3/L4 are derived artifacts persisted beside it.

pub mod l1;
pub mod l2;
pub mod l3;

#[cfg(test)]
pub(crate) mod test_util {
    use okf_parser::{Concept, ConceptKind, Language, Location};

    /// A tiny, hand-shaped concept set used by layer tests (no file IO).
    pub(crate) fn sample_concepts() -> Vec<Concept> {
        vec![
            Concept {
                id: "functions/alpha".into(),
                kind: ConceptKind::Function,
                language: Language::Rust,
                name: "alpha".into(),
                qualified_name: "crate::alpha".into(),
                description: Some("first function".into()),
                location: Location {
                    file: "src/lib.rs".into(),
                    start_line: 1,
                    end_line: 3,
                },
                signature: Some("pub fn alpha() -> u32".into()),
                tags: vec![],
                is_public: true,
                generated_at: None,
                relationships: vec![],
            },
            Concept {
                id: "modules/lib".into(),
                kind: ConceptKind::Module,
                language: Language::Rust,
                name: "lib".into(),
                qualified_name: "crate".into(),
                description: None,
                location: Location {
                    file: "src/lib.rs".into(),
                    start_line: 1,
                    end_line: 1,
                },
                signature: None,
                tags: vec![],
                is_public: true,
                generated_at: None,
                relationships: vec![],
            },
        ]
    }
}
