//! H0 + H1 CLI acceptance: no silent hybrid degradation, workspace
//! create/list, and ingest `--workspace` isolation (REQ-MR-003, REQ-WS-*).

use std::path::Path;

use assert_cmd::Command;
use serde_json::Value;

fn bin() -> Command {
    Command::cargo_bin("memento").expect("binary")
}

fn json_of(out: &std::process::Output) -> Value {
    assert!(
        out.status.success(),
        "expected success, got {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("stdout is JSON")
}

fn json_err(out: &std::process::Output) -> Value {
    assert!(
        !out.status.success(),
        "expected failure: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    serde_json::from_slice(&out.stderr).expect("stderr is JSON error")
}

fn provisioned() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = bin()
        .args(["--json", "tenant", "create", "--name", "h0h1"])
        .env("MEMENTO_ROOT", dir.path())
        .arg("--root")
        .arg(dir.path())
        .output()
        .expect("tenant create");
    let v = json_of(&out);
    let token = v["token"].as_str().expect("token").to_string();
    (dir, token)
}

fn authed(root: &Path, token: &str) -> Command {
    let mut c = bin();
    c.env("MEMENTO_ROOT", root)
        .env("MEMENTO_TOKEN", token)
        .env("MEMENTO_AGENT_ID", "agent-a")
        .arg("--root")
        .arg(root)
        .arg("--no-embeddings");
    c
}

#[test]
fn hybrid_without_embeddings_is_invalid_input_req_mr_003() {
    // H0: never silently fall back to FTS when --rrf is set without embeddings.
    let (dir, token) = provisioned();
    let _ = authed(dir.path(), &token)
        .args(["--json", "ingest", "text", "memoria hibrida"])
        .output()
        .expect("ingest");
    let out = authed(dir.path(), &token)
        .args(["--json", "search", "memoria", "--rrf"])
        .output()
        .expect("search rrf");
    let err = json_err(&out);
    assert_eq!(err["code"], "INVALID_INPUT");
}

#[test]
fn workspace_create_list_and_ingest_isolation_req_ws() {
    let (dir, token) = provisioned();

    let out = authed(dir.path(), &token)
        .args(["--json", "workspace", "create", "--name", "pilot"])
        .output()
        .expect("workspace create");
    let created = json_of(&out);
    let ws_b = created["workspace_id"].as_str().expect("workspace_id");

    let out = authed(dir.path(), &token)
        .args(["--json", "workspace", "list"])
        .output()
        .expect("workspace list");
    let listed = json_of(&out);
    let workspaces = listed["workspaces"].as_array().expect("workspaces");
    assert!(workspaces.iter().any(|w| w["is_default"] == true));
    assert!(workspaces.iter().any(|w| w["workspace_id"] == ws_b));

    let _ = authed(dir.path(), &token)
        .args([
            "--json",
            "ingest",
            "text",
            "dato exclusivo del workspace pilot xyz",
            "--workspace",
            ws_b,
        ])
        .output()
        .expect("ingest into ws");

    let out = authed(dir.path(), &token)
        .args(["--json", "search", "pilot xyz", "--workspace", ws_b])
        .output()
        .expect("search ws");
    let hits = json_of(&out);
    assert_eq!(hits["hits"].as_array().unwrap().len(), 1);
    assert_eq!(hits["hits"][0]["provenance"]["workspace_id"], ws_b);

    let out = authed(dir.path(), &token)
        .args(["--json", "search", "pilot xyz"])
        .output()
        .expect("search default");
    let hits = json_of(&out);
    assert!(
        hits["hits"].as_array().unwrap().is_empty(),
        "default workspace isolation"
    );
}

#[test]
fn json_search_provenance_matches_bound_agent() {
    // H0 provenance: fields present AND agent_id equals the bound agent.
    let (dir, token) = provisioned();
    let _ = authed(dir.path(), &token)
        .args(["--json", "ingest", "text", "Los recuerdos se consolidan."])
        .output()
        .expect("ingest");
    let out = authed(dir.path(), &token)
        .args(["--json", "search", "recuerdos"])
        .output()
        .expect("search");
    let v = json_of(&out);
    let p = &v["hits"][0]["provenance"];
    assert_eq!(p["agent_id"], "agent-a");
    assert_eq!(p["source"], "text");
    for field in [
        "doc_id",
        "chunk_id",
        "created_at",
        "embedding_model_version",
        "tenant_id",
        "workspace_id",
    ] {
        assert!(p.get(field).is_some(), "missing {field}");
    }
}
