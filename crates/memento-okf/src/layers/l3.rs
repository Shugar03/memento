//! L3 layer: the relationship graph (T-042, REQ-CK-005/006/007/009).
//!
//! Built from the L1 bundle's concepts and their relationships:
//!
//! * **call graph** — `Calls` edges (with `CalledBy` mirrors) power
//!   `callers_of`/`callees_of` (depth-bounded BFS, REQ-CK-005) and
//!   `impact` (unbounded reverse reachability, REQ-CK-006);
//! * **module graph** — `Imports` edges between module/package concepts
//!   power `dependencies` with explicit cycle detection (REQ-CK-007);
//! * **canonical dump** — `graph.json` (`{nodes, edges}`, Gephi/
//!   Cytoscape/Sigma-style) persists the graph; every edge references
//!   nodes present in the dump (REQ-CK-009 referential integrity).
//!
//! Edges whose target leaves the bundle (external imports) are excluded
//! from the dump and traversal — they carry no node to anchor on.

use okf_parser::{Concept, ConceptKind, RelationKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

/// Edge kinds surfaced in the canonical dump (relation kinds minus the
/// `CalledBy` mirror).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum EdgeKind {
    Calls,
    Imports,
    Inherits,
    Implements,
    MemberOf,
    DependsOn,
}

impl EdgeKind {
    /// Snake_case label used in `graph.json` (sigma/gephi-friendly).
    pub fn as_str(&self) -> &'static str {
        match self {
            EdgeKind::Calls => "calls",
            EdgeKind::Imports => "imports",
            EdgeKind::Inherits => "inherits",
            EdgeKind::Implements => "implements",
            EdgeKind::MemberOf => "member_of",
            EdgeKind::DependsOn => "depends_on",
        }
    }

    fn from_relation(kind: RelationKind) -> Option<Self> {
        match kind {
            RelationKind::Calls => Some(EdgeKind::Calls),
            RelationKind::Imports => Some(EdgeKind::Imports),
            RelationKind::Inherits => Some(EdgeKind::Inherits),
            RelationKind::Implements => Some(EdgeKind::Implements),
            RelationKind::MemberOf => Some(EdgeKind::MemberOf),
            RelationKind::DependsOn => Some(EdgeKind::DependsOn),
            // Mirror of `Calls`; the dump keeps one direction only.
            RelationKind::CalledBy => None,
        }
    }
}

/// One node of the knowledge graph (one concept).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: String,
    /// ConceptKind as_str (e.g. "Function", "Module").
    pub kind: String,
    pub name: String,
    /// Path relative to the project root, `/`-separated.
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
}

/// One directed edge of the knowledge graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphEdge {
    pub source: String,
    pub target: String,
    pub kind: EdgeKind,
}

/// In-memory knowledge graph with adjacency maps for fast traversal.
#[derive(Debug, Clone, Default)]
pub struct L3Graph {
    nodes: BTreeMap<String, GraphNode>,
    edges: Vec<GraphEdge>,
    /// concept id → ids that call it (reverse of `Calls`).
    callers: HashMap<String, Vec<String>>,
    /// concept id → ids it calls (`Calls` targets).
    callees: HashMap<String, Vec<String>>,
    /// module/package id → module/package ids it imports (internal only).
    module_imports: HashMap<String, Vec<String>>,
}

impl L3Graph {
    /// Build the graph from the analyzed concepts. Deterministic: nodes
    /// are id-sorted, edges are (source, target, kind)-sorted, adjacency
    /// vectors are sorted and deduplicated.
    pub fn from_concepts(concepts: &[Concept]) -> Self {
        let mut graph = L3Graph::default();
        for concept in concepts {
            graph.nodes.insert(
                concept.id.clone(),
                GraphNode {
                    id: concept.id.clone(),
                    kind: concept.kind.as_str().to_string(),
                    name: concept.name.clone(),
                    file: concept.location.file.clone(),
                    start_line: concept.location.start_line,
                    end_line: concept.location.end_line,
                },
            );
        }

        let mut seen: HashSet<(String, String, EdgeKind)> = HashSet::new();
        for concept in concepts {
            for rel in &concept.relationships {
                let Some(kind) = EdgeKind::from_relation(rel.kind) else {
                    continue;
                };
                if !graph.nodes.contains_key(&rel.target) {
                    continue; // external target (outside the bundle)
                }
                if !seen.insert((concept.id.clone(), rel.target.clone(), kind)) {
                    continue;
                }
                graph.edges.push(GraphEdge {
                    source: concept.id.clone(),
                    target: rel.target.clone(),
                    kind,
                });
            }
        }
        graph.edges.sort_by(|a, b| {
            a.source
                .cmp(&b.source)
                .then_with(|| a.target.cmp(&b.target))
                .then_with(|| a.kind.cmp(&b.kind))
        });
        graph.rebuild_adjacency();
        graph
    }

