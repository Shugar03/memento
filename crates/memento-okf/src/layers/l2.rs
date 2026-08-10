//! L2 layer: the symbol index (T-041, REQ-CK-004).
//!
//! One in-memory map from symbol name to every definition of that name
//! (a name can legitimately resolve to several definitions — overloads,
//! `#[cfg]` variants, same-named functions in different modules). Lookup
//! is a hash/btree hit — the < 5 ms budget is met by construction; the
//! definitions themselves come from the L1 bundle (source of truth).
//!
//! The same facts are mirrored into the tenant's LanceDB `symbols` table
//! (T-041) with replace-per-project semantics, so the storage layer can
//! answer symbol questions without the okf crate.

use okf_parser::Concept;
use std::collections::BTreeMap;

/// One resolved definition of a symbol name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRef {
    /// Stable okf concept id (e.g. `functions/src/helpers/add`).
    pub concept_id: String,
    /// Human name (duplicates possible).
    pub name: String,
    /// Fully-qualified name (module path).
    pub qualified_name: String,
    /// ConceptKind as_str (e.g. "Function", "Class").
    pub kind: String,
    /// Path relative to the project root, `/`-separated.
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: Option<String>,
    pub is_public: bool,
}

/// Deterministic symbol index: name → definitions sorted by (file, line).
#[derive(Debug, Clone, Default)]
pub struct L2SymbolIndex {
    by_name: BTreeMap<String, Vec<SymbolRef>>,
}

impl L2SymbolIndex {
    /// Build the index from the analyzed concepts (all kinds — modules
    /// and packages resolve too; they simply carry no call edges).
    pub fn from_concepts(concepts: &[Concept]) -> Self {
        let mut by_name: BTreeMap<String, Vec<SymbolRef>> = BTreeMap::new();
        for concept in concepts {
            by_name
                .entry(concept.name.clone())
                .or_default()
                .push(SymbolRef {
                    concept_id: concept.id.clone(),
                    name: concept.name.clone(),
                    qualified_name: concept.qualified_name.clone(),
                    kind: concept.kind.as_str().to_string(),
                    file: concept.location.file.clone(),
                    start_line: concept.location.start_line,
                    end_line: concept.location.end_line,
                    signature: concept.signature.clone(),
                    is_public: concept.is_public,
                });
        }
        for refs in by_name.values_mut() {
            refs.sort_by(|a, b| {
                a.file
                    .cmp(&b.file)
                    .then_with(|| a.start_line.cmp(&b.start_line))
                    .then_with(|| a.concept_id.cmp(&b.concept_id))
            });
        }
        Self { by_name }
    }

    /// Resolve a symbol name to its definitions; `None` when the symbol
    /// does not exist — a clean not-found, not an error (REQ-CK-004).
    pub fn lookup(&self, name: &str) -> Option<&[SymbolRef]> {
        self.by_name.get(name).map(Vec::as_slice)
    }

    /// Distinct symbol names, in deterministic order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(String::as_str)
    }

    /// Number of distinct symbol names.
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Every (name, definition) pair in deterministic order — feeds the
    /// LanceDB mirror rows.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &SymbolRef)> {
        self.by_name
            .iter()
            .flat_map(|(name, refs)| refs.iter().map(move |r| (name.as_str(), r)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layers::test_util::sample_concepts;
    use okf_parser::Location;

    #[test]
    fn lookup_known_symbol() {
        let idx = L2SymbolIndex::from_concepts(&sample_concepts());
        let refs = idx.lookup("alpha").unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].concept_id, "functions/alpha");
        assert_eq!(refs[0].kind, "Function");
        assert_eq!(refs[0].file, "src/lib.rs");
        assert_eq!(refs[0].start_line, 1);
        assert_eq!(refs[0].signature.as_deref(), Some("pub fn alpha() -> u32"));
        assert!(refs[0].is_public);
    }

    #[test]
    fn unknown_symbol_is_clean_not_found() {
        // REQ-CK-004: `None`, not an error.
        let idx = L2SymbolIndex::from_concepts(&[]);
        assert!(idx.lookup("missing").is_none());
        assert!(idx.is_empty());
    }

    #[test]
    fn duplicate_names_resolve_in_file_order() {
        let mut concepts = sample_concepts();
        let mut dup = concepts[0].clone();
        dup.id = "functions/alpha-2".into();
        dup.location = Location {
            file: "src/other.rs".into(),
            start_line: 9,
            end_line: 11,
        };
        concepts.push(dup);

        let idx = L2SymbolIndex::from_concepts(&concepts);
        let refs = idx.lookup("alpha").unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].concept_id, "functions/alpha");
        assert_eq!(refs[1].concept_id, "functions/alpha-2");
    }

    #[test]
    fn names_are_deterministic() {
        let mut concepts = sample_concepts();
        concepts.reverse();
        let idx = L2SymbolIndex::from_concepts(&concepts);
        let names: Vec<&str> = idx.names().collect();
        assert_eq!(names, vec!["alpha", "lib"]);
    }

    #[test]
    fn entries_cover_every_definition() {
        let idx = L2SymbolIndex::from_concepts(&sample_concepts());
        let entries: Vec<(&str, &SymbolRef)> = idx.entries().collect();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "alpha");
        assert_eq!(entries[1].0, "lib");
    }
}
