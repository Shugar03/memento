//! Memory commands (T-083): search, get-chunk, feedback, delete,
//! context-fit (REQ-CL-001 operational parity with the MCP surface,
//! REQ-MS-006 — same semantics, same canonical JSON fields).
//!
//! The workspace defaults to the process-bound tenant's workspace
//! (REQ-TA-004); `--workspace` overrides per call (REQ-MR-006 mandatory
//! per-call scope).

use std::str::FromStr;

use clap::ArgMatches;
use memento_application::context_fit::ContextFitRequest;
use memento_domain::{ChunkId, DocId, DomainError, SourceKind, WorkspaceId};
use memento_i18n::I18n;
use memento_ports::{DeleteScope, SearchFilters, SearchQuery};
use serde_json::{Value, json};

use crate::commands::confirm_ceremony;
use crate::output::{emit_json, emit_json_value};
use crate::startup::CliApp;

/// Parse the `--rrf-k` fusion constant (defaults to the standard 60).
fn parse_rrf_k(m: &ArgMatches) -> Result<f32, DomainError> {
    let raw = m
        .get_one::<String>("rrf-k")
        .expect("clap: default")
        .parse::<f32>()
        .map_err(|_| DomainError::InvalidInput {
            message: "--rrf-k must be a number".into(),
        })?;
    if !raw.is_finite() || raw <= 0.0 {
        return Err(DomainError::InvalidInput {
            message: "--rrf-k must be a positive finite number".into(),
        });
    }
    Ok(raw)
}

/// `search <query> [--top-k N] [--workspace <uuid>] [--rrf] [--doc-id]
/// [--source ...]` (REQ-MR-001/002/003/006).
pub async fn run_search(m: &ArgMatches, app: &CliApp) -> Result<(), DomainError> {
    let query = m.get_one::<String>("query").expect("clap: required");
    let top_k: usize = m
        .get_one::<String>("top-k")
        .expect("clap: default")
        .parse()
        .map_err(|_| DomainError::InvalidInput {
            message: "--top-k must be a non-negative integer".into(),
        })?;
    let workspace = workspace_of(m, app)?;
    let filters = filters_of(m)?;
    let rrf_k = parse_rrf_k(m)?;

    let hits = app
        .app
        .search(
            &app.ctx,
            SearchQuery {
                query: query.clone(),
                top_k,
                workspace_id: workspace,
                rrf_enabled: m.get_flag("rrf"),
                rrf_k,
                filters,
            },
        )
        .await?;

    let hits_json: Vec<Value> = hits.iter().map(hit_json).collect();
    if m.get_flag("json") {
        emit_json_value(&json!({ "hits": hits_json }));
        Ok(())
    } else {
        if hits.is_empty() {
            println!("sin resultados");
        }
        for (hit, value) in hits.iter().zip(&hits_json) {
            println!(
                "[{}] {:.3} {}",
                hit.chunk_id,
                hit.score,
                value["text"].as_str().unwrap_or("")
            );
        }
        Ok(())
    }
}

/// `get-chunk <chunk-id>` (REQ-MR-005: unknown → null, no existence leak).
pub async fn run_get_chunk(m: &ArgMatches, app: &CliApp) -> Result<(), DomainError> {
    let raw = m.get_one::<String>("chunk").expect("clap: required");
    let id = ChunkId::from_str(raw).map_err(|_| DomainError::InvalidInput {
        message: format!("chunk id is not a valid uuid: {raw}"),
    })?;
    let chunk = app.app.get_chunk(&app.ctx, &id).await?;
    if m.get_flag("json") {
        let value = match chunk {
            Some(chunk) => json!({ "chunk": chunk_json(&chunk) }),
            None => json!({ "chunk": null }),
        };
        emit_json_value(&value);

        Ok(())
    } else {
        match chunk {
            Some(chunk) => {
                println!("{}", chunk.text);
                println!("doc_id: {}", chunk.doc_id);
            }
            None => println!("fragmento no encontrado"),
        }
        Ok(())
    }
}

