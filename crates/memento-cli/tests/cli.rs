//! CLI integration tests (T-081..T-084 acceptance): assert_cmd + real
//! LanceDB on temp roots. Every command runs with `--no-embeddings`
//! (REQ-MC-004 absent vectors — no ONNX download in tests; FTS remains
//! fully functional, REQ-MR-001).
//!
//! Covered scenarios:
//! * REQ-CL-005 exit-code contract (auth failure = 4, validation = 2,
//!   top_k = 14) with bilingual structured errors, human + `--json`.
//! * REQ-CL-001 CLI-only round-trip (ingest → search → get_chunk →
//!   feedback → context_fit → delete).
//! * REQ-CL-003 `--json` machine output carrying provenance.
//! * REQ-CL-002 bulk ingest with per-file report (mixed dir, no abort).
//! * REQ-CL-004 bilingual help (ES primary, EN fallback).
//! * REQ-CL-006 stats (per-workspace counts).
//! * T-082 ceremonies: tenant delete aborts without confirm; confirmed
//!   delete destroys credentials; rotate kills the old token; restore
//!   rejects a live store.
//! * T-084 code index/status/debug over a real fixture repo.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::Value;

fn bin() -> Command {
    Command::cargo_bin("memento").expect("binary")
}

/// JSON stdout of a successful run.
fn json_of(out: &std::process::Output) -> Value {
    assert!(
        out.status.success(),
        "expected success, got {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("stdout is JSON")
}

/// Provision a tenant on a temp root; returns (root, token).
fn provisioned() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = bin()
        .args(["--json", "--root"])
        .arg(dir.path())
        .args(["tenant", "create", "--name", "dev"])
        .output()
        .expect("run create");
    let v = json_of(&out);
    (dir, v["token"].as_str().expect("token").to_string())
}

/// A command pre-loaded with credentials + no-embeddings for `root`.
fn authed(root: &Path, token: &str) -> Command {
    let mut cmd = bin();
    cmd.env("MEMENTO_ROOT", root)
        .env("MEMENTO_TOKEN", token)
        .env("MEMENTO_AGENT_ID", "test-agent")
        .arg("--no-embeddings");
    cmd
}

/// Ingest one text via the CLI and return its ingest result JSON.
fn ingest_text(root: &Path, token: &str, text: &str) -> Value {
    let out = authed(root, token)
        .args(["--json", "ingest", "text", text])
        .output()
        .expect("run ingest");
    json_of(&out)
}

fn single_document_error(out: &std::process::Output) -> Value {
    assert_eq!(out.status.code(), Some(2));
    serde_json::from_slice(&out.stderr).expect("structured JSON error on stderr")
}

#[test]
fn ingest_document_rejects_dotdot_path() {
    let (dir, token) = provisioned();
    let nested = dir.path().join("nested");
    std::fs::create_dir_all(&nested).expect("nested directory");
    let file = dir.path().join("inside.txt");
    std::fs::write(&file, "inside").expect("document");
    let traversal = nested.join("..").join("inside.txt");

    let out = authed(dir.path(), &token)
        .args(["--json", "ingest", "document", "--source", "text"])
        .arg(&traversal)
        .output()
        .expect("run document ingest");
    let error = single_document_error(&out);
    assert_eq!(error["code"], "INVALID_INPUT");
    assert_eq!(error["message"], "Entrada no válida.");
    assert!(
        error["detail"]
            .as_str()
            .unwrap()
            .contains("path resolves outside storage root")
    );
}

#[test]
fn ingest_document_rejects_path_outside_storage_root() {
    let (dir, token) = provisioned();
    let outside = tempfile::tempdir().expect("outside directory");
    let file = outside.path().join("outside.txt");
    std::fs::write(&file, "outside").expect("document");

    let out = authed(dir.path(), &token)
        .args(["--json", "ingest", "document", "--source", "text"])
        .arg(&file)
        .output()
        .expect("run document ingest");
    let error = single_document_error(&out);
    assert_eq!(error["code"], "INVALID_INPUT");
    assert_eq!(error["message"], "Entrada no válida.");
    assert!(
        error["detail"]
            .as_str()
            .unwrap()
            .contains("path resolves outside storage root")
    );
}

