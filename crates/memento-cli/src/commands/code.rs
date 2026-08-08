//! Code-knowledge commands (T-084): `code index <path>`, `code status`,
//! `code debug <project-id>`.
//!
//! Indexing is CLI-only by design ("Indexing is CLI-only
//! (`memento code index <path>`); the 8 MCP tools are read-only" —
//! design, Code Knowledge section). The CLI constructs the [`OkfIndex`]
//! directly for indexing (the guarded [`CodeFacade`] is read-only);
//! status/debug go through the facade for the REQ-TA-005 ctx guard.

use std::path::PathBuf;

use clap::ArgMatches;
use memento_domain::DomainError;
use memento_okf::OkfIndex;
use serde_json::{Value, json};

use crate::output::emit_json_value;
use crate::startup::CliApp;

/// Dispatch the `code` subtree.
pub async fn run(sub: &ArgMatches, app: &CliApp) -> Result<(), DomainError> {
    match sub.subcommand() {
        Some(("index", m)) => index(m, app).await,
        Some(("status", m)) => status(m, app).await,
        Some(("debug", m)) => debug(m, app).await,
        _ => Err(DomainError::InvalidInput {
            message: "unknown code subcommand; run 'memento code --help'".into(),
        }),
    }
}

/// `code index <path>`: full index (L1 bundle + L2 mirror + L3 graph +
/// L4 summary; REQ-CK-001/002) with the honest skip-report for
/// unsupported languages.
async fn index(m: &ArgMatches, app: &CliApp) -> Result<(), DomainError> {
    let path = PathBuf::from(m.get_one::<String>("path").expect("clap: required"));
    let index = OkfIndex::open(&app.ctx, &app.root, app.embedder()).await?;
    let report = index.index_project(&app.ctx, &path).await?;

    let skipped: Vec<Value> = report
        .files_skipped
        .iter()
        .map(|skip| json!({ "file": skip.file, "language": skip.language }))
        .collect();
    let value = json!({
        "project_id": report.project_id,
        "files_scanned": report.files_scanned,
        "files_indexed": report.files_indexed,
        "files_skipped": skipped,
        "concept_count": report.concept_count,
        "symbol_count": report.symbol_count,
        "graph_node_count": report.graph_node_count,
        "graph_edge_count": report.graph_edge_count,
        "duration_ms": report.duration_ms,
    });
    if m.get_flag("json") {
        emit_json_value(&value);

        Ok(())
    } else {
        println!(
            "indexado {}: {} archivos ({} omitidos), {} conceptos, {} símbolos, {} nodos/{} aristas ({:.1}s)",
            report.project_id,
            report.files_indexed,
            report.files_skipped.len(),
            report.concept_count,
            report.symbol_count,
            report.graph_node_count,
            report.graph_edge_count,
            report.duration_ms as f64 / 1000.0,
        );
        Ok(())
    }
}

/// `code status [--project <id>]`: layer state (L1-L4) + L4 overview.
/// The project id defaults to the single indexed project (an error when
/// there is none or several — ambiguity must be explicit).
async fn status(m: &ArgMatches, app: &CliApp) -> Result<(), DomainError> {
    let project_id = match m.get_one::<String>("project") {
        Some(raw) => raw.clone(),
        None => single_project(app)?,
    };
    let code = app.app.code(&app.ctx).await?;
    let overview = code
        .project_overview(&app.ctx, &project_id)
        .await
        .map_err(|err| match err {
            DomainError::NotFound { .. } => DomainError::NotFound {
                what: format!(
                    "code index for project {project_id} (run 'memento code index <path>')"
                ),
            },
            other => other,
        })?;

    let layers = layer_state(app, &project_id).await?;
    let value = json!({
        "project_id": project_id,
        "overview": {
            "project_id": overview.project_id,
            "summary": overview.summary,
            "artifact_count": overview.artifact_count,
        },
        "layers": {
            "l1_bundles": layers.0,
            "l2_symbols": layers.1,
            "l3_nodes": layers.2,
            "l3_edges": layers.3,
            "l4_summary": layers.4,
        },
    });
    if m.get_flag("json") {
        emit_json_value(&value);

        Ok(())
    } else {
        println!("proyecto {project_id}");
        println!("  L1 bundles: {}", if layers.0 { "sí" } else { "no" });
        println!("  L2 símbolos: {}", layers.1);
        println!("  L3 grafo: {} nodos, {} aristas", layers.2, layers.3);
        println!("  L4 summary: {}", if layers.4 { "sí" } else { "no" });
        println!("  overview: {} artefactos", overview.artifact_count);
        Ok(())
    }
}

