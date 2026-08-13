//! Build script for `memento-embed-fastembed`.
//!
//! `fastembed` 5.x pulls `ort` 2.0.0-rc.13 which hard-requires ONNX Runtime
//! API version 1.28 (compiled-in `ORT_API_VERSION`). Windows ships an
//! `onnxruntime.dll` 1.17.x in `C:\Windows\System32` (used by Edge / DirectML
//! / Microsoft Store apps) and the `ort-load-dynamic` feature scans `PATH`
//! for `onnxruntime.dll` first — that 1.17 DLL gets picked, ort rejects it
//! with `LoadError::BadVersion { version_str: "1.17.1" }`, and the next ort
//! call panics with "Mutex poisoned, abort." when `setup_api` re-enters the
//! poisoned `G_ORT_LIB` OnceLock.
//!
//! Fix: vendor `onnxruntime.dll` 1.28.0 inside this crate
//! (`vendor/onnxruntime/lib/onnxruntime.dll`) and bake its absolute path
//! into the binary via `cargo:rustc-env=ORT_DYLIB_PATH_BAKED=…`. The
//! runtime hook in `src/dylib.rs` calls `ort::init_from(...)` with this
//! path before any `fastembed::TextEmbedding::try_new`, so ort loads the
//! 1.28.0 DLL instead of PATH-resolving to System32's 1.17.x.
//!
//! This script:
//!   * Validates the vendored DLL exists at the expected path (a clear
//!     build error beats a runtime panic in production).
//!   * Emits `ORT_DYLIB_PATH_BAKED` for the runtime to read with `env!()`.
//!   * Reruns only when the vendored DLL changes (binary blob, so the
//!     rest of the crate recompiling must not trigger this script).
//!
//! Source download (pinned, recorded in docs/dependencies.md):
//!   <https://github.com/microsoft/onnxruntime/releases/download/v1.28.0/onnxruntime-win-x64-1.28.0.zip>
//! The release ships `lib/onnxruntime.dll` (15.1 MB) and
//! `lib/onnxruntime_providers_shared.dll` (0.02 MB) — the shared-providers
//! DLL is co-located by Windows side-by-side resolution automatically, so
//! it is NOT vendored. The vendored layout is intentionally
//! `vendor/onnxruntime/lib/onnxruntime.dll` (no version suffix in path)
//! so the build script and runtime agree on a single relative layout.

use std::path::PathBuf;

fn main() {
    // CARGO_MANIFEST_DIR points at this crate's root at build time.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dll_rel = PathBuf::from("vendor/onnxruntime/lib/onnxruntime.dll");
    let dll_abs = manifest_dir.join(&dll_rel);

    if !dll_abs.is_file() {
        panic!(
            "memento-embed-fastembed: vendored ONNX Runtime DLL missing.\n\
             Expected: {}\n\
             Download onnxruntime-win-x64-1.28.0.zip from\n\
             https://github.com/microsoft/onnxruntime/releases/tag/v1.28.0\n\
             and place lib/onnxruntime.dll at the expected path above.\n\
             See crates/memento-embed-fastembed/build.rs for the rationale.",
            dll_abs.display()
        );
    }

    // Rerun this script only when the DLL or this script itself changes.
    // The path is relative to CARGO_MANIFEST_DIR (the build script's CWD).
    println!("cargo:rerun-if-changed=vendor/onnxruntime/lib/onnxruntime.dll");
    println!("cargo:rerun-if-changed=build.rs");

    // Bake the absolute path. ort::init_from takes &Path; the runtime side
    // resolves this with a fallback chain (executable-relative, then
    // CWD-relative) for dev / install layouts where the absolute build-time
    // path no longer exists on the target machine.
    println!("cargo:rustc-env=ORT_DYLIB_PATH_BAKED={}", dll_abs.display());
}
