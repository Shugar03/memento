//! memento-okf — Memento RS code-knowledge adapter (REQ-CK-*).
//!
//! Indexes Rust + Python repositories through okf-rs (T-040) and exposes
//! the four knowledge layers (design): L1 bundles (source of truth),
//! L2 symbol map (T-041, mirrored into LanceDB), L3 relationship graph
//! (T-042), L4 architectural summaries (T-043). [`OkfIndex`] implements
//! [`KnowledgePort`] — read-only queries over one process-bound tenant;
//! indexing happens through the CLI (`memento code index`, T-084).
//!
//! Isolation (REQ-CK-011): every index lives under
//! `<root>/db/tenants/<tid>/okf-bundles/<project_id>/`, and every query
//! re-validates the bound context (`TENANT_FORBIDDEN` on mismatch) — a
//! process bound to tenant T2 cannot even see T1's directories, so
//! cross-tenant access resolves to not-found with zero data exposure.

pub mod index;
pub mod layers;
pub mod project_id;
pub mod queries;

use crate::layers::l1;
use crate::layers::l2::{L2SymbolIndex, SymbolRef};
use crate::layers::l3::{GraphNode, L3Graph};
use crate::layers::l4;
use async_trait::async_trait;
use memento_domain::{
    ArtifactKind, DomainError, KnowledgeArtifact, KnowledgeArtifactId, TenantContext, TenantId,
};
use memento_lancedb::LanceStore;
use memento_ports::{EmbedPort, KnowledgePort, ProjectOverview};
use okf_parser::Concept;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub use index::{
    IndexReport, SUPPORTED_LANGUAGES, SkipEntry, index_project, index_project_with_mirror,
};
pub use project_id::{is_valid_project_id, project_id_from_path};

/// One loaded project: the L1 concepts (source of truth) plus the derived
/// layers, cached in memory for sub-5ms symbol lookups and ~10ms graph
/// traversals.
struct ProjectState {
    concepts: Vec<Concept>,
    l2: L2SymbolIndex,
    l3: L3Graph,
    /// `summary.md` content (L4; REQ-CK-003 ~100ms overview).
    summary: String,
    /// Semantic sidecar: concept id → name+signature embedding
    /// (best-effort, REQ-CK-008; `None` under `--no-embeddings`).
    vectors: Option<BTreeMap<String, Vec<f32>>>,
}

/// The code-knowledge adapter: one process-bound tenant, per-project
/// lazy-loaded state, read-only queries. Indexing is CLI-driven.
pub struct OkfIndex {
    /// Storage root (production: `~/.memento` equivalent).
    root: PathBuf,
    tenant_id: TenantId,
    /// LanceDB store for the L2 symbols mirror.
    lancedb: LanceStore,
    /// Embedder for semantic search; `None` under `--no-embeddings`
    /// (REQ-CK-008 literal must still work).
    embed: Option<Arc<dyn EmbedPort>>,
    /// Project state cache (loaded on first query, refreshed at index).
    projects: Mutex<HashMap<String, Arc<ProjectState>>>,
}

impl OkfIndex {
    /// Open the adapter bound to `ctx`'s tenant under `root` (D8 layout).
    pub async fn open(
        ctx: &TenantContext,
        root: impl AsRef<Path>,
        embed: Option<Arc<dyn EmbedPort>>,
    ) -> Result<Self, DomainError> {
        let lancedb = LanceStore::open(ctx, root.as_ref()).await?;
        lancedb.ensure_schema().await?;
        Ok(Self {
            root: root.as_ref().to_path_buf(),
            tenant_id: *ctx.tenant_id(),
            lancedb,
            embed,
            projects: Mutex::new(HashMap::new()),
        })
    }