    /// Persist as canonical `{nodes, edges}` JSON (graph.json).
    pub fn save(&self, path: &Path) -> Result<(), DomainError> {
        let json = serde_json::json!({
            "nodes": self.nodes.values().collect::<Vec<_>>(),
            "edges": &self.edges,
        });
        let bytes = serde_json::to_vec_pretty(&json).map_err(|err| DomainError::Internal {
            message: format!("graph serialization failed: {err}"),
        })?;
        std::fs::write(path, bytes).map_err(|source| DomainError::Io { source })
    }

    /// Load a persisted graph (graph.json) and rebuild the adjacency maps.
    pub fn load(path: &Path) -> Result<Self, DomainError> {
        let raw = std::fs::read_to_string(path).map_err(|source| DomainError::Io { source })?;
        let value: serde_json::Value =
            serde_json::from_str(&raw).map_err(|err| DomainError::Parse {
                message: format!("graph.json malformed: {err}"),
            })?;
        let nodes: Vec<GraphNode> =
            serde_json::from_value(value["nodes"].clone()).map_err(|err| DomainError::Parse {
                message: format!("graph.json nodes malformed: {err}"),
            })?;
        let edges: Vec<GraphEdge> =
            serde_json::from_value(value["edges"].clone()).map_err(|err| DomainError::Parse {
                message: format!("graph.json edges malformed: {err}"),
            })?;

        let mut graph = L3Graph::default();
        for node in nodes {
            graph.nodes.insert(node.id.clone(), node);
        }
        graph.edges = edges;
        graph.rebuild_adjacency();
        Ok(graph)
    }

    /// Rebuild `callers`/`callees`/`module_imports` from the current
    /// nodes + edges (used after construction and after load).
    fn rebuild_adjacency(&mut self) {
        self.callers.clear();
        self.callees.clear();
        self.module_imports.clear();

        for edge in &self.edges {
            match edge.kind {
                EdgeKind::Calls => {
                    self.callees
                        .entry(edge.source.clone())
                        .or_default()
                        .push(edge.target.clone());
                    self.callers
                        .entry(edge.target.clone())
                        .or_default()
                        .push(edge.source.clone());
                }
                EdgeKind::Imports => {
                    let module_kind = ConceptKind::Module.as_str();
                    let package_kind = ConceptKind::Package.as_str();
                    let is_module = |id: &str| {
                        self.nodes
                            .get(id)
                            .is_some_and(|n| n.kind == module_kind || n.kind == package_kind)
                    };
                    if is_module(&edge.source) && is_module(&edge.target) {
                        self.module_imports
                            .entry(edge.source.clone())
                            .or_default()
                            .push(edge.target.clone());
                    }
                }
                _ => {}
            }
        }
        for vec in self.callers.values_mut() {
            vec.sort_unstable();
            vec.dedup();
        }
        for vec in self.callees.values_mut() {
            vec.sort_unstable();
            vec.dedup();
        }
        for vec in self.module_imports.values_mut() {
            vec.sort_unstable();
            vec.dedup();
        }
    }

    /// Direct callers of `id` (depth 1), transitive up to `depth` levels
    /// (depth 2 = callers of callers; REQ-CK-005). Deterministic.
    pub fn callers_of(&self, id: &str, depth: usize) -> BTreeSet<String> {
        self.reachable(id, depth, &self.callers)
    }