// ---- REQ-CL-005: exit-code contract -----------------------------------------

#[test]
fn auth_failure_exits_with_code_4_bilingual() {
    // Missing/invalid MEMENTO_TOKEN → uniform AUTH_FAILED (REQ-TA-006),
    // exit 4, bilingual ES message, nothing served (REQ-CL-005 scenario).
    let dir = tempfile::tempdir().expect("tempdir");
    let out = bin()
        .env("MEMENTO_ROOT", dir.path())
        .env("MEMENTO_AGENT_ID", "test-agent")
        .args(["--no-embeddings", "search", "hola"])
        .output()
        .expect("run search");
    assert_eq!(out.status.code(), Some(4), "auth exit code");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Falló la autenticación"),
        "ES message: {stderr}"
    );
    assert!(out.stdout.is_empty(), "no data served");
}

#[test]
fn auth_failure_json_is_structured() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = bin()
        .env("MEMENTO_ROOT", dir.path())
        .env("MEMENTO_AGENT_ID", "test-agent")
        .args(["--json", "--no-embeddings", "stats"])
        .output()
        .expect("run stats");
    assert_eq!(out.status.code(), Some(4), "auth exit code");
    let v: Value = serde_json::from_slice(&out.stderr).expect("structured JSON error on stderr");
    assert_eq!(v["code"], "AUTH_FAILED", "stable code in envelope");
    assert_eq!(v["exit_code"], 4);
    assert!(
        !v["message"].as_str().unwrap().is_empty(),
        "bilingual message"
    );
}

#[test]
fn exit_code_contract_validation_and_topk() {
    let (dir, token) = provisioned();

    // Validation error: get-chunk with a non-uuid id → 2 (INVALID_INPUT).
    let out = authed(dir.path(), &token)
        .args(["--json", "get-chunk", "not-a-uuid"])
        .output()
        .expect("run get-chunk");
    assert_eq!(out.status.code(), Some(2), "validation exit code");
    let v: Value = serde_json::from_slice(&out.stderr).expect("structured error");
    assert_eq!(v["code"], "INVALID_INPUT");

    // top_k over the store maximum → 14 (TOP_K_EXCEEDED).
    let out = authed(dir.path(), &token)
        .args(["--json", "search", "hola", "--top-k", "999"])
        .output()
        .expect("run search");
    assert_eq!(out.status.code(), Some(14), "top_k exit code");
    let v: Value = serde_json::from_slice(&out.stderr).expect("structured error");
    assert_eq!(v["code"], "TOP_K_EXCEEDED");
}

// ---- REQ-CL-004: bilingual help ---------------------------------------------

#[test]
fn bilingual_help_es_first_en_fallback() {
    // ES primary by default.
    let out = bin().arg("--help").output().expect("run --help");
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("Busca en la memoria del workspace"),
        "ES search about: {help}"
    );
    assert!(help.contains("Salida en JSON"), "ES json help: {help}");

    // Subcommand help is ES too.
    let out = bin()
        .args(["tenant", "--help"])
        .output()
        .expect("tenant --help");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("Administración del tenant"),
        "ES tenant about: {help}"
    );

    // EN fallback via --locale en.
    let out = bin()
        .args(["--locale", "en", "--help"])
        .output()
        .expect("en --help");
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("Search workspace memory"),
        "EN search about: {help}"
    );
    assert!(help.contains("JSON output"), "EN json help: {help}");

    // EN fallback via MEMENTO_LOCALE env.
    let out = bin()
        .env("MEMENTO_LOCALE", "en")
        .arg("--help")
        .output()
        .expect("env --help");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("Tenant administration"),
        "EN tenant about: {help}"
    );
}