    /// Full index of `path`: L1 bundle + L2 mirror + L3 graph.json + L4
    /// summary.md + semantic sidecar (when an embedder is configured).
    /// The in-memory state is refreshed, so queries see the new index
    /// immediately.
    pub async fn index_project(
        &self,
        ctx: &TenantContext,
        path: &Path,
    ) -> Result<IndexReport, DomainError> {
        self.ensure_tenant(ctx)?;
        let bundles = self.bundles_root();
        let report = index_project_with_mirror(ctx, path, &bundles, &self.lancedb).await?;
        if report.concept_count == 0 {
            return Ok(report);
        }

        let project_dir = bundles.join(&report.project_id);

        // L4 summary (pre-computed; REQ-CK-003).
        let concepts = l1::load_bundle(&project_dir.join("bundle"))?;
        let graph = L3Graph::load(&project_dir.join("graph.json"))
            .unwrap_or_else(|_| L3Graph::from_concepts(&concepts));
        let summary = l4::compute(&concepts, &graph);
        l4::write_markdown(&summary, &project_dir.join("summary.md"))?;

        // Semantic sidecar (best-effort: embedding failure must not fail
        // indexing — literal search stays the guaranteed mode).
        if let Some(embed) = &self.embed {
            let rows = queries::semantic_rows(&concepts);
            let texts: Vec<&str> = rows.iter().map(|(_, t)| t.as_str()).collect();
            match embed.embed(&texts).await {
                Ok(vectors) if vectors.len() == rows.len() => {
                    let map: BTreeMap<String, Vec<f32>> = rows
                        .iter()
                        .zip(vectors)
                        .map(|((id, _), v)| (id.clone(), v))
                        .collect();
                    if let Ok(bytes) = serde_json::to_vec(&map) {
                        let _ = std::fs::write(project_dir.join("l2_vectors.json"), bytes);
                    }
                }
                Ok(_) => tracing::warn!("embedding count mismatch; semantic sidecar skipped"),
                Err(err) => tracing::warn!(%err, "embedding failed; semantic sidecar skipped"),
            }
        }

        let state = Arc::new(self.load_state(&report.project_id).await?);
        let mut cache = self.projects.lock().map_err(|_| DomainError::Internal {
            message: "project cache lock poisoned".into(),
        })?;
        cache.insert(report.project_id.clone(), state);
        Ok(report)
    }

    /// The tenant-scoped okf-bundles root (D8 layout).
    fn bundles_root(&self) -> PathBuf {
        self.root
            .join("db")
            .join("tenants")
            .join(self.tenant_id.to_string())
            .join("okf-bundles")
    }

    /// Guard: every operation must carry the bound tenant's context.
    fn ensure_tenant(&self, ctx: &TenantContext) -> Result<(), DomainError> {
        if ctx.tenant_id() != &self.tenant_id {
            return Err(DomainError::TenantForbidden);
        }
        Ok(())
    }

    /// Load-or-cache a project's state. A project that was never indexed
    /// resolves to `NOT_FOUND` with guidance toward indexing (REQ-CK-003).
    async fn state(
        &self,
        ctx: &TenantContext,
        project_id: &str,
    ) -> Result<Arc<ProjectState>, DomainError> {
        self.ensure_tenant(ctx)?;
        if !is_valid_project_id(project_id) {
            return Err(DomainError::InvalidInput {
                message: format!("invalid project id {project_id:?}"),
            });
        }
        {
            let cache = self.projects.lock().map_err(|_| DomainError::Internal {
                message: "project cache lock poisoned".into(),
            })?;
            if let Some(state) = cache.get(project_id) {
                return Ok(state.clone());
            }
        }
        let state = Arc::new(self.load_state(project_id).await?);
        let mut cache = self.projects.lock().map_err(|_| DomainError::Internal {
            message: "project cache lock poisoned".into(),
        })?;
        cache.insert(project_id.to_string(), state.clone());
        Ok(state)
    }

    /// Load a project's layers from disk. L1 is the source of truth;
    /// graph.json and summary.md are the persisted derived artifacts
    /// (a corrupt graph falls back to rebuilding from the bundle).
    async fn load_state(&self, project_id: &str) -> Result<ProjectState, DomainError> {
        let dir = self.bundles_root().join(project_id);
        let bundle = dir.join("bundle");
        if !dir.is_dir() {
            return Err(DomainError::NotFound {
                what: format!(
                    "code index for project '{project_id}' — run `memento code index <path>` first"
                ),
            });
        }
        if !bundle.is_dir() {
            return Err(DomainError::NotFound {
                what: format!(
                    "code index for project '{project_id}' is incomplete (missing L1 bundle)"
                ),
            });
        }
        let concepts = l1::load_bundle(&bundle)?;
        let graph = match L3Graph::load(&dir.join("graph.json")) {
            Ok(graph) => graph,
            Err(err) => {
                tracing::warn!(project = %project_id, %err, "graph.json unreadable; rebuilding from bundle");
                L3Graph::from_concepts(&concepts)
            }
        };
        let summary = l4::read_markdown(&dir.join("summary.md")).unwrap_or_default();
        let vectors = read_l2_vectors(&dir.join("l2_vectors.json"));
        let l2 = L2SymbolIndex::from_concepts(&concepts);
        Ok(ProjectState {
            concepts,
            l2,
            l3: graph,
            summary,
            vectors,
        })
    }

