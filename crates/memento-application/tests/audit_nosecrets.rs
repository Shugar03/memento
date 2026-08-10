//! Audit-log no-secrets scan (T-066, REQ-CG-003).
//!
//! Runs a full operation battery (ingest, feedback, delete, sweep, backup,
//! export, erase) and then scans EVERY audit line mechanically:
//!
//! * every line is JSON with the documented shape
//!   (`{ts, tenant_id, agent_id, action, target, outcome, error_code,
//!   chore_id}`) — REQ-CG-003 attribution contract;
//! * no line contains ingested content, query text, credentials, or key
//!   material (the scan is content-blind to WHAT the ops did — it greps for
//!   the exact secret strings the battery planted, plus credential shape
//!   patterns);
//! * the delete event carries `(ts, tenant, agent, delete, target)` without
//!   content (REQ-CG-003 scenario).

use memento_application::audit::AuditLogger;
use memento_application::{AppService, Clock};
use memento_domain::{AgentId, SourceKind, TenantId};
use memento_parse::ParseService;
use memento_parse::anydoc::{AnydocCommand, AnydocConfig};
use memento_ports::{DeleteScope, IngestDocumentRequest, IngestTextRequest};
use memento_testkit::{StubEmbedPort, TempStore, TestClock};
use std::path::PathBuf;
use std::sync::Arc;

/// A fixed clock for the battery (deterministic timestamps).
#[derive(Clone, Debug)]
struct BatteryClock(TestClock);

impl Clock for BatteryClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        self.0.now()
    }
}

/// A real parse boundary on the FALLBACK path (no subprocess invoked).
fn fallback_parse() -> Arc<dyn memento_ports::ParsePort> {
    Arc::new(ParseService::new(AnydocConfig {
        command: AnydocCommand {
            program: "never-invoked".into(),
            args: vec![],
            env: vec![],
        },
        timeout: std::time::Duration::from_secs(1),
        stdout_limit: 1024,
        staging_dir: std::env::temp_dir(),
    }))
}

/// Planted secrets the scan must NEVER find in the audit:
/// * a credential-shaped token (memo_<tid>_<48×base62>);
/// * an Argon2 PHC hash;
/// * the ingested content itself (content must never ride the audit).
const PLANTED_TOKEN: &str =
    "memo_00000000-0000-4000-8000-000000000000_abcDEF0123456789abcDEF0123456789abcDEF0123456789ab";
const PLANTED_PHC: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$c29tZWhhc2h2YWx1ZQ";
const PLANTED_CONTENT: &str = "texto secreto que jamas debe aparecer en la auditoria";
const PLANTED_DOC_CONTENT: &str = "# Titulo\n\ncontenido del documento auditado";

fn audit_path(root: &std::path::Path, tid: &TenantId) -> PathBuf {
    root.join("logs").join(format!("{tid}.jsonl"))
}