/// `feedback <chunk-id> --useful|--not-useful [--reason <text>]`
/// (REQ-ML-001: unknown chunk → structured CHUNK_NOT_FOUND).
pub async fn run_feedback(m: &ArgMatches, app: &CliApp) -> Result<(), DomainError> {
    let raw = m.get_one::<String>("chunk").expect("clap: required");
    let id = ChunkId::from_str(raw).map_err(|_| DomainError::InvalidInput {
        message: format!("chunk id is not a valid uuid: {raw}"),
    })?;
    let useful = m.get_flag("useful");
    let reason = m.get_one::<String>("reason").cloned();
    app.app.feedback(&app.ctx, id, useful, reason).await?;
    if m.get_flag("json") {
        emit_json_value(&json!({ "ok": true }));
        Ok(())
    } else {
        println!("retroalimentación registrada");
        Ok(())
    }
}

/// `delete --chunk <id>|--doc <id>|--workspace <id>|--tenant` (REQ-ML-002).
/// The tenant scope is tenant-wide destruction → confirmation ceremony
/// (design: destructive ops get a ceremony; abort leaves data intact).
pub async fn run_delete(m: &ArgMatches, app: &CliApp, i18n: &I18n) -> Result<(), DomainError> {
    let scope = if let Some(raw) = m.get_one::<String>("chunk") {
        DeleteScope::Chunk {
            id: ChunkId::from_str(raw).map_err(|_| DomainError::InvalidInput {
                message: format!("chunk id is not a valid uuid: {raw}"),
            })?,
        }
    } else if let Some(raw) = m.get_one::<String>("doc") {
        DeleteScope::Doc {
            id: DocId::from_str(raw).map_err(|_| DomainError::InvalidInput {
                message: format!("doc id is not a valid uuid: {raw}"),
            })?,
        }
    } else if let Some(raw) = m.get_one::<String>("workspace") {
        DeleteScope::Workspace {
            id: WorkspaceId::from_str(raw).map_err(|_| DomainError::InvalidInput {
                message: format!("workspace id is not a valid uuid: {raw}"),
            })?,
        }
    } else if m.get_flag("tenant") {
        confirm_ceremony(i18n, app.ctx.tenant_id(), m.get_flag("json"))?;
        DeleteScope::Tenant {
            id: *app.ctx.tenant_id(),
        }
    } else {
        return Err(DomainError::InvalidInput {
            message: "delete requires one of --chunk, --doc, --workspace or --tenant".into(),
        });
    };

    let report = app.app.delete(&app.ctx, scope).await?;
    if m.get_flag("json") {
        emit_json(&report)
    } else {
        println!(
            "eliminados: {} ({} bytes)",
            report.deleted_count, report.freed_bytes
        );
        Ok(())
    }
}

/// `context-fit <query> --budget <tokens> [--top-k N] [--workspace] [--rrf]`
/// (REQ-MR-004, design D6: greedy score + feedback bonus ≤ +0.5).
pub async fn run_context_fit(m: &ArgMatches, app: &CliApp) -> Result<(), DomainError> {
    let query = m.get_one::<String>("query").expect("clap: required");
    let budget: usize = m
        .get_one::<String>("budget")
        .expect("clap: required")
        .parse()
        .map_err(|_| DomainError::InvalidInput {
            message: "--budget must be a non-negative integer".into(),
        })?;
    let top_k: usize = m
        .get_one::<String>("top-k")
        .expect("clap: default")
        .parse()
        .map_err(|_| DomainError::InvalidInput {
            message: "--top-k must be a non-negative integer".into(),
        })?;
    let result = app
        .app
        .context_fit(
            &app.ctx,
            ContextFitRequest {
                query: query.clone(),
                budget_tokens: budget,
                top_k,
                workspace_id: workspace_of(m, app)?,
                rrf_enabled: m.get_flag("rrf"),
                rrf_k: parse_rrf_k(m)?,
            },
        )
        .await?;

    let chunks: Vec<Value> = result.chunks.iter().map(hit_json).collect();
    if m.get_flag("json") {
        emit_json_value(&json!({
            "chunks": chunks,
            "total_tokens": result.total_tokens,
            "reason": result.reason,
        }));
        Ok(())
    } else {
        println!(
            "contexto: {} fragmentos, {} tokens{}",
            result.chunks.len(),
            result.total_tokens,
            result.reason.map(|r| format!(" ({r})")).unwrap_or_default()
        );
        Ok(())
    }
}