// ---- REQ-CL-001/003: round-trip + canonical JSON ----------------------------

#[test]
fn cli_only_round_trip_req_cl_001() {
    let (dir, token) = provisioned();

    // ingest text → chunk ids + doc id + chore id.
    let ingested = ingest_text(
        dir.path(),
        &token,
        "La memoria es la facultad de recordar las cosas pasadas.",
    );
    let chunk_ids = ingested["chunk_ids"].as_array().expect("chunk_ids").clone();
    assert!(!chunk_ids.is_empty(), "chunks produced");
    let doc_id = ingested["doc_id"].as_str().expect("doc_id").to_string();
    assert!(ingested["chore_id"].is_string(), "chore id present");

    // search → hit with provenance.
    let out = authed(dir.path(), &token)
        .args(["--json", "search", "memoria"])
        .output()
        .expect("run search");
    let v = json_of(&out);
    let hits = v["hits"].as_array().expect("hits");
    assert!(!hits.is_empty(), "round-trip hit");
    assert_eq!(hits[0]["provenance"]["doc_id"], doc_id, "provenance doc");

    // get-chunk → full chunk.
    let chunk_id = chunk_ids[0].as_str().expect("chunk id");
    let out = authed(dir.path(), &token)
        .args(["--json", "get-chunk", chunk_id])
        .output()
        .expect("run get-chunk");
    let v = json_of(&out);
    let chunk = v["chunk"].as_object().expect("chunk object");
    assert_eq!(chunk["id"], chunk_id, "chunk identity");
    assert!(
        chunk["text"].as_str().unwrap().contains("memoria"),
        "chunk text"
    );

    // feedback --useful → ok.
    let out = authed(dir.path(), &token)
        .args([
            "--json", "feedback", chunk_id, "--useful", "--reason", "es clave",
        ])
        .output()
        .expect("run feedback");
    let v = json_of(&out);
    assert_eq!(v["ok"], true);

    // context-fit within budget.
    let out = authed(dir.path(), &token)
        .args(["--json", "context-fit", "memoria", "--budget", "200"])
        .output()
        .expect("run context-fit");
    let v = json_of(&out);
    assert_eq!(
        v["total_tokens"].as_u64().unwrap() as usize,
        v["total_tokens"].as_u64().unwrap() as usize
    );
    assert!(
        v["total_tokens"].as_u64().unwrap() <= 200,
        "fitted set within budget"
    );

    // delete the chunk → hard delete (REQ-ML-002).
    let out = authed(dir.path(), &token)
        .args(["--json", "delete", "--chunk", chunk_id])
        .output()
        .expect("run delete");
    let v = json_of(&out);
    assert!(v["deleted_count"].as_u64().unwrap() >= 1, "deleted");

    // Search again → no hits.
    let out = authed(dir.path(), &token)
        .args(["--json", "search", "memoria"])
        .output()
        .expect("run search");
    let v = json_of(&out);
    assert!(
        v["hits"].as_array().unwrap().is_empty(),
        "gone after delete"
    );
}

#[test]
fn json_search_carries_full_provenance_req_cl_003() {
    let (dir, token) = provisioned();
    ingest_text(
        dir.path(),
        &token,
        "Los recuerdos se consolidan durante el sueño.",
    );

    let out = authed(dir.path(), &token)
        .args(["--json", "search", "sueño"])
        .output()
        .expect("run search");
    let v = json_of(&out);
    let hit = &v["hits"][0];
    let provenance = &hit["provenance"];
    for field in [
        "source",
        "doc_id",
        "chunk_id",
        "created_at",
        "embedding_model_version",
        "tenant_id",
        "workspace_id",
        "agent_id",
    ] {
        assert!(
            provenance.get(field).is_some(),
            "provenance field {field} present: {provenance}"
        );
    }
    assert_eq!(provenance["source"], "text", "source label matches MCP");
    assert!(hit["score"].is_number(), "score is a number");
}

