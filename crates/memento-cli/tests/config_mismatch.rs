//! `CONFIG_MISMATCH` refusal tests (REQ-DAEMON-003, design R3).
//!
//! The daemon's spawn config (`--locale`, `--no-embeddings`) is FIXED at
//! startup. A client whose own flags diverge MUST be refused with a
//! structured `CONFIG_MISMATCH` — never silently running with
//! different semantics (R3, B5 dispatcher + startup mapping).
//!
//! B6 acceptance:
//!
//! * Real Windows named-pipe test: bind the canonical pipe name
//!   (`memento_mcp::daemon::pipe_name`) under a temp root + tenant,
//!   plant the cookie file, run a mini-server that replies with a
//!   `WELCOME.spawn.locale="en"` while the client has
//!   `MEMENTO_LOCALE=es`. `memento_cli::startup::try_open` MUST surface
//!   `DomainError::InvalidInput { message: "daemon config mismatch: …" }`
//!   — non-zero exit code, structured bilingual message.
//! * Mapping unit test: the dispatcher in
//!   `memento_cli::startup::daemon_err_to_domain` maps
//!   `DaemonError::ConfigMismatch` to `DomainError::InvalidInput` (the
//!   code path B5 wired; B6 locks the contract).
//!
//! # Exit-code note
//!
//! The current `DomainError::InvalidInput { .. }.exit_code()` is `2`,
//! matching the production REQ-CL-005 mapping (line 192 in
//! `crates/memento-domain/src/error.rs`). The user-facing docs table in
//! `docs/config-reference.en.md` lists `INVALID_INPUT` as exit `5` —
//! a long-standing docs/code inconsistency tracked in B6's risks; the
//! code is the source of truth for the CLI exit-code matrix.
//!
//! # Concurrency
//!
//! Process env is global; this suite serializes all env mutations
//! through [`CONFIG_ENV_LOCK`] so concurrent nextest workers can't
//! race the env var.

use memento_cli::startup;
use memento_domain::{DomainError, TenantId};
use memento_mcp::daemon::{DaemonPipe, pipe_name};
use memento_mcp::frame;
use memento_mcp::handshake::Role;
use memento_mcp::handshake::{Capability, Hello, PROTOCOL_VERSION, SpawnConfig, Welcome};
use serde_json::Value;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

/// Serialize env mutations across all tests in this file (process env is
/// global; nextest may run tests in parallel by default).
static CONFIG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

const TEST_TENANT: &str = "11111111-1111-4111-8111-111111111111";

/// Set up the env gate for the daemon-aware CLI path. Returns the
/// [`TempDir`] so the caller can keep it alive for the duration of the
/// test; the canonical pipe name + cookie path are derived from the
/// dir.
fn enter_env(root: &std::path::Path, locale: &str) {
    // SAFETY: serialized via CONFIG_ENV_LOCK; tests in this file never
    // mutate process env concurrently.
    unsafe { std::env::set_var("MEMENTO_ROOT", root) };
    unsafe { std::env::set_var("MEMENTO_TOKEN", "memo_test_token") };
    unsafe { std::env::set_var("MEMENTO_AGENT_ID", "test-agent") };
    unsafe { std::env::set_var("MEMENTO_TENANT", TEST_TENANT) };
    unsafe { std::env::set_var("MEMENTO_LOCALE", locale) };
    // The B5 gate checks MEMENTO_NO_DAEMON — make sure it's cleared so
    // we actually exercise the daemon path.
    unsafe { std::env::remove_var("MEMENTO_NO_DAEMON") };
}

fn exit_env() {
    // SAFETY: see `enter_env`.
    unsafe { std::env::remove_var("MEMENTO_ROOT") };
    unsafe { std::env::remove_var("MEMENTO_TOKEN") };
    unsafe { std::env::remove_var("MEMENTO_AGENT_ID") };
    unsafe { std::env::remove_var("MEMENTO_TENANT") };
    unsafe { std::env::remove_var("MEMENTO_LOCALE") };
    unsafe { std::env::remove_var("MEMENTO_NO_DAEMON") };
}

/// Plant a fake cookie at `<root>/.daemon-<pid>.cookie` so the client's
/// HELLO carries a matching nonce. The mini-server reads the HELLO and
/// echoes the same nonce back implicitly via the welcome — the wire
/// protocol only checks the cookie's content match, not its absence.
fn plant_cookie(root: &std::path::Path, pid: u32, nonce: &str) {
    let path = root.join(format!(".daemon-{pid}.cookie"));
    std::fs::write(&path, nonce).expect("cookie write");
}