    /// Direct callees of `id` (depth 1), transitive up to `depth` levels.
    pub fn callees_of(&self, id: &str, depth: usize) -> BTreeSet<String> {
        self.reachable(id, depth, &self.callees)
    }

    /// BFS up to `depth` levels over `adjacency`; `self` never returned.
    fn reachable(
        &self,
        id: &str,
        depth: usize,
        adjacency: &HashMap<String, Vec<String>>,
    ) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        if depth == 0 {
            return out;
        }
        let mut visited: HashSet<&str> = HashSet::from([id]);
        let mut frontier: Vec<&str> = vec![id];
        for _ in 0..depth {
            let mut next: Vec<&str> = Vec::new();
            for node in &frontier {
                if let Some(neighbors) = adjacency.get(*node) {
                    for neighbor in neighbors {
                        if visited.insert(neighbor.as_str()) {
                            out.insert(neighbor.clone());
                            next.push(neighbor);
                        }
                    }
                }
            }
            frontier = next;
        }
        out
    }

    /// Reverse reachability over the call graph: every concept that
    /// transitively calls `id` (REQ-CK-006 impact analysis).
    pub fn impact(&self, id: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut visited: HashSet<&str> = HashSet::from([id]);
        let mut stack: Vec<&str> = vec![id];
        while let Some(node) = stack.pop() {
            if let Some(callers) = self.callers.get(node) {
                for caller in callers {
                    if visited.insert(caller.as_str()) {
                        out.insert(caller.clone());
                        stack.push(caller);
                    }
                }
            }
        }
        out
    }

    /// Internal module→module import edges, deterministic order
    /// (REQ-CK-007 dependency view).
    pub fn module_edges(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (source, targets) in &self.module_imports {
            for target in targets {
                out.push((source.clone(), target.clone()));
            }
        }
        out.sort();
        out
    }

    /// Detect module-level dependency cycles; each cycle is reported
    /// once as `"A -> B -> A"`, rotated to its lexicographically smallest
    /// start and sorted (REQ-CK-007 "cycle reported explicitly").
    pub fn dependency_cycles(&self) -> Vec<String> {
        const WHITE: u8 = 0;
        const GRAY: u8 = 1;
        const BLACK: u8 = 2;

        let mut modules: Vec<&str> = self.module_ids().collect();
        modules.sort_unstable();

        let mut color: HashMap<&str, u8> = HashMap::new();
        let mut stack: Vec<&str> = Vec::new();
        let mut cycles: BTreeSet<String> = BTreeSet::new();

        fn visit<'a>(
            graph: &'a L3Graph,
            node: &'a str,
            color: &mut HashMap<&'a str, u8>,
            stack: &mut Vec<&'a str>,
            cycles: &mut BTreeSet<String>,
        ) {
            color.insert(node, GRAY);
            stack.push(node);
            if let Some(neighbors) = graph.module_imports.get(node) {
                for neighbor in neighbors {
                    match color.get(neighbor.as_str()).copied().unwrap_or(WHITE) {
                        WHITE => visit(graph, neighbor, color, stack, cycles),
                        GRAY => {
                            // neighbor is on the current path: cycle found.
                            if let Some(start) = stack.iter().position(|s| *s == neighbor) {
                                let mut path: Vec<&str> = stack[start..].to_vec();
                                path.push(neighbor);
                                cycles.insert(canonical_cycle(&path));
                            }
                        }
                        _ => {}
                    }
                }
            }
            stack.pop();
            color.insert(node, BLACK);
        }

        for module in modules {
            if color.get(module).copied().unwrap_or(WHITE) == WHITE {
                visit(self, module, &mut color, &mut stack, &mut cycles);
            }
        }
        cycles.into_iter().collect()
    }

    /// Ids of every module/package node (the dependency-graph vertex set).
    pub fn module_ids(&self) -> impl Iterator<Item = &str> {
        let module_kind = ConceptKind::Module.as_str();
        let package_kind = ConceptKind::Package.as_str();
        self.nodes.values().filter_map(move |n| {
            (n.kind == module_kind || n.kind == package_kind).then_some(n.id.as_str())
        })
    }

    pub fn node(&self, id: &str) -> Option<&GraphNode> {
        self.nodes.get(id)
    }

    /// Every node in id order (the dump's vertex list).
    pub fn nodes(&self) -> impl Iterator<Item = &GraphNode> {
        self.nodes.values()
    }

    pub fn nodes_len(&self) -> usize {
        self.nodes.len()
    }

    pub fn edges_len(&self) -> usize {
        self.edges.len()
    }

    /// REQ-CK-009: every edge endpoint must resolve to a node in the dump.
    pub fn referential_integrity(&self) -> bool {
        self.edges
            .iter()
            .all(|e| self.nodes.contains_key(&e.source) && self.nodes.contains_key(&e.target))
    }
}

