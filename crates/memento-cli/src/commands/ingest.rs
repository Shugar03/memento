//! Ingest commands (T-083): `ingest text`, `ingest document`, and
//! `ingest bulk` with the per-file report (REQ-CL-002).
//!
//! # Bulk-ingest boundary (T-080, threat-matrix row 5)
//!
//! [`collect_bulk_files`] applies the design's canonical-path check —
//! "workspace-relative only":
//!
//! 1. **Glob-traversal gate**: the root argument may not contain a `..`
//!    component (`dir/../outside` is rejected before any walk).
//! 2. **Canonical-path containment gate**: every collected entry
//!    canonicalizes and MUST stay under the canonical workspace root.
//!    A symlink (or any entry) resolving outside is a structured
//!    `INVALID_INPUT` naming the offending path — the whole run aborts,
//!    zero files leave the workspace.
//!
//! Individual per-file ingest failures (unsupported formats, corrupt
//! documents) do NOT abort the batch: they land in the report with their
//! reason (REQ-CL-002 scenario).

use std::collections::HashSet;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use clap::ArgMatches;
use memento_domain::{DomainError, SourceKind};
use memento_ports::{IngestDocumentRequest, IngestTextRequest, Metadata};
use serde_json::{Value, json};

use crate::output::{emit_json, emit_json_value};
use crate::startup::CliApp;

/// Source-hint for a file path: `.md`/`.markdown` → Markdown, `.txt` →
/// Text, everything else → `Document(<ext>)` (the parse boundary routes
/// through the anydoc allowlist or the fallback parser).
fn source_hint_for(path: &Path) -> SourceKind {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "md" | "markdown" => SourceKind::Markdown,
        "txt" => SourceKind::Text,
        other => SourceKind::Document(other.to_string()),
    }
}

/// Parse the `--source` override ("text" | "markdown" | "document:<ext>"),
/// same contract as the MCP `memory.ingest_document` parameter.
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

fn doc_id_of(m: &ArgMatches) -> Result<Option<memento_domain::DocId>, DomainError> {
    m.get_one::<String>("doc-id")
        .map(|raw| {
            raw.parse().map_err(|_| DomainError::InvalidInput {
                message: format!("doc-id is not a valid uuid: {raw}"),
            })
        })
        .transpose()
}

/// Dispatch the `ingest` subtree.
pub async fn run(sub: &ArgMatches, app: &CliApp) -> Result<(), DomainError> {
    match sub.subcommand() {
        Some(("text", m)) => ingest_text(m, app).await,
        Some(("document", m)) => ingest_document(m, app).await,
        Some(("bulk", m)) => bulk(m, app).await,
        _ => Err(DomainError::InvalidInput {
            message: "unknown ingest subcommand; run 'memento ingest --help'".into(),
        }),
    }
}

/// `ingest text <text> [--doc-id <uuid>]` (REQ-MC-001).
async fn ingest_text(m: &ArgMatches, app: &CliApp) -> Result<(), DomainError> {
    let text = m.get_one::<String>("text").expect("clap: required");
    let result = app
        .app
        .ingest_text(
            &app.ctx,
            IngestTextRequest {
                text: text.clone(),
                doc_id: doc_id_of(m)?,
                metadata: None,
            },
        )
        .await?;
    if m.get_flag("json") {
        emit_json(&result)
    } else {
        println!(
            "documento {}: {} fragmentos",
            result.doc_id,
            result.chunk_ids.len()
        );
        Ok(())
    }
}

/// `ingest document <file> [--source <text|markdown|document:ext>]
/// [--doc-id <uuid>]` (REQ-MC-002).
async fn ingest_document(m: &ArgMatches, app: &CliApp) -> Result<(), DomainError> {
    let file = PathBuf::from(m.get_one::<String>("file").expect("clap: required"));
    let source_hint = match m.get_one::<String>("source") {
        Some(raw) => parse_source(raw)?,
        None => source_hint_for(&file),
    };
    let mut blob = Vec::new();
    std::fs::File::open(&file)
        .map_err(DomainError::from)?
        .read_to_end(&mut blob)
        .map_err(DomainError::from)?;
    let result = app
        .app
        .ingest_document(
            &app.ctx,
            IngestDocumentRequest {
                blob,
                source_hint,
                doc_id: doc_id_of(m)?,
                metadata: Some(Metadata(
                    json!({ "source_path": file.to_string_lossy() })
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                )),
            },
        )
        .await?;
    if m.get_flag("json") {
        emit_json(&result)
    } else {
        println!(
            "documento {}: {} fragmentos",
            result.doc_id,
            result.chunk_ids.len()
        );
        Ok(())
    }
}