    /// Concept ids a symbol name resolves to (empty for unknown symbols —
    /// callers/callees/impact of an unknown name are clean empty sets).
    fn symbol_ids(
        &self,
        state: &ProjectState,
        symbol: &str,
    ) -> Result<BTreeSet<String>, DomainError> {
        Ok(state
            .l2
            .lookup(symbol)
            .map(|refs| refs.iter().map(|r| r.concept_id.clone()).collect())
            .unwrap_or_default())
    }

    /// Deterministic `name (file#Lx-Ly)` rendering of graph nodes.
    fn format_nodes(&self, state: &ProjectState, ids: &BTreeSet<String>) -> Vec<String> {
        ids.iter()
            .filter_map(|id| {
                state
                    .l3
                    .node(id)
                    .map(|n| format!("{} ({})", n.name, location_str(n)))
            })
            .collect()
    }
}

fn location_str(node: &GraphNode) -> String {
    if node.start_line == node.end_line {
        format!("{}#L{}", node.file, node.start_line)
    } else {
        format!("{}#L{}-L{}", node.file, node.start_line, node.end_line)
    }
}

/// Read the semantic sidecar (missing or corrupt → `None`, literal-only).
fn read_l2_vectors(path: &Path) -> Option<BTreeMap<String, Vec<f32>>> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// The definition artifact shape shared by symbol_lookup (REQ-CK-004).
fn symbol_artifact(project_id: &str, symbol: &str, r: &SymbolRef) -> KnowledgeArtifact {
    KnowledgeArtifact {
        project_id: project_id.to_string(),
        artifact_id: KnowledgeArtifactId::new(symbol),
        kind: ArtifactKind::Symbol,
        content: json!({
            "id": r.concept_id,
            "name": r.name,
            "kind": r.kind,
            "file": r.file,
            "start_line": r.start_line,
            "end_line": r.end_line,
            "signature": r.signature,
            "is_public": r.is_public,
        }),
    }
}

#[async_trait]
impl KnowledgePort for OkfIndex {
    /// L4 overview: reads the pre-computed summary — ~100ms order of
    /// magnitude by construction (REQ-CK-003).
    async fn project_overview(
        &self,
        ctx: &TenantContext,
        project_id: &str,
    ) -> Result<ProjectOverview, DomainError> {
        let state = self.state(ctx, project_id).await?;
        Ok(ProjectOverview {
            project_id: project_id.to_string(),
            summary: state.summary.clone(),
            artifact_count: state.concepts.len(),
        })
    }

    /// L2 lookup: `None` for unknown symbols — a clean not-found, not an
    /// error (REQ-CK-004).
    async fn symbol_lookup(
        &self,
        ctx: &TenantContext,
        project_id: &str,
        symbol: &str,
    ) -> Result<Option<KnowledgeArtifact>, DomainError> {
        let state = self.state(ctx, project_id).await?;
        let Some(refs) = state.l2.lookup(symbol) else {
            return Ok(None);
        };
        Ok(Some(symbol_artifact(project_id, symbol, &refs[0])))
    }

    /// L3: transitive callers up to depth 2 (REQ-CK-005). Unknown symbols
    /// and leaves both yield an empty set.
    async fn callers_of(
        &self,
        ctx: &TenantContext,
        project_id: &str,
        symbol: &str,
    ) -> Result<Vec<String>, DomainError> {
        let state = self.state(ctx, project_id).await?;
        let mut ids = BTreeSet::new();
        for id in self.symbol_ids(&state, symbol)? {
            ids.extend(state.l3.callers_of(&id, 2));
        }
        Ok(self.format_nodes(&state, &ids))
    }