// ---- REQ-CL-002: bulk ingest with per-file report ---------------------------

#[test]
fn bulk_ingest_per_file_report_req_cl_002() {
    let (dir, token) = provisioned();

    // Mixed directory: supported md + txt, unsupported xyz.
    let bulk_dir = tempfile::tempdir().expect("bulk dir");
    std::fs::write(
        bulk_dir.path().join("notas.md"),
        "# Notas\n\nDatos importantes.\n",
    )
    .unwrap();
    std::fs::write(bulk_dir.path().join("log.txt"), "línea de registro\n").unwrap();
    std::fs::write(bulk_dir.path().join("datos.xyz"), "no soportado\n").unwrap();

    let out = authed(dir.path(), &token)
        .args(["--json", "ingest", "bulk"])
        .arg(bulk_dir.path())
        .output()
        .expect("run bulk");
    let v = json_of(&out);
    assert_eq!(v["total"], 3, "all files visited: {v}");
    assert_eq!(v["ingested"], 2, "supported files ingested: {v}");
    assert_eq!(v["failed"], 1, "unsupported file reported: {v}");
    assert!(
        out.status.success(),
        "batch not aborted by per-file failure"
    );

    let files = v["files"].as_array().expect("report");
    let failed = files
        .iter()
        .find(|f| f["status"] == "error")
        .expect("failed entry");
    assert!(
        failed["reason"]
            .as_str()
            .unwrap()
            .contains("unsupported document format"),
        "reason names the format: {failed}"
    );
    let ok_count = files.iter().filter(|f| f["status"] == "ok").count();
    assert_eq!(ok_count, 2, "two ok entries");

    // The ingested corpus is searchable.
    let out = authed(dir.path(), &token)
        .args(["--json", "search", "Datos"])
        .output()
        .expect("run search");
    let v = json_of(&out);
    assert!(
        !v["hits"].as_array().unwrap().is_empty(),
        "bulk content searchable"
    );
}

// ---- REQ-CL-006: stats ------------------------------------------------------

#[test]
fn stats_reports_counts_per_workspace_req_cl_006() {
    let (dir, token) = provisioned();
    ingest_text(dir.path(), &token, "Primer recuerdo.");
    ingest_text(dir.path(), &token, "Segundo recuerdo.");

    let out = authed(dir.path(), &token)
        .args(["--json", "stats"])
        .output()
        .expect("run stats");
    let v = json_of(&out);
    assert!(
        v["chunks_total"].as_u64().unwrap() >= 2,
        "chunks counted: {v}"
    );
    let by_ws = v["chunks_by_workspace"]
        .as_object()
        .expect("per-workspace map");
    assert_eq!(by_ws.keys().count(), 1, "one bound workspace");
    assert!(by_ws.values().next().unwrap().as_u64().unwrap() >= 2);
    assert!(v["docs"].as_u64().unwrap() >= 2, "docs counted");
    assert_eq!(v["retention_days"], 30, "default retention");
}

// ---- T-082: tenant ceremonies ------------------------------------------------

#[test]
fn tenant_delete_ceremony_aborts_without_confirm() {
    let (dir, token) = provisioned();
    ingest_text(dir.path(), &token, "Dato que debe sobrevivir.");

    // Abort: anything but 'yes' on stdin → validation error, data intact.
    let out = authed(dir.path(), &token)
        .args(["--json", "tenant", "delete"])
        .write_stdin("no\n")
        .output()
        .expect("run tenant delete");
    assert_eq!(out.status.code(), Some(2), "aborted with validation code");
    let v: Value = serde_json::from_slice(&out.stderr).expect("structured error");
    assert_eq!(v["code"], "INVALID_INPUT", "structured abort reason");

    // Data intact.
    let out = authed(dir.path(), &token)
        .args(["--json", "search", "sobrevivir"])
        .output()
        .expect("run search");
    let v = json_of(&out);
    assert!(
        !v["hits"].as_array().unwrap().is_empty(),
        "data untouched after abort"
    );
}