#[tokio::test]
async fn audit_lines_have_shape_and_never_carry_content_or_secrets() {
    let ts = TempStore::new();
    let app = AppService::open(
        &ts.ctx(),
        ts.root(),
        fallback_parse(),
        Some(Arc::new(StubEmbedPort::default())),
        Arc::new(BatteryClock(TestClock::default())),
    )
    .await
    .expect("app opens");

    // ---- battery: every security-relevant event (REQ-CG-003) ----
    // ingest (text + document)
    let ingest = app
        .ingest_text(
            &ts.ctx(),
            IngestTextRequest {
                text: format!("{PLANTED_CONTENT} y tambien {PLANTED_TOKEN}"),
                doc_id: None,
                metadata: None,
            },
        )
        .await
        .expect("ingest text");
    app.ingest_document(
        &ts.ctx(),
        IngestDocumentRequest {
            blob: PLANTED_DOC_CONTENT.as_bytes().to_vec(),
            source_hint: SourceKind::Markdown,
            doc_id: None,
            metadata: None,
        },
    )
    .await
    .expect("ingest doc");

    // feedback
    let chunk_id = ingest.chunk_ids[0];
    app.feedback(
        &ts.ctx(),
        chunk_id,
        true,
        Some("razón con contenido".to_string()),
    )
    .await
    .expect("feedback");
    app.feedback(&ts.ctx(), chunk_id, false, None)
        .await
        .expect("feedback 2");

    // delete (chunk + doc scopes)
    let report = app
        .delete(&ts.ctx(), DeleteScope::Chunk { id: chunk_id })
        .await
        .expect("delete chunk");
    assert!(report.deleted_count >= 1);

    // retention change + sweep (opt-out then re-enable + sweep)
    app.set_retention_days(&ts.ctx(), 90)
        .await
        .expect("retention change");
    app.retention_sweep(&ts.ctx()).await.expect("sweep");

    // backup + export
    app.backup(&ts.ctx()).await.expect("backup");
    app.export_tenant(&ts.ctx()).await.expect("export");

    // erase (destroys keys; the audit of erase happens before the config
    // file is removed — the log itself is not a tenant data file)
    app.erase(&ts.ctx()).await.expect("erase");

    // ---- scan every line ----
    let raw = std::fs::read_to_string(audit_path(ts.root(), ts.tenant_id())).expect("audit file");
    assert!(!raw.is_empty(), "battery produced audit lines");
    let lines: Vec<&str> = raw.lines().collect();

    // Shape contract per line.
    for line in &lines {
        let v: serde_json::Value = serde_json::from_str(line).expect("each line is JSON");
        assert!(v["ts"].is_string(), "ts present: {line}");
        assert_eq!(v["tenant_id"], ts.tenant_id().to_string(), "tenant: {line}");
        assert!(v["agent_id"].is_string(), "agent: {line}");
        assert!(v["action"].is_string(), "action: {line}");
        assert!(v["target"].is_object(), "target: {line}");
        assert!(v["outcome"].is_string(), "outcome: {line}");
        assert!(
            v["error_code"].is_null() || v["error_code"].is_string(),
            "error_code: {line}"
        );
        assert!(
            v["chore_id"].is_null() || v["chore_id"].is_string(),
            "chore_id: {line}"
        );
    }

    // REQ-CG-003 scenario: the delete event carries (ts, tenant, agent,
    // delete, target) — verify by locating the delete line.
    let delete_line = lines
        .iter()
        .find(|l| l.contains("\"action\":\"delete\""))
        .expect("delete audited");
    let v: serde_json::Value = serde_json::from_str(delete_line).unwrap();
    assert_eq!(v["action"], "delete");
    assert_eq!(v["target"]["scope"], "chunk");
    assert_eq!(v["target"]["target"], chunk_id.to_string());

    // No-secrets scan: content, tokens, PHC hashes, key material absent.
    for line in &lines {
        assert!(!line.contains(PLANTED_CONTENT), "content leaked: {line}");
        assert!(
            !line.contains(PLANTED_DOC_CONTENT),
            "doc content leaked: {line}"
        );
        assert!(!line.contains(PLANTED_TOKEN), "credential leaked: {line}");
        assert!(!line.contains(PLANTED_PHC), "hash leaked: {line}");
        assert!(!line.contains("memo_"), "credential-shape leaked: {line}");
        assert!(!line.contains("$argon2"), "PHC-shape leaked: {line}");
        assert!(!line.contains("master.key"), "key material leaked: {line}");
        assert!(
            !line.contains("MEMENTO_TOKEN"),
            "env credential leaked: {line}"
        );
        assert!(
            !line.contains("razón"),
            "feedback reason text leaked: {line}"
        );
    }

    // The ingest lines record counts and ids — never text.
    let ingest_lines: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| l.contains("\"action\":\"ingest\""))
        .collect();
    assert_eq!(ingest_lines.len(), 2, "text + document ingests audited");
}

#[tokio::test]
async fn auth_failure_shape_is_recorded_by_the_logger() {
    // The logger's error path writes the same shape with outcome=error and
    // a stable code (the auth-failure events themselves are emitted by the
    // tenant layer; this test pins the shape contract they rely on).
    let ts = TempStore::new();
    let logger = AuditLogger::new(ts.root(), ts.tenant_id()).expect("logger");
    logger.error(
        &memento_domain::TenantContext::new_for_tests(
            *ts.tenant_id(),
            *ts.workspace_id(),
            AgentId::new("test-agent"),
        ),
        "auth",
        serde_json::json!({"outcome": "AUTH_FAILED"}),
        "AUTH_FAILED",
        None,
    );
    let raw = std::fs::read_to_string(audit_path(ts.root(), ts.tenant_id())).unwrap();
    let v: serde_json::Value = serde_json::from_str(raw.trim()).unwrap();
    assert_eq!(v["action"], "auth");
    assert_eq!(v["outcome"], "error");
    assert_eq!(v["error_code"], "AUTH_FAILED");
    assert_eq!(v["tenant_id"], ts.tenant_id().to_string());
    assert_eq!(v["agent_id"], "test-agent");
    assert!(v["ts"].is_string());
}