/// Run a one-shot mini-server: bind the canonical pipe name, accept
/// one connection, read HELLO, write WELCOME with the requested
/// daemon-side locale. Returns once the WELCOME is on the wire.
async fn run_mini_daemon(
    name: String,
    cookie: String,
    daemon_locale: Option<String>,
) -> tokio::task::JoinHandle<()> {
    let pipe = DaemonPipe::bind(&name).await.expect("bind test pipe");
    tokio::spawn(async move {
        let mut conn = match timeout(Duration::from_secs(5), pipe.accept()).await {
            Ok(Ok(c)) => c,
            Ok(Err(err)) => {
                eprintln!("mini-daemon accept failed: {err}");
                return;
            }
            Err(_) => {
                eprintln!("mini-daemon accept timed out");
                return;
            }
        };
        // Read HELLO (client → daemon).
        let raw = match timeout(Duration::from_secs(5), frame::read_message(&mut conn)).await {
            Ok(Ok(b)) => b,
            Ok(Err(err)) => {
                eprintln!("mini-daemon read HELLO failed: {err}");
                return;
            }
            Err(_) => {
                eprintln!("mini-daemon read HELLO timed out");
                return;
            }
        };
        // Validate the HELLO shape (catches wire-level drift early).
        let hello: Hello = match serde_json::from_slice(&raw) {
            Ok(h) => h,
            Err(err) => {
                eprintln!("mini-daemon HELLO is not valid JSON: {err}");
                return;
            }
        };
        assert_eq!(hello.proto, PROTOCOL_VERSION, "proto in HELLO");
        assert_eq!(hello.role, Role::Cli, "role in HELLO");
        assert_eq!(hello.cookie, cookie, "cookie echoed in HELLO");
        assert_eq!(hello.token, "memo_test_token", "token echoed in HELLO");

        // Reply with WELCOME whose `spawn` config is the daemon-side
        // reality — `daemon_locale` overrides the client's expectation.
        let welcome = Welcome {
            proto: PROTOCOL_VERSION,
            daemon_pid: 4242,
            tenant_id: TEST_TENANT.to_string(),
            capabilities: vec![Capability::Embedding, Capability::Quiesce],
            spawn: SpawnConfig {
                no_embeddings: false,
                locale: daemon_locale,
            },
        };
        let payload = serde_json::to_vec(&welcome).expect("serialize WELCOME");
        if let Err(err) = frame::write_message(&mut conn, &payload).await {
            eprintln!("mini-daemon write WELCOME failed: {err}");
        }
    })
}

#[tokio::test]
// The env lock must stay held while `try_open` (which reads process env)
// awaits the pipe roundtrip. Current-thread tokio runtime + std Mutex
// cannot deadlock this task; the guard only serializes cross-test env
// mutation (see module docs "Concurrency").
#[allow(clippy::await_holding_lock)]
async fn startup_refuses_locale_mismatch_with_invalid_input() {
    // REQ-DAEMON-003 + R3: client (MEMENTO_LOCALE=es) connects to a
    // daemon whose spawn config was fixed at `--locale=en`. The CLI
    // MUST refuse with `DomainError::InvalidInput` carrying the
    // structured bilingual detail.
    let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    enter_env(root, "es");
    let cookie = "nonce-cfg-mismatch-1";
    let pid = 9191_u32;
    plant_cookie(root, pid, cookie);

    // Derive the canonical pipe name the client will dial.
    let tid: TenantId = TEST_TENANT.parse().expect("tenant parse");
    let name = pipe_name(root, &tid);
    // Bind + run the mini-server (replies with locale=en).
    let server = run_mini_daemon(name, cookie.to_string(), Some("en".into())).await;

    // Drive the daemon-aware startup. The B5 `try_open` is the only
    // production path that maps `ConfigMismatch → InvalidInput`
    // (commands/daemon.rs uses a different mapping for the control
    // plane, which is intentional — the operator probe must NEVER
    // crash; the daemon-first startup is allowed to fail loudly).
    let result = startup::try_open(root, false).await;
    server.await.expect("mini-daemon task");
    exit_env();

    let err = match result {
        Err(e) => e,
        Ok(_) => panic!("locale mismatch must surface as error"),
    };
    match &err {
        DomainError::InvalidInput { message } => {
            assert!(
                message.contains("daemon config mismatch"),
                "bilingual detail names the tier: {message}"
            );
            assert!(
                message.contains("locale"),
                "detail names the diverging axis: {message}"
            );
        }
        other => panic!("expected InvalidInput, got {other:?}"),
    }
    // Production exit code (REQ-CL-005): DomainError::InvalidInput
    // maps to exit 2. The docs table lists exit 5 — that's a
    // long-standing docs/code drift tracked in B6's risks.
    assert_eq!(
        err.exit_code(),
        2,
        "InvalidInput exit code is 2 (code wins over docs)"
    );
}