/// `ingest bulk <dir>`: walk the directory (canonical-path gate, T-080),
/// ingest every file, and report per-file outcomes. Individual failures do
/// not abort the batch (REQ-CL-002).
async fn bulk(m: &ArgMatches, app: &CliApp) -> Result<(), DomainError> {
    let dir = PathBuf::from(m.get_one::<String>("dir").expect("clap: required"));
    // The walker aborts the WHOLE run on any escape (T-080): nothing
    // outside the workspace is ever ingested.
    let files = collect_bulk_files(&dir)?;

    let mut report = Vec::with_capacity(files.len());
    let (mut ingested, mut failed) = (0usize, 0usize);
    for file in &files {
        let blob = match std::fs::read(file) {
            Ok(blob) => blob,
            Err(err) => {
                failed += 1;
                report.push(json!({
                    "file": rel_label(&dir, file),
                    "status": "error",
                    "reason": format!("{err}"),
                }));
                continue;
            }
        };
        let hint = source_hint_for(file);
        match app
            .app
            .ingest_document(
                &app.ctx,
                IngestDocumentRequest {
                    blob,
                    source_hint: hint,
                    doc_id: None,
                    metadata: None,
                },
            )
            .await
        {
            Ok(result) => {
                ingested += 1;
                report.push(json!({
                    "file": rel_label(&dir, file),
                    "status": "ok",
                    "doc_id": result.doc_id,
                    "chunks": result.chunk_ids.len(),
                }));
            }
            Err(err) => {
                failed += 1;
                report.push(json!({
                    "file": rel_label(&dir, file),
                    "status": "error",
                    "reason": err.to_string(),
                    "code": err.code(),
                }));
            }
        }
    }

    let summary = json!({
        "total": files.len(),
        "ingested": ingested,
        "failed": failed,
        "files": Value::Array(report),
    });
    if m.get_flag("json") {
        emit_json_value(&summary);
        Ok(())
    } else {
        println!(
            "bulk: {} archivos, {} ingestados, {} fallidos",
            files.len(),
            ingested,
            failed
        );
        for file in summary["files"].as_array().expect("report array") {
            println!(
                "  {}: {}",
                file["file"].as_str().unwrap_or("?"),
                file["status"].as_str().unwrap_or("?")
            );
        }
        Ok(())
    }
}

/// Relative label of a file under the bulk root (display only).
fn rel_label(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .map(|rel| rel.to_string_lossy().to_string())
        .unwrap_or_else(|_| file.to_string_lossy().to_string())
}

// ---- bulk walker (T-080) ----------------------------------------------------

/// Collect every regular file under `root`, deterministically sorted, with
/// the canonical-path boundary enforced on the whole walk (threat-matrix
/// row 5: symlink escape, glob traversal).
///
/// # Errors
///
/// * `InvalidInput` — the root argument contains a `..` component (glob
///   traversal), is not a directory, or an entry canonicalizes outside the
///   workspace root (symlink escape; names the offending path).
/// * `Io` — the tree cannot be read.
pub fn collect_bulk_files(root: &Path) -> Result<Vec<PathBuf>, DomainError> {
    // Gate 1: glob/traversal — no `..` component may appear in the root
    // argument. The workspace is what the user named; traversal strings
    // are rejected before any filesystem work.
    if root.components().any(|c| c == Component::ParentDir) {
        return Err(DomainError::InvalidInput {
            message: format!(
                "bulk ingest path contains '..' traversal: {}",
                root.display()
            ),
        });
    }
    let canonical_root = root.canonicalize().map_err(DomainError::from)?;
    if !canonical_root.is_dir() {
        return Err(DomainError::InvalidInput {
            message: format!("bulk ingest root is not a directory: {}", root.display()),
        });
    }

    let mut files = Vec::new();
    let mut seen_dirs: HashSet<PathBuf> = HashSet::new();
    let mut stack = vec![canonical_root.clone()];
    while let Some(dir) = stack.pop() {
        if !seen_dirs.insert(dir.clone()) {
            continue; // symlink cycle guard
        }
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .map_err(DomainError::from)?
            .collect::<Result<_, _>>()
            .map_err(DomainError::from)?;
        // Deterministic order → deterministic errors and report lines.
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            let file_type = entry.file_type().map_err(DomainError::from)?;
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            // Gate 2: canonical containment — a symlink (or anything else)
            // resolving outside the workspace aborts the whole run.
            let canonical = canonical_within(&canonical_root, &path)?;
            if canonical.is_dir() {
                stack.push(canonical);
            } else {
                files.push(canonical);
            }
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

/// The canonical-path containment gate: `entry` must canonicalize under
/// `root` (workspace-relative only). Exposed publicly so the T-080 RED
/// tests can pin the boundary directly; the walker applies it to every
/// entry.
///
/// # Errors
///
/// * `Io` — the entry cannot be canonicalized (missing path, symlink
///   cycle, ...).
/// * `InvalidInput` — the canonical entry escapes the workspace root.
pub fn canonical_within(root: &Path, entry: &Path) -> Result<PathBuf, DomainError> {
    // Both sides canonicalized: on Windows `canonicalize` returns
    // `\\?\`-verbatim paths, and `Path::starts_with` between a verbatim
    // and a plain path is always false — comparing canonical-to-canonical
    // keeps the prefix check honest.
    let canonical_root = root.canonicalize().map_err(DomainError::from)?;
    let canonical = entry.canonicalize().map_err(DomainError::from)?;
    if canonical.starts_with(&canonical_root) {
        Ok(canonical)
    } else {
        Err(DomainError::InvalidInput {
            message: format!(
                "bulk ingest entry escapes the workspace root: {}",
                entry.display()
            ),
        })
    }
}