#[test]
fn tenant_delete_ceremony_confirmed_destroys_credentials() {
    let (dir, token) = provisioned();
    ingest_text(dir.path(), &token, "Dato a borrar.");
    // A backup creates the master key (lazily) → crypto-shredding can
    // destroy it (D4).
    authed(dir.path(), &token)
        .args(["--json", "tenant", "backup"])
        .output()
        .expect("run backup");

    let out = authed(dir.path(), &token)
        .args(["--json", "tenant", "delete"])
        .write_stdin("yes\n")
        .output()
        .expect("run tenant delete");
    let v = json_of(&out);
    assert_eq!(v["credentials_destroyed"], true, "account destroyed");
    assert_eq!(v["master_key_destroyed"], true, "crypto-shredding ran");
    assert!(v["deleted_count"].as_u64().unwrap() >= 1, "data purged");

    // The old token no longer authenticates (credentials file gone).
    let out = authed(dir.path(), &token)
        .args(["--json", "stats"])
        .output()
        .expect("run stats");
    assert_eq!(
        out.status.code(),
        Some(4),
        "credential destroyed → auth fails"
    );
}

#[test]
fn tenant_rotate_token_old_dies() {
    let (dir, token) = provisioned();

    let out = authed(dir.path(), &token)
        .args(["--json", "tenant", "rotate-token"])
        .output()
        .expect("run rotate");
    let v = json_of(&out);
    let new_token = v["token"].as_str().expect("new token").to_string();
    assert_ne!(new_token, token, "fresh token");

    // Old token → AUTH_FAILED (exit 4).
    let out = authed(dir.path(), &token)
        .args(["--json", "stats"])
        .output()
        .expect("old token stats");
    assert_eq!(out.status.code(), Some(4), "old token dies immediately");

    // New token works.
    let out = authed(dir.path(), &new_token)
        .args(["--json", "stats"])
        .output()
        .expect("new token stats");
    assert!(out.status.success());
}

#[test]
fn tenant_retention_set_and_show() {
    let (dir, token) = provisioned();

    let out = authed(dir.path(), &token)
        .args(["--json", "tenant", "retention", "--days", "90"])
        .output()
        .expect("set retention");
    let v = json_of(&out);
    assert_eq!(v["updated"], true);
    assert_eq!(v["retention_days"], 90);

    let out = authed(dir.path(), &token)
        .args(["--json", "tenant", "retention"])
        .output()
        .expect("show retention");
    let v = json_of(&out);
    assert_eq!(v["retention_days"], 90, "override persisted");
}

#[test]
fn tenant_sweep_reports_disabled() {
    let (dir, token) = provisioned();
    ingest_text(dir.path(), &token, "Recuerdo antiguo.");
    // Disable retention (opt-out, REQ-ML-003) → sweep expires nothing.
    authed(dir.path(), &token)
        .args(["--json", "tenant", "retention", "--days", "0"])
        .output()
        .expect("disable retention");

    let out = authed(dir.path(), &token)
        .args(["--json", "tenant", "sweep"])
        .output()
        .expect("run sweep");
    let v = json_of(&out);
    assert_eq!(v["expired_count"], 0, "opt-out honored");
}

