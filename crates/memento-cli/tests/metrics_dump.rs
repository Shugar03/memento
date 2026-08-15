//! `memento observability metrics` dump command tests (REQ-OBS-007, design
//! D7): Prometheus text to stdout or MEMENTO_METRICS_FILE, exit 0 with an
//! empty registry when metrics are off, and NO HTTP listener ever (the
//! exporter is compiled with default-features=false — no hyper in the tree).
//!
//! The subprocess cases run the real binary: a fresh process has an empty
//! registry by construction, so they pin the command contract (exists,
//! exit 0, empty output, destination override honored). The in-process case
//! records REAL traffic and verifies the file destination renders
//! Prometheus text with the recorded series.

use assert_cmd::Command;

/// Serializes the in-process test's env mutation (MEMENTO_METRICS /
/// MEMENTO_METRICS_FILE are process-global).
static METRICS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn bin() -> Command {
    Command::cargo_bin("memento").expect("binary")
}

/// `memento observability metrics` with no root/credentials (the dump is
/// process-local and root-independent — design D7).
fn metrics_dump_cmd() -> Command {
    let mut cmd = bin();
    cmd.args(["observability", "metrics"]);
    cmd
}

#[test]
fn metrics_dump_off_exits_zero_with_empty_stdout() {
    // REQ-OBS-007 scenario 2: metrics disabled (default) → exit 0 with an
    // empty registry. Explicitly cleared so a parallel test's parent-env
    // mutation can never leak into this child process.
    let out = metrics_dump_cmd()
        .env_remove("MEMENTO_METRICS")
        .env_remove("MEMENTO_METRICS_FILE")
        .output()
        .expect("run dump");
    assert_eq!(
        out.status.code(),
        Some(0),
        "exit 0 when off: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "empty registry renders nothing: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(out.stderr.is_empty(), "no error output: {:?}", out.stderr);
}

#[test]
fn metrics_dump_enabled_exits_zero_with_empty_registry() {
    // REQ-OBS-007 (empty side of scenario 1): a fresh process has no
    // recorded traffic, so even enabled the registry renders empty — still
    // exit 0 and no error output.
    let out = metrics_dump_cmd()
        .env("MEMENTO_METRICS", "1")
        .env_remove("MEMENTO_METRICS_FILE")
        .output()
        .expect("run dump");
    assert_eq!(out.status.code(), Some(0), "exit 0 when enabled");
    assert!(
        out.stdout.is_empty(),
        "empty registry in a fresh process: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(out.stderr.is_empty(), "no error output");
}

#[test]
fn metrics_dump_file_override_creates_destination() {
    // REQ-OBS-007: MEMENTO_METRICS_FILE overrides the destination — the dump
    // lands in the file, not stdout.
    let dir = tempfile::tempdir().expect("tempdir");
    let dest = dir.path().join("metrics.txt");
    let out = metrics_dump_cmd()
        .env("MEMENTO_METRICS", "1")
        .env("MEMENTO_METRICS_FILE", &dest)
        .output()
        .expect("run dump");
    assert_eq!(out.status.code(), Some(0), "exit 0 with file override");
    assert!(
        out.stdout.is_empty(),
        "stdout stays empty when the file override is set: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(dest.is_file(), "destination file created");
    let content = std::fs::read_to_string(&dest).expect("read destination");
    assert!(
        !content.contains("memento_"),
        "fresh-process registry renders no metric lines: {content}"
    );
}

#[test]
fn metrics_dump_file_destination_renders_recorded_traffic() {
    // REQ-OBS-007 scenario 1 (with traffic): in THIS process (the test
    // binary shares the registry with the library call), record real metric
    // traffic and verify the dump command renders it as Prometheus text into
    // the file destination — the same code path as stdout.
    let _guard = METRICS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let dest = dir.path().join("metrics.txt");
    // SAFETY: test-only env mutation, serialized by METRICS_ENV_LOCK.
    unsafe { std::env::set_var("MEMENTO_METRICS", "1") };
    // Install the recorder, then generate traffic the dump must render.
    let _ = memento_observability::metrics::ensure_recorder();
    metrics::counter!("memento_cli_dump_probe_total").increment(3);

    // SAFETY: test-only env mutation, serialized by METRICS_ENV_LOCK.
    unsafe { std::env::set_var("MEMENTO_METRICS_FILE", &dest) };
    // Parse through the REAL args tree — proves the subcommand wiring too —
    // and dispatch exactly like `memento_cli::run` does (sub matches).
    let root = memento_cli::args::build(&memento_i18n::I18n::load(memento_i18n::Locale::Es))
        .try_get_matches_from(["memento", "observability", "metrics"])
        .expect("observability metrics parses");
    let (_name, sub) = root.subcommand().expect("observability subcommand");
    memento_cli::commands::observability::run_sync(sub).expect("dump runs");

    let content = std::fs::read_to_string(&dest).expect("read destination");
    assert!(
        content.contains("memento_cli_dump_probe_total 3"),
        "recorded traffic renders as Prometheus text: {content}"
    );

    // Triangulation: more traffic → the next dump reflects the new value.
    metrics::counter!("memento_cli_dump_probe_total").increment(2);
    memento_cli::commands::observability::run_sync(sub).expect("dump runs again");
    let content = std::fs::read_to_string(&dest).expect("read destination");
    assert!(
        content.contains("memento_cli_dump_probe_total 5"),
        "dump re-renders the accumulated registry: {content}"
    );

    // SAFETY: test-only env mutation, serialized by METRICS_ENV_LOCK.
    unsafe { std::env::remove_var("MEMENTO_METRICS") };
    unsafe { std::env::remove_var("MEMENTO_METRICS_FILE") };
}
