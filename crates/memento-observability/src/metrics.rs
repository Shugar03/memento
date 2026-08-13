//! Lazy Prometheus metrics registry (REQ-OBS-006/007, design D2).
//!
//! The recorder is installed ONLY when `MEMENTO_METRICS=1` and only on the
//! first call that needs it (`OnceLock`). With the var unset the system does
//! zero metrics work: [`render`] returns an empty string and no registry
//! exists. `metrics` 0.24 macros are no-ops without a recorder, so hot paths
//! only pay atomic increments when enabled (REQ-OBS-004/006).
//!
//! The exporter is compiled with `default-features=false` (workspace pin):
//! the HTTP listener feature is absent — no port is ever bound (REQ-OBS-007,
//! threat-model "no telemetry" reconciliation). Dumps happen through
//! [`render`], which emits Prometheus text (gate verified at design time:
//! metrics-exporter-prometheus 0.18.3 on x86_64-pc-windows-gnu).

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::sync::OnceLock;

/// Pure enable gate (takes the raw env value so tests never mutate the
/// process environment to check it): exactly `1` enables the recorder.
pub fn metrics_enabled(value: Option<&str>) -> bool {
    value == Some("1")
}

fn metrics_enabled_from_env() -> bool {
    metrics_enabled(std::env::var("MEMENTO_METRICS").ok().as_deref())
}

/// Cached recorder handle. Only ever populated when `MEMENTO_METRICS=1`;
/// the enable check runs before touching the cache, so disabling stays
/// zero-cost regardless of prior state (order-independent tests).
static RECORDER: OnceLock<Option<PrometheusHandle>> = OnceLock::new();

fn install_recorder() -> Option<PrometheusHandle> {
    if !metrics_enabled_from_env() {
        return None;
    }
    PrometheusBuilder::new().install_recorder().ok()
}

/// Lazily install (once) and return the global Prometheus recorder, or
/// `None` while `MEMENTO_METRICS` is unset/not `1`.
pub fn ensure_recorder() -> Option<&'static PrometheusHandle> {
    if !metrics_enabled_from_env() {
        return None;
    }
    RECORDER.get_or_init(install_recorder).as_ref()
}

/// Dump the registry as Prometheus text. Empty string while disabled
/// (REQ-OBS-007: the CLI dump exits 0 with an empty registry when off).
pub fn render() -> String {
    match ensure_recorder() {
        Some(handle) => handle.render(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::{metrics_enabled, render};
    use std::sync::Mutex;

    /// Serializes tests that mutate `MEMENTO_METRICS` (process-wide env).
    static METRICS_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn disabled_when_var_absent_or_not_one() {
        // REQ-OBS-006: MEMENTO_METRICS unset (default) → zero metrics work.
        assert!(!metrics_enabled(None));
        assert!(!metrics_enabled(Some("0")));
        assert!(!metrics_enabled(Some("")));
        assert!(!metrics_enabled(Some("yes")));
    }

    #[test]
    fn enabled_only_when_var_is_one() {
        // REQ-OBS-006: exactly `1` enables the recorder.
        assert!(metrics_enabled(Some("1")));
    }

    #[test]
    fn render_is_empty_when_disabled() {
        // REQ-OBS-006/007: registry off → dump is an empty string (the CLI
        // dump exits 0 with an empty registry; no work happens when off).
        let _guard = METRICS_ENV_LOCK.lock().unwrap();
        // SAFETY: test-only env mutation, serialized by METRICS_ENV_LOCK.
        unsafe { std::env::remove_var("MEMENTO_METRICS") };
        assert_eq!(render(), "");
    }

    #[test]
    fn render_exposes_prometheus_text_when_enabled() {
        // REQ-OBS-006/007: with the recorder installed, counters and
        // histograms render as Prometheus text (no HTTP listener involved —
        // the exporter is compiled with default-features=false, D2).
        let _guard = METRICS_ENV_LOCK.lock().unwrap();
        // SAFETY: test-only env mutation, serialized by METRICS_ENV_LOCK.
        unsafe { std::env::set_var("MEMENTO_METRICS", "1") };

        // First render installs the recorder on an empty registry.
        let before = render();
        assert!(
            !before.contains("memento_test"),
            "empty registry renders no metric lines: {before}"
        );

        metrics::counter!("memento_test_total").increment(1);
        metrics::histogram!("memento_test_duration_ms").record(1.5);

        let after = render();
        assert!(
            after.contains("memento_test_total"),
            "counter renders as Prometheus text: {after}"
        );
        assert!(
            after.contains("memento_test_duration_ms"),
            "histogram renders as Prometheus text: {after}"
        );

        // SAFETY: test-only env mutation, serialized by METRICS_ENV_LOCK.
        unsafe { std::env::remove_var("MEMENTO_METRICS") };
    }
}