    /// L3: transitive callees up to depth 2 (REQ-CK-005).
    async fn callees_of(
        &self,
        ctx: &TenantContext,
        project_id: &str,
        symbol: &str,
    ) -> Result<Vec<String>, DomainError> {
        let state = self.state(ctx, project_id).await?;
        let mut ids = BTreeSet::new();
        for id in self.symbol_ids(&state, symbol)? {
            ids.extend(state.l3.callees_of(&id, 2));
        }
        Ok(self.format_nodes(&state, &ids))
    }

    /// L3: reverse reachability — everything transitively affected by a
    /// change to `symbol` (REQ-CK-006).
    async fn impact(
        &self,
        ctx: &TenantContext,
        project_id: &str,
        symbol: &str,
    ) -> Result<Vec<String>, DomainError> {
        let state = self.state(ctx, project_id).await?;
        let mut ids = BTreeSet::new();
        for id in self.symbol_ids(&state, symbol)? {
            ids.extend(state.l3.impact(&id));
        }
        Ok(self.format_nodes(&state, &ids))
    }

    /// L3: module dependency edges plus explicit cycle reports
    /// (REQ-CK-007) — deterministic order.
    async fn dependencies(
        &self,
        ctx: &TenantContext,
        project_id: &str,
    ) -> Result<Vec<String>, DomainError> {
        let state = self.state(ctx, project_id).await?;
        let mut rows: Vec<String> = state
            .l3
            .module_edges()
            .iter()
            .map(|(a, b)| format!("{a} -> {b}"))
            .collect();
        for cycle in state.l3.dependency_cycles() {
            rows.push(format!("cycle: {cycle}"));
        }
        Ok(rows)
    }