/// Rotate a cycle path so it starts at its lexicographically smallest id
/// (`A -> B -> A` and `B -> A -> B` collapse to one canonical string).
fn canonical_cycle(path: &[&str]) -> String {
    let min = path
        .iter()
        .enumerate()
        .min_by_key(|(_, id)| **id)
        .map(|(idx, _)| idx)
        .unwrap_or(0);
    path[min..]
        .iter()
        .chain(path[..min].iter())
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(" -> ")
}

use memento_domain::DomainError;

#[cfg(test)]
mod tests {
    use super::*;
    use okf_parser::{Language, Location, Relationship};

    fn concept(id: &str, kind: ConceptKind, name: &str, rels: Vec<Relationship>) -> Concept {
        Concept {
            id: id.into(),
            kind,
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

    fn calls(_from: &str, to: &str) -> Relationship {
        Relationship {
            kind: RelationKind::Calls,
            target: to.into(),
            target_display: to.into(),
        }
    }

    fn imports(_from: &str, to: &str) -> Relationship {
        Relationship {
            kind: RelationKind::Imports,
            target: to.into(),
            target_display: to.into(),
        }
    }

    fn fn_concept(id: &str, name: &str, rels: Vec<Relationship>) -> Concept {
        concept(id, ConceptKind::Function, name, rels)
    }

    fn module_concept(id: &str, name: &str, rels: Vec<Relationship>) -> Concept {
        concept(id, ConceptKind::Module, name, rels)
    }

    /// Chain A → B → C (three concepts, two call edges).
    fn chain() -> Vec<Concept> {
        vec![
            fn_concept("f/a", "a", vec![calls("f/a", "f/b")]),
            fn_concept("f/b", "b", vec![calls("f/b", "f/c")]),
            fn_concept("f/c", "c", vec![]),
        ]
    }

    #[test]
    fn depth_2_callers_and_callees() {
        // REQ-CK-005: transitive traversal up to depth 2.
        let graph = L3Graph::from_concepts(&chain());

        assert_eq!(
            graph.callers_of("f/c", 1),
            BTreeSet::from(["f/b".to_string()])
        );
        assert_eq!(
            graph.callers_of("f/c", 2),
            BTreeSet::from(["f/a".to_string(), "f/b".to_string()])
        );
        assert_eq!(
            graph.callees_of("f/a", 2),
            BTreeSet::from(["f/b".to_string(), "f/c".to_string()])
        );
    }

    #[test]
    fn leaf_has_empty_caller_set() {
        // REQ-CK-005 "Leaf symbol": empty set, not an error.
        let graph = L3Graph::from_concepts(&chain());
        assert!(graph.callers_of("f/a", 2).is_empty());
        assert!(graph.callees_of("f/c", 2).is_empty());
    }

    #[test]
    fn depth_zero_returns_nothing() {
        let graph = L3Graph::from_concepts(&chain());
        assert!(graph.callers_of("f/c", 0).is_empty());
    }

    #[test]
    fn impact_reaches_transitive_callers() {
        // REQ-CK-006: E1 → M → S and E2 → S; changing S affects all of them.
        let concepts = vec![
            fn_concept("f/s", "s", vec![]),
            fn_concept("f/m", "m", vec![calls("f/m", "f/s")]),
            fn_concept("f/e1", "e1", vec![calls("f/e1", "f/m")]),
            fn_concept("f/e2", "e2", vec![calls("f/e2", "f/s")]),
        ];
        let graph = L3Graph::from_concepts(&concepts);
        assert_eq!(
            graph.impact("f/s"),
            BTreeSet::from(["f/e1".to_string(), "f/e2".to_string(), "f/m".to_string()])
        );
        // A leaf caller has no impact set of its own beyond itself.
        assert!(graph.impact("f/e1").is_empty());
    }

    #[test]
    fn cycle_a_b_a_is_reported() {
        // REQ-CK-007: explicit cycle reporting.
        let concepts = vec![
            module_concept("m/a", "A", vec![imports("m/a", "m/b")]),
            module_concept("m/b", "B", vec![imports("m/b", "m/a")]),
        ];
        let graph = L3Graph::from_concepts(&concepts);
        assert_eq!(graph.dependency_cycles(), vec!["m/a -> m/b -> m/a"]);
        assert_eq!(
            graph.module_edges(),
            vec![
                ("m/a".to_string(), "m/b".to_string()),
                ("m/b".to_string(), "m/a".to_string())
            ]
        );
    }

    #[test]
    fn acyclic_modules_report_no_cycles() {
        let concepts = vec![
            module_concept("m/a", "A", vec![imports("m/a", "m/b")]),
            module_concept("m/b", "B", vec![]),
        ];
        let graph = L3Graph::from_concepts(&concepts);
        assert!(graph.dependency_cycles().is_empty());
    }

    #[test]
    fn external_imports_are_excluded_from_dump_and_graph() {
        // Imports pointing outside the bundle carry no node — REQ-CK-009
        // requires edges to reference nodes in the dump, so they drop out
        // of the canonical graph (cycle detection over internal modules).
        let concepts = vec![
            module_concept("m/a", "A", vec![imports("m/a", "external/crate")]),
            module_concept("m/b", "B", vec![]),
        ];
        let graph = L3Graph::from_concepts(&concepts);
        assert_eq!(graph.edges_len(), 0);
        assert!(graph.dependency_cycles().is_empty());
    }

    #[test]
    fn save_load_round_trip_preserves_graph() {
        let graph = L3Graph::from_concepts(&chain());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.json");
        graph.save(&path).unwrap();

        let loaded = L3Graph::load(&path).unwrap();
        assert_eq!(loaded.nodes_len(), graph.nodes_len());
        assert_eq!(loaded.edges_len(), graph.edges_len());
        assert_eq!(loaded.callers_of("f/c", 2), graph.callers_of("f/c", 2));
        assert_eq!(loaded.impact("f/c"), graph.impact("f/c"));
        assert!(loaded.referential_integrity());
    }

    #[test]
    fn referential_integrity_holds_always() {
        // REQ-CK-009: every edge endpoint resolves to a node (mixed graph:
        // calls + imports + external targets that get dropped).
        let concepts = vec![
            fn_concept(
                "f/a",
                "a",
                vec![calls("f/a", "f/b"), calls("f/a", "f/ghost")],
            ),
            fn_concept("f/b", "b", vec![]),
            module_concept("m/x", "X", vec![imports("m/x", "m/y")]),
            module_concept("m/y", "Y", vec![]),
        ];
        let graph = L3Graph::from_concepts(&concepts);
        assert!(graph.referential_integrity());
        assert_eq!(graph.edges_len(), 2, "ghost edge dropped");
    }

    #[test]
    fn malformed_graph_file_is_a_structured_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.json");
        std::fs::write(&path, "{not json").unwrap();
        let err = L3Graph::load(&path).unwrap_err();
        assert_eq!(err.code(), "PARSE");
    }

    #[test]
    fn adjacency_is_deduplicated() {
        // Two call sites to the same callee must not duplicate edges.
        let concepts = vec![
            fn_concept("f/a", "a", vec![calls("f/a", "f/b"), calls("f/a", "f/b")]),
            fn_concept("f/b", "b", vec![]),
        ];
        let graph = L3Graph::from_concepts(&concepts);
        assert_eq!(graph.edges_len(), 1);
        assert_eq!(
            graph.callees_of("f/a", 1),
            BTreeSet::from(["f/b".to_string()])
        );
        assert_eq!(
            graph.callers_of("f/b", 1),
            BTreeSet::from(["f/a".to_string()])
        );
    }
}