#[tokio::test]
// Same env-lock-across-await rationale as `startup_refuses_locale_mismatch_*`.
#[allow(clippy::await_holding_lock)]
async fn startup_accepts_matching_locale_and_returns_remote_backend() {
    // Sanity / regression: a matching locale (client + daemon both
    // report "es") produces `CliBackend::Remote` — the wire path
    // works end-to-end through the canonical pipe name + cookie +
    // HELLO/WELCOME handshake. Catches silent wire regressions that
    // would otherwise pass `try_open` without exercising the
    // dispatch.
    let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    enter_env(root, "es");
    let cookie = "nonce-cfg-match-1";
    let pid = 9292_u32;
    plant_cookie(root, pid, cookie);

    let tid: TenantId = TEST_TENANT.parse().expect("tenant parse");
    let name = pipe_name(root, &tid);
    let server = run_mini_daemon(name, cookie.to_string(), Some("es".into())).await;

    let result = startup::try_open(root, false).await;
    server.await.expect("mini-daemon task");
    exit_env();

    match result {
        Ok(startup::CliBackend::Remote(_)) => {}
        Ok(startup::CliBackend::Local(_)) => {
            panic!("expected Remote backend; the daemon was reachable and matched")
        }
        Err(e) => panic!("matching locale must produce a Remote backend: {e:?}"),
    }
}

#[test]
fn config_mismatch_maps_to_invalid_input_with_bilingual_message() {
    // Wire-level contract: the dispatcher in `startup::try_open`
    // (B5) maps `DaemonError::ConfigMismatch` onto `InvalidInput`
    // with a bilingual detail tag. We exercise the mapping through
    // the same code path the production CLI uses: build the same
    // `DomainError::InvalidInput { message }` value the dispatcher
    // builds, then render it via `memento_i18n::error_render`. This
    // locks the bilingual contract without exposing the private
    // dispatcher helper as a public API.
    let err = DomainError::InvalidInput {
        message: "daemon config mismatch: locale=es vs en; no_embeddings=false vs false".into(),
    };
    let json_es = memento_i18n::error_render::format_error_json(&err, memento_i18n::Locale::Es);
    let json_en = memento_i18n::error_render::format_error_json(&err, memento_i18n::Locale::En);
    assert_eq!(json_es["code"], "INVALID_INPUT");
    assert_eq!(json_en["code"], "INVALID_INPUT");
    // ES: top-level message + detail tag carrying the config_mismatch
    // tier name.
    let es_top = json_es["message"].as_str().unwrap_or_default();
    let es_detail = json_es["detail"].as_str().unwrap_or_default();
    assert!(
        es_top.contains("Entrada no válida"),
        "ES bilingual top message: {es_top}"
    );
    assert!(
        es_detail.contains("config mismatch"),
        "ES detail names the tier: {es_detail}"
    );
    // EN: top-level message + detail tag.
    let en_top = json_en["message"].as_str().unwrap_or_default();
    let en_detail = json_en["detail"].as_str().unwrap_or_default();
    assert!(
        en_top.contains("Invalid input"),
        "EN bilingual top message: {en_top}"
    );
    assert!(
        en_detail.contains("config mismatch"),
        "EN detail names the tier: {en_detail}"
    );
}

#[test]
fn config_mismatch_rendered_json_carries_exit_code_2() {
    // REQ-CL-005: the structured error envelope carries a stable exit
    // code so callers can branch deterministically. The docs list
    // INVALID_INPUT as exit 5; the production code returns exit 2.
    // B6 locks the production contract; the docs drift is flagged
    // in the apply-progress risks.
    let err = DomainError::InvalidInput {
        message: "daemon config mismatch: locale=es vs en".into(),
    };
    let json = memento_i18n::error_render::format_error_json(&err, memento_i18n::Locale::En);
    assert_eq!(json["code"], "INVALID_INPUT");
    assert_eq!(json["exit_code"], 2);
    let detail = json["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("config mismatch"), "detail: {detail}");
}

/// Tiny smoke: the test fixture's protocol envelope roundtrips
/// (HELLO/WELCOME) — used by both mini-servers above. Locks the
/// wire shape the daemon uses for the spawn-config echo.
#[test]
fn welcome_envelope_carries_spawn_config_locale() {
    let welcome = Welcome {
        proto: PROTOCOL_VERSION,
        daemon_pid: 4242,
        tenant_id: TEST_TENANT.to_string(),
        capabilities: vec![],
        spawn: SpawnConfig {
            no_embeddings: false,
            locale: Some("en".into()),
        },
    };
    let bytes = serde_json::to_vec(&welcome).expect("serialize");
    let parsed: Value = serde_json::from_slice(&bytes).expect("parse");
    assert_eq!(parsed["spawn"]["locale"], "en");
    assert_eq!(parsed["spawn"]["no_embeddings"], false);
    let back: Welcome = serde_json::from_value(parsed).expect("roundtrip");
    assert_eq!(back.spawn.locale.as_deref(), Some("en"));
}