/// `code debug <project-id>`: canonical `{nodes, edges}` graph dump with
/// referential integrity (REQ-CK-009) — every edge endpoint must be a node.
async fn debug(m: &ArgMatches, app: &CliApp) -> Result<(), DomainError> {
    let project_id = m
        .get_one::<String>("project")
        .expect("clap: required")
        .clone();
    let code = app.app.code(&app.ctx).await?;
    let graph = code.graph_dump(&app.ctx, &project_id).await?;

    let (nodes, edges) = match (&graph["nodes"], &graph["edges"]) {
        (Value::Array(nodes), Value::Array(edges)) => (nodes, edges),
        _ => {
            return Err(DomainError::Internal {
                message: format!("graph_dump returned a non-canonical shape for {project_id}"),
            });
        }
    };
    let node_ids: std::collections::HashSet<&str> =
        nodes.iter().filter_map(|n| n["id"].as_str()).collect();
    let dangling: Vec<&str> = edges
        .iter()
        .flat_map(|e| [e["source"].as_str(), e["target"].as_str()])
        .flatten()
        .filter(|endpoint| !node_ids.contains(endpoint))
        .collect();

    let value = json!({
        "project_id": project_id,
        "graph": graph,
        "referential_integrity": dangling.is_empty(),
        "dangling_endpoints": dangling,
    });
    if m.get_flag("json") {
        emit_json_value(&value);

        Ok(())
    } else {
        println!(
            "grafo {project_id}: {} nodos, {} aristas",
            nodes.len(),
            edges.len()
        );
        println!(
            "integridad referencial: {}",
            if dangling.is_empty() { "ok" } else { "FALLA" }
        );
        Ok(())
    }
}

/// The single indexed project id, or a structured error when there is
/// none / several (ambiguity must be explicit).
fn single_project(app: &CliApp) -> Result<String, DomainError> {
    let bundles = app.app.tenant_dir().join("okf-bundles");
    let mut ids = Vec::new();
    if bundles.is_dir() {
        for entry in std::fs::read_dir(&bundles).map_err(DomainError::from)? {
            let entry = entry.map_err(DomainError::from)?;
            if entry.path().is_dir() {
                ids.push(entry.file_name().to_string_lossy().to_string());
            }
        }
    }
    ids.sort();
    match ids.as_slice() {
        [only] => Ok(only.clone()),
        [] => Err(DomainError::NotFound {
            what: "code index (run 'memento code index <path>' first)".into(),
        }),
        _ => Err(DomainError::InvalidInput {
            message: format!(
                "multiple projects indexed ({}); pass --project <id>",
                ids.join(", ")
            ),
        }),
    }
}

/// On-disk layer state for a project: `(l1_bundles, l2_symbols,
/// l3_nodes, l3_edges, l4_summary)`.
async fn layer_state(
    app: &CliApp,
    project_id: &str,
) -> Result<(bool, u64, usize, usize, bool), DomainError> {
    let dir = app.app.tenant_dir().join("okf-bundles").join(project_id);
    let l1 = dir.join("bundle").is_dir();
    let l4 = dir.join("summary.md").is_file();
    let (l3_nodes, l3_edges) = match std::fs::read(dir.join("graph.json")) {
        Ok(raw) => {
            let graph: Value =
                serde_json::from_slice(&raw).map_err(|err| DomainError::Internal {
                    message: format!("corrupt graph.json for {project_id}: {err}"),
                })?;
            (
                graph["nodes"].as_array().map_or(0, Vec::len),
                graph["edges"].as_array().map_or(0, Vec::len),
            )
        }
        Err(_) => (0, 0),
    };
    let l2_symbols = memento_lancedb::count_symbols(app.app.store(), &app.ctx, project_id).await?;
    Ok((l1, l2_symbols, l3_nodes, l3_edges, l4))
}