    /// L2 + text search (REQ-CK-008). Semantic mode when an embedder is
    /// configured AND the index carries a sidecar; literal otherwise
    /// (works under `--no-embeddings`). Embedding failure degrades to
    /// literal — search never errors on the semantic path.
    async fn search(
        &self,
        ctx: &TenantContext,
        project_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<KnowledgeArtifact>, DomainError> {
        let state = self.state(ctx, project_id).await?;
        let limit = limit.min(100);
        if let (Some(embed), Some(vectors)) = (&self.embed, &state.vectors)
            && let Ok(out) = embed.embed(&[query]).await
            && let Some(query_vec) = out.first()
        {
            let ranked = queries::rank_by_cosine(query_vec, vectors);
            if !ranked.is_empty() {
                return Ok(queries::artifacts_for_ranked(
                    project_id,
                    &state.concepts,
                    &ranked,
                    limit,
                ));
            }
        }
        Ok(queries::search_literal(
            project_id,
            &state.concepts,
            query,
            limit,
        ))
    }

    /// L3 canonical dump `{nodes, edges}` (REQ-CK-009; Gephi/Cytoscape/
    /// Sigma-shaped: `source`/`target` endpoints, string edge kinds).
    async fn graph_dump(
        &self,
        ctx: &TenantContext,
        project_id: &str,
    ) -> Result<Value, DomainError> {
        let state = self.state(ctx, project_id).await?;
        let nodes: Vec<Value> = state
            .l3
            .nodes()
            .map(|n| {
                json!({
                    "id": n.id,
                    "kind": n.kind,
                    "name": n.name,
                    "file": n.file,
                    "start_line": n.start_line,
                    "end_line": n.end_line,
                })
            })
            .collect();
        let edges: Vec<Value> = state
            .l3
            .edges()
            .map(|e| {
                json!({
                    "source": e.source,
                    "target": e.target,
                    "kind": e.kind.as_str(),
                })
            })
            .collect();
        Ok(json!({ "nodes": nodes, "edges": edges }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memento_testkit::{StubEmbedPort, TempStore};
    use std::fs;

    /// Fixture with a real call chain in one file (module `src.chain`).
    fn write_chain_fixture(root: &Path) {
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/chain.rs"),
            "fn entry() { mid(); }\nfn mid() { leaf(); }\nfn leaf() {}\n",
        )
        .unwrap();
    }

    /// Fixture with a cross-module call (src/a.rs → src/b.rs) for the
    /// module dependency view.
    fn write_cross_module_fixture(root: &Path) {
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.rs"), "fn run() { helper(); }\n").unwrap();
        fs::write(root.join("src/b.rs"), "fn helper() {}\n").unwrap();
    }

    async fn open_index(ctx: &TenantContext, root: &Path) -> OkfIndex {
        OkfIndex::open(ctx, root, None).await.unwrap()
    }

    #[tokio::test]
    async fn full_round_trip_serves_every_port_method() {
        let ts = TempStore::new();
        let index = open_index(&ts.ctx(), ts.root()).await;
        let repo = tempfile::tempdir().unwrap();
        write_chain_fixture(repo.path());
        let ctx = ts.ctx();

        let report = index.index_project(&ctx, repo.path()).await.unwrap();
        assert_eq!(report.concept_count, 4, "module + 3 functions");
        assert!(report.symbol_count >= 3);

        // L4 overview (REQ-CK-003).
        let overview = index
            .project_overview(&ctx, &report.project_id)
            .await
            .unwrap();
        assert_eq!(overview.project_id, report.project_id);
        assert_eq!(overview.artifact_count, 4);
        assert!(overview.summary.contains("## Concepts by kind"));

        // L2 lookup (REQ-CK-004).
        let leaf = index
            .symbol_lookup(&ctx, &report.project_id, "leaf")
            .await
            .unwrap()
            .expect("leaf is indexed");
        assert_eq!(leaf.kind, ArtifactKind::Symbol);
        assert_eq!(leaf.content["kind"], "Function");
        assert_eq!(leaf.content["file"], "src/chain.rs");
        assert!(
            index
                .symbol_lookup(&ctx, &report.project_id, "nope")
                .await
                .unwrap()
                .is_none(),
            "unknown symbol → clean None (REQ-CK-004)"
        );

        // L3 callers/callees depth 2 (REQ-CK-005).
        let callers = index
            .callers_of(&ctx, &report.project_id, "leaf")
            .await
            .unwrap();
        assert!(
            callers.iter().any(|s| s.starts_with("mid (")),
            "{callers:?}"
        );
        assert!(
            callers.iter().any(|s| s.starts_with("entry (")),
            "depth-2: {callers:?}"
        );
        let callees = index
            .callees_of(&ctx, &report.project_id, "entry")
            .await
            .unwrap();
        assert!(
            callees.iter().any(|s| s.starts_with("leaf (")),
            "depth-2 callees: {callees:?}"
        );
        // Leaf symbol → empty set (REQ-CK-005).
        assert!(
            index
                .callees_of(&ctx, &report.project_id, "leaf")
                .await
                .unwrap()
                .is_empty()
        );

        // Impact (REQ-CK-006).
        let impact = index
            .impact(&ctx, &report.project_id, "leaf")
            .await
            .unwrap();
        assert_eq!(impact.len(), 2, "entry + mid affected: {impact:?}");

        // Search literal (REQ-CK-008, no embeddings).
        let hits = index
            .search(&ctx, &report.project_id, "leaf", 10)
            .await
            .unwrap();
        assert_eq!(hits[0].artifact_id.as_str(), "leaf");
        assert_eq!(hits[0].content["file"], "src/chain.rs");

        // graph_dump (REQ-CK-009): referential integrity on the JSON.
        let dump = index.graph_dump(&ctx, &report.project_id).await.unwrap();
        let nodes = dump["nodes"].as_array().unwrap();
        let edges = dump["edges"].as_array().unwrap();
        assert_eq!(nodes.len(), 4);
        assert_eq!(edges.len(), 2);
        let ids: std::collections::HashSet<&str> =
            nodes.iter().filter_map(|n| n["id"].as_str()).collect();
        for edge in edges {
            assert!(
                ids.contains(edge["source"].as_str().unwrap()),
                "source in nodes"
            );
            assert!(
                ids.contains(edge["target"].as_str().unwrap()),
                "target in nodes"
            );
        }
    }

    #[tokio::test]
    async fn unindexed_project_is_structured_not_found() {
        // REQ-CK-003 "Unindexed project": structured error with guidance.
        let ts = TempStore::new();
        let index = open_index(&ts.ctx(), ts.root()).await;
        let err = index
            .project_overview(&ts.ctx(), "deadbeefdeadbeef")
            .await
            .unwrap_err();
        assert_eq!(err.code(), "NOT_FOUND");
        let msg = err.to_string();
        assert!(msg.contains("code index"), "guides toward indexing: {msg}");
        assert!(msg.contains("memento code index"), "{msg}");
    }

    #[tokio::test]
    async fn invalid_project_id_is_rejected() {
        let ts = TempStore::new();
        let index = open_index(&ts.ctx(), ts.root()).await;
        let err = index
            .symbol_lookup(&ts.ctx(), "../escape", "x")
            .await
            .unwrap_err();
        assert_eq!(err.code(), "INVALID_INPUT");
    }

    #[tokio::test]
    async fn cross_tenant_access_resolves_to_not_found() {
        // REQ-CK-011: T2's process cannot see T1's index at all.
        let ts1 = TempStore::new();
        let index1 = open_index(&ts1.ctx(), ts1.root()).await;
        let repo = tempfile::tempdir().unwrap();
        write_chain_fixture(repo.path());
        let report = index1.index_project(&ts1.ctx(), repo.path()).await.unwrap();

        let ts2 = TempStore::new();
        let index2 = open_index(&ts2.ctx(), ts2.root()).await;
        let err = index2
            .project_overview(&ts2.ctx(), &report.project_id)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "NOT_FOUND", "no data leak across tenants");
    }

    #[tokio::test]
    async fn foreign_context_is_forbidden() {
        let ts1 = TempStore::new();
        let index1 = open_index(&ts1.ctx(), ts1.root()).await;
        let repo = tempfile::tempdir().unwrap();
        write_chain_fixture(repo.path());
        let report = index1.index_project(&ts1.ctx(), repo.path()).await.unwrap();

        let ts2 = TempStore::new();
        let err = index1
            .project_overview(&ts2.ctx(), &report.project_id)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "TENANT_FORBIDDEN");
    }

    #[tokio::test]
    async fn dependencies_report_module_edges() {
        let ts = TempStore::new();
        let index = open_index(&ts.ctx(), ts.root()).await;
        let repo = tempfile::tempdir().unwrap();
        write_cross_module_fixture(repo.path());
        let ctx = ts.ctx();

        let report = index.index_project(&ctx, repo.path()).await.unwrap();
        let deps = index.dependencies(&ctx, &report.project_id).await.unwrap();
        assert_eq!(
            deps,
            vec!["modules/src/a -> modules/src/b".to_string()],
            "cross-module call aggregated to module granularity"
        );
        assert!(
            !deps.iter().any(|d| d.starts_with("cycle:")),
            "acyclic fixture"
        );
    }

    #[tokio::test]
    async fn semantic_search_uses_the_sidecar() {
        // REQ-CK-008 semantic mode: sidecar written at index, cosine
        // ranking at query; literal still works when embedder is absent.
        let ts = TempStore::new();
        let index = OkfIndex::open(
            &ts.ctx(),
            ts.root(),
            Some(Arc::new(StubEmbedPort::default())),
        )
        .await
        .unwrap();
        let repo = tempfile::tempdir().unwrap();
        write_chain_fixture(repo.path());
        let ctx = ts.ctx();

        let report = index.index_project(&ctx, repo.path()).await.unwrap();
        let project_dir = ts
            .root()
            .join("db")
            .join("tenants")
            .join(ts.tenant_id().to_string())
            .join("okf-bundles")
            .join(&report.project_id);
        assert!(
            project_dir.join("l2_vectors.json").is_file(),
            "sidecar written when embedder configured"
        );

        let hits = index
            .search(&ctx, &report.project_id, "leaf", 10)
            .await
            .unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].artifact_id.as_str(), "leaf", "semantic top hit");
        assert!(hits[0].content.get("score").is_some(), "score attached");

        // Without an embedder the same project answers literally.
        let ts_literal = TempStore::new();
        let index_literal = open_index(&ts_literal.ctx(), ts_literal.root()).await;
        let report_l = index_literal
            .index_project(&ts_literal.ctx(), repo.path())
            .await
            .unwrap();
        let hits = index_literal
            .search(&ts_literal.ctx(), &report_l.project_id, "leaf", 10)
            .await
            .unwrap();
        assert_eq!(
            hits[0].artifact_id.as_str(),
            "leaf",
            "literal under --no-embeddings"
        );
    }
}