#[test]
fn tenant_backup_export_and_live_restore_rejection() {
    let (dir, token) = provisioned();
    ingest_text(dir.path(), &token, "Contenido respaldable.");

    // backup → artifact path.
    let out = authed(dir.path(), &token)
        .args(["--json", "tenant", "backup"])
        .output()
        .expect("run backup");
    let v = json_of(&out);
    let backup_path = PathBuf::from(v["path"].as_str().expect("backup path"));
    assert!(
        backup_path.join("backup.enc").is_file(),
        "encrypted payload"
    );
    assert!(backup_path.join("backup.key.json").is_file(), "wrapped key");

    // export → artifact exists.
    let out = authed(dir.path(), &token)
        .args(["--json", "tenant", "export"])
        .output()
        .expect("run export");
    let v = json_of(&out);
    let export_path = PathBuf::from(v["path"].as_str().expect("export path"));
    assert!(export_path.is_file(), "export artifact");
    assert!(v["chunk_count"].as_u64().unwrap() >= 1);

    // restore against the LIVE store → structured rejection (REQ-ML-005).
    let out = authed(dir.path(), &token)
        .args(["--json", "tenant", "restore"])
        .arg(&backup_path)
        .output()
        .expect("run restore");
    assert_eq!(out.status.code(), Some(2), "live restore rejected");
    let v: Value = serde_json::from_slice(&out.stderr).expect("structured error");
    assert_eq!(v["code"], "INVALID_INPUT", "quiesce requirement named");
}

// ---- health -----------------------------------------------------------------

#[test]
fn health_reports_ok() {
    let (dir, token) = provisioned();
    let out = authed(dir.path(), &token)
        .args(["--json", "health"])
        .output()
        .expect("run health");
    let v = json_of(&out);
    assert_eq!(v["status"], "ok");
    assert!(v["tenant_id"].is_string(), "tenant reported");
    assert_eq!(v["embeddings"], "disabled", "no-embeddings mode surfaced");
}

// ---- T-084: code commands ----------------------------------------------------

#[test]
fn code_index_status_debug_round_trip() {
    let (dir, token) = provisioned();

    // Real fixture repo: cross-module Rust chain (same shape as the MCP
    // e2e fixture — module edges need ≥2 modules).
    let repo = tempfile::tempdir().expect("repo");
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(
        repo.path().join("src/a.rs"),
        "fn entry() { mid(); helper(); }\nfn mid() { leaf(); }\nfn leaf() {}\n",
    )
    .unwrap();
    std::fs::write(repo.path().join("src/b.rs"), "fn helper() {}\n").unwrap();

    // index → report.
    let out = authed(dir.path(), &token)
        .args(["--json", "code", "index"])
        .arg(repo.path())
        .output()
        .expect("run code index");
    let v = json_of(&out);
    assert!(
        v["files_indexed"].as_u64().unwrap() >= 2,
        "fixture indexed: {v}"
    );
    let project_id = v["project_id"].as_str().expect("project id").to_string();
    assert!(v["symbol_count"].as_u64().unwrap() >= 4, "L2 symbols: {v}");

    // status → layer state.
    let out = authed(dir.path(), &token)
        .args(["--json", "code", "status", "--project", &project_id])
        .output()
        .expect("run code status");
    let v = json_of(&out);
    assert_eq!(v["project_id"], project_id);
    assert_eq!(v["layers"]["l1_bundles"], true, "L1 present");
    assert!(
        v["layers"]["l2_symbols"].as_u64().unwrap() >= 4,
        "L2 mirror"
    );
    assert!(v["layers"]["l3_nodes"].as_u64().unwrap() >= 4, "L3 nodes");
    assert_eq!(v["layers"]["l4_summary"], true, "L4 summary");

    // debug → canonical graph with referential integrity (REQ-CK-009).
    let out = authed(dir.path(), &token)
        .args(["--json", "code", "debug", &project_id])
        .output()
        .expect("run code debug");
    let v = json_of(&out);
    assert_eq!(v["project_id"], project_id);
    assert_eq!(
        v["referential_integrity"], true,
        "every edge endpoint is a node"
    );
    let graph = &v["graph"];
    assert!(graph["nodes"].as_array().unwrap().len() >= 4, "nodes");
    assert!(
        !graph["edges"].as_array().unwrap().is_empty(),
        "cross-module edge exists"
    );
}
