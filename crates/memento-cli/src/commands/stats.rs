//! Inspection commands (T-083): `stats` (REQ-CL-006 — chunk counts per
//! workspace + index state) and `health` (REQ-OP-001 Q3 — the CLI probe
//! the Docker compose uses).

use clap::ArgMatches;
use memento_domain::DomainError;
use serde_json::json;

use crate::output::emit_json_value;
use crate::startup::CliApp;

/// `stats`: store statistics — per-workspace chunk counts, doc/feedback/
/// symbol counts, retention horizon, code-index state (REQ-CL-006).
pub async fn run_stats(m: &ArgMatches, app: &CliApp) -> Result<(), DomainError> {
    let chunks_total = app.app.store().count_chunks(&app.ctx).await?;
    let workspace = *app.ctx.workspace_id();
    let chunks_workspace = app
        .app
        .store()
        .count_chunks_workspace(&app.ctx, &workspace)
        .await?;
    let docs = memento_lancedb::all_docs(app.app.store(), &app.ctx)
        .await?
        .len();
    let feedback = memento_lancedb::all_feedback(app.app.store(), &app.ctx)
        .await?
        .len();
    let retention_days = app.app.retention_days(&app.ctx)?;
    let code_projects = code_project_ids(app)?;
    // The symbols mirror is per project; the tenant total is the sum.
    let mut symbols = 0u64;
    for project in &code_projects {
        symbols += memento_lancedb::count_symbols(app.app.store(), &app.ctx, project).await?;
    }

    let value = json!({
        "chunks_total": chunks_total,
        "chunks_by_workspace": { workspace.to_string(): chunks_workspace },
        "docs": docs,
        "feedback": feedback,
        "symbols": symbols,
        "retention_days": retention_days,
        "code_projects": code_projects,
    });
    if m.get_flag("json") {
        emit_json_value(&value);

        Ok(())
    } else {
        println!("fragmentos: {chunks_total} (workspace {workspace}: {chunks_workspace})");
        println!("documentos: {docs}; retroalimentación: {feedback}; símbolos: {symbols}");
        println!("retención: {retention_days} días");
        println!("proyectos de código: {}", code_projects.join(", "));
        Ok(())
    }
}

/// `health`: the service probe (REQ-OP-001 Q3) — reports the bound tenant,
/// store state and retention. Requires valid credentials like every other
/// command (REQ-CL-005 scenario: nothing is served unauthenticated).
pub async fn run_health(m: &ArgMatches, app: &CliApp) -> Result<(), DomainError> {
    let chunks = app.app.store().count_chunks(&app.ctx).await?;
    let retention_days = app.app.retention_days(&app.ctx)?;
    let value = json!({
        "status": "ok",
        "tenant_id": app.ctx.tenant_id(),
        "workspace_id": app.ctx.workspace_id(),
        "agent_id": app.ctx.agent_id(),
        "chunks": chunks,
        "retention_days": retention_days,
        "embeddings": if app.no_embeddings { "disabled" } else { "enabled" },
        "storage_root": app.root,
    });
    if m.get_flag("json") {
        emit_json_value(&value);

        Ok(())
    } else {
        println!(
            "ok — tenant {}, workspace {}, {} fragmentos",
            app.ctx.tenant_id(),
            app.ctx.workspace_id(),
            chunks
        );
        Ok(())
    }
}

/// Project ids present under `<tenant>/okf-bundles/` (index state).
fn code_project_ids(app: &CliApp) -> Result<Vec<String>, DomainError> {
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
    Ok(ids)
}
