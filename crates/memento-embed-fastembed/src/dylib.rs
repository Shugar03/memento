//! Vendored ONNX Runtime dynamic-library loader (companion to `build.rs`).
//!
//! `ort` 2.0.0-rc.13 is compiled against ONNX Runtime API 1.28
//! (`ort_sys::ORT_API_VERSION = 28` with cumulative `api-28` feature, see
//! the `version.rs` table in the `ort-sys` crate). The
//! `ort-load-dynamic` feature on `fastembed` tells ort to `libloading`
//! `onnxruntime.dll` at runtime instead of statically linking — but ort
//! reads the DLL path from the `ORT_DYLIB_PATH` env var, and if unset,
//! resolves `onnxruntime.dll` via the standard Windows DLL search order
//! (cwd, then `%PATH%`, then `System32`). On a stock Windows host, the
//! first match is `C:\Windows\System32\onnxruntime.dll` 1.17.260311,
//! which ort rejects with `LoadError::BadVersion { version_str:
//! "1.17.1", … }` because 17 < 28.
//!
//! **Fix:** this module calls `ort::init_from(...)` with the path baked
//! by `build.rs` (`ORT_DYLIB_PATH_BAKED`) before any
//! `fastembed::TextEmbedding::try_new`. The call is guarded by a
//! `OnceLock` so the cost is one `libloading::Library::open` per process.
//!
//! **Why not `env::set_var("ORT_DYLIB_PATH", …)` instead?** Setting
//! process env vars in Rust 1.97+ is `unsafe` (the global environment is
//! shared with the C runtime; concurrent readers and DLL search state
//! race). `ort::init_from` takes the path directly and is the
//! first-party escape hatch — it is the same `pub fn` `ort` exports for
//! users who want a custom loading scheme.
//!
//! **Why pre-load before fastembed and not lazily on first embed?** The
//! first call to `ort::api()` triggers `setup_api`, which calls
//! `load_dynamic::init(path)` exactly once. If the first call lands on
//! a non-1.28 DLL, the `OnceLock` for `G_ORT_LIB` is poisoned forever
//! in that process and the next `api()` call panics with "Mutex
//! poisoned, abort." Pre-loading guarantees the 1.28 DLL wins.
//!
//! **Test-only behaviour:** unit tests inject a fake backend via
//! `ModelLoader::from_backend`, so they never reach `try_new` and this
//! module's `ensure_loaded` is a no-op there (the vendored DLL is
//! validated at build time only).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Path baked at build time by `build.rs`. The build script panics if
/// the DLL is missing, so this env var is set on every normal build
/// (including release and `cargo test`).
const BAKED_PATH: &str = env!("ORT_DYLIB_PATH_BAKED");

/// Filename ort looks for when `ORT_DYLIB_PATH` is unset. We use it as
/// the suffix when probing the executable / cwd for a user-supplied
/// runtime (so power users can drop `onnxruntime.dll` next to the
/// binary and override the baked path without rebuilding).
#[cfg(windows)]
const DYLIB_FILENAME: &str = "onnxruntime.dll";
#[cfg(target_os = "linux")]
const DYLIB_FILENAME: &str = "libonnxruntime.so";
#[cfg(target_os = "macos")]
const DYLIB_FILENAME: &str = "libonnxruntime.dylib";

/// Resolves the runtime DLL path, in order of preference:
///
/// 1. `ORT_DYLIB_PATH` env var (lets ops pin a known-good runtime
///    without rebuilding — the documented override path).
/// 2. `ORT_DYLIB_PATH_BAKED` (set by `build.rs`; the vendored 1.28 DLL
///    inside the crate).
/// 3. `<exe-dir>/onnxruntime.dll` (next-to-binary deployment).
/// 4. `./onnxruntime.dll` relative to the current working directory
///    (dev-mode smoke test convenience).
///
/// Each candidate must exist on disk; the first hit wins.
fn resolve_dylib_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ORT_DYLIB_PATH")
        && !p.is_empty()
    {
        let pb = PathBuf::from(&p);
        if pb.is_file() {
            return Some(pb);
        }
        eprintln!(
            "memento-embed-fastembed: ORT_DYLIB_PATH={p} is set but the file does not exist; falling back"
        );
    }

    let baked = PathBuf::from(BAKED_PATH);
    if baked.is_file() {
        return Some(baked);
    }

    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join(DYLIB_FILENAME);
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    let cwd_candidate = Path::new(DYLIB_FILENAME);
    if cwd_candidate.is_file() {
        return Some(cwd_candidate.to_path_buf());
    }

    None
}

/// Idempotent pre-loader. Calls `ort::init_from` exactly once per
/// process. A second call is a no-op (ort's internal `G_ORT_LIB`
/// `OnceLock` would absorb the duplicate load anyway, but we avoid the
/// cost and the log line).
///
/// If the resolved DLL cannot be found, the function returns silently
/// — the user is probably in a unit test or has intentionally stripped
/// the vendored blob. The first `fastembed::TextEmbedding::try_new`
/// will then surface a clear `EmbeddingFailed` from fastembed with the
/// underlying `ort` error.
pub fn ensure_loaded() {
    static LOADED: OnceLock<()> = OnceLock::new();
    LOADED.get_or_init(|| {
        let Some(path) = resolve_dylib_path() else {
            // No DLL found at any of the four probe sites. Unit tests
            // that exercise the fake backend (`ModelLoader::from_backend`)
            // never reach this code path; a real-binary deployment that
            // ships without the vendored blob surfaces a clear error
            // from `fastembed::TextEmbedding::try_new` instead of a
            // panic from this module.
            return;
        };
        if let Err(err) = ort::init_from(&path) {
            // Log but do not panic: the application layer surfaces the
            // structured `EMBEDDING_FAILED` error to the caller with the
            // ort error message, which is more useful than a panic from
            // a process-wide global.
            eprintln!(
                "memento-embed-fastembed: failed to pre-load ONNX Runtime dylib at {}: {err}",
                path.display()
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `BAKED_PATH` must point at an existing file in the build
    /// environment that runs the tests (`build.rs` panics otherwise).
    /// This is the cheapest invariant test: if the vendored blob is
    /// accidentally deleted or the build script is broken, this test
    /// fails before any `try_new` is reached.
    #[test]
    fn baked_path_resolves() {
        let baked = PathBuf::from(BAKED_PATH);
        assert!(baked.is_file(), "baked path {baked:?} must exist");
    }

    /// Fallback chain returns the baked path when `ORT_DYLIB_PATH` is
    /// not set, which is the normal test environment.
    #[test]
    fn resolve_prefers_baked_when_no_env_override() {
        // SAFETY: test-only; single-threaded test runtime.
        unsafe { std::env::remove_var("ORT_DYLIB_PATH") };
        let resolved = resolve_dylib_path().expect("must resolve a DLL");
        let baked = PathBuf::from(BAKED_PATH);
        assert_eq!(resolved.canonicalize().ok(), baked.canonicalize().ok());
    }

    /// `ORT_DYLIB_PATH` wins when set and the file exists.
    #[test]
    fn env_override_wins() {
        // SAFETY: test-only; single-threaded test runtime.
        let baked = PathBuf::from(BAKED_PATH);
        unsafe { std::env::set_var("ORT_DYLIB_PATH", &baked) };
        let resolved = resolve_dylib_path().expect("must resolve a DLL");
        unsafe { std::env::remove_var("ORT_DYLIB_PATH") };
        assert_eq!(
            resolved.canonicalize().ok(),
            baked.canonicalize().ok(),
            "env override must be honored when the file exists"
        );
    }
}