// ---- helpers ----------------------------------------------------------------

fn workspace_of(m: &ArgMatches, app: &CliApp) -> Result<WorkspaceId, DomainError> {
    match m.get_one::<String>("workspace") {
        Some(raw) => WorkspaceId::from_str(raw).map_err(|_| DomainError::InvalidInput {
            message: format!("workspace is not a valid uuid: {raw}"),
        }),
        // REQ-TA-004: the process-bound workspace by default.
        None => Ok(*app.ctx.workspace_id()),
    }
}

fn filters_of(m: &ArgMatches) -> Result<Option<SearchFilters>, DomainError> {
    let doc_id = m
        .get_one::<String>("doc-id")
        .map(|raw| {
            DocId::from_str(raw).map_err(|_| DomainError::InvalidInput {
                message: format!("doc-id is not a valid uuid: {raw}"),
            })
        })
        .transpose()?;
    let source = m
        .get_one::<String>("source")
        .map(|raw| parse_source(raw))
        .transpose()?;
    if doc_id.is_none() && source.is_none() {
        Ok(None)
    } else {
        Ok(Some(SearchFilters { doc_id, source }))
    }
}

fn parse_source(raw: &str) -> Result<SourceKind, DomainError> {
    match raw {
        "text" => Ok(SourceKind::Text),
        "markdown" => Ok(SourceKind::Markdown),
        other => other
            .strip_prefix("document:")
            .map(|ext| SourceKind::Document(ext.to_string()))
            .ok_or_else(|| DomainError::InvalidInput {
                message: format!(
                    "source must be 'text', 'markdown' or 'document:<ext>', got: {raw}"
                ),
            }),
    }
}

/// Canonical hit JSON — same fields as the MCP `memory.search` DTO
/// (REQ-MS-006 equivalence): `{chunk_id, score, text, provenance{source,
/// doc_id, chunk_id, created_at, embedding_model_version, tenant_id,
/// workspace_id, agent_id}}`.
fn hit_json(hit: &memento_ports::SearchHit) -> Value {
    json!({
        "chunk_id": hit.chunk_id,
        "score": hit.score,
        "text": hit.text,
        "provenance": provenance_json(&hit.provenance),
    })
}

/// Canonical chunk JSON — same fields as the MCP `memory.get_chunk` DTO.
fn chunk_json(chunk: &memento_domain::MemoryChunk) -> Value {
    json!({
        "id": chunk.id,
        "doc_id": chunk.doc_id,
        "text": chunk.text,
        "created_at": chunk.created_at,
        "provenance": provenance_json(&chunk.provenance),
    })
}

fn provenance_json(p: &memento_domain::Provenance) -> Value {
    let source = match &p.source {
        SourceKind::Text => "text".to_string(),
        SourceKind::Markdown => "markdown".to_string(),
        SourceKind::Document(ext) => format!("document:{ext}"),
    };
    json!({
        "source": source,
        "doc_id": p.doc_id,
        "chunk_id": p.chunk_id,
        "created_at": p.created_at,
        "embedding_model_version": p.embedding_model_version,
        "tenant_id": p.tenant_id,
        "workspace_id": p.workspace_id,
        "agent_id": p.agent_id,
    })
}
