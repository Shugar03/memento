# Dependencies — pinning policy

Two dependencies are pinned outside normal semver resolution because they
are not stable crates.io packages yet.

## Why pinned

- **okf-rs** (`jyjeanne/okf-rs`) — pre-crates.io git dependency. Reproducible
  builds require pinning an exact commit; the default-branch head moves.
- **@firecrawl/anydoc** — pre-1.0 npm package, invoked as a subprocess via
  `npx`. Exact-version pinning avoids surprise behavior changes in the
  document-normalization layer.

## Pin table (verified 2026-08-07)

| Name | Version / source | URL | Upgrade path |
|---|---|---|---|
| `okf-rs` | git `https://github.com/jyjeanne/okf-rs` @ `42842becafac841a97555b203c2c72f39410f0fb` (branch `main`, head as of 2026-08-07); resolves workspace member package **`okf-core`** (repo has 18 member crates; no package named `okf-rs`) | https://github.com/jyjeanne/okf-rs | bump `rev` in `Cargo.toml` → `[workspace.dependencies]`; verify `cargo check -p memento-okf`; update this table; commit together |
| `@firecrawl/anydoc` | `0.1.7` (npm latest as of 2026-08-07) | https://www.npmjs.com/package/@firecrawl/anydoc | bump the pinned version in the `npx` invocation below; re-run `memento-parse` tests; update this table; commit together |

## anydoc invocation (batch 4, T-031)

The subprocess adapter (`crates/memento-parse/src/anydoc.rs`) resolves the
converter at runtime, in order:

1. `anydoc` on PATH (global `npm install -g @firecrawl/anydoc`), else
2. the pinned npm package run shell-free through `node`:
   `<node-dir>/node_modules/npm/bin/npx-cli.js --yes @firecrawl/anydoc@0.1.7`
   — **version pinned in the argv**. Windows cannot spawn `.cmd` shims
   directly (CreateProcess error 193) and `cmd /C` would reintroduce shell
   parsing, so the resolver runs the real npx JS entry with `node.exe`;
   argv stays positional on every platform.

CLI contract mirrored: `anydoc <input-file>` writes GitHub-Flavored Markdown
to stdout (`-o` variant unused; stdout is capped at 50 MiB). Integration
tests use a fake binary (`memento-parse-fake-anydoc`) that mirrors this
argv shape; the real path is exercised by the `#[ignore]`d test
`docx_to_markdown_real_anydoc` (verified manually on the bootstrap host,
2026-08-07).

## Crate pins (workspace)

`[workspace.dependencies]` pins caret-compatible `major.minor` ranges; the
committed `Cargo.lock` pins exact builds for the binaries the workspace
ships (CLI, MCP server, worker). Exact resolved versions live in
`Cargo.lock`.

## How to update

1. Change the version requirement or git `rev`.
2. Resolve: `cargo update -p <crate>` (or a fresh `cargo check --workspace`).
3. Run the affected crate's tests plus `cargo check --workspace`.
4. Update this table and commit the code change and the docs together.

## Vendored model assets

| Asset | Source | License | Why vendored |
|---|---|---|---|
| `crates/memento-parse/assets/spanish-tokenizer.json` | HF `dccuchile/bert-base-spanish-wwm-uncased` (tokenizer.json, ~486 KiB) | Apache-2.0 | Deterministic, offline chunking (REQ-MC-003): identical bytes on every machine ⇒ identical chunk boundaries. Embedded via `include_bytes!` (also the D6 context_fit budget tokenizer). First-run model download (ONNX) is unaffected — see `docs/ci.md`. |

## MultilingualE5Base (2026-08-10 swap)

Default embedder is now `fastembed::EmbeddingModel::MultilingualE5Base`
(768 dims, ~250 MB ONNX), replacing `MultilingualE5Small` (384 dims,
~130 MB ONNX). Picked because Phase 1 v3 showed E5Small's ES↔EN alignment
was too weak for the Spanish-first persona — paraphrase queries like
"como escribir titulos magneticos" returned empty or off-topic results on
EN-only corpora. E5Base has the same Multilingual E5 family alignment
properties with 2× the parameters and a more discriminative 768-d
embedding space, which materially improves cross-lingual cosine ranking
on small corpora (the typical LATAM startup corpus size).

The `embedder` crate's `MODEL_VERSION` and the application layer's
`embedding_model_version()` are pinned to `multilingual-e5-base-v0.0.3`,
switching to `multilingual-e5-base-int8-v0.0.3` when the
`MEMENTO_QUANTIZED_MODEL` env toggle is set (P2 — see the "int8 quantization"
row below).
The LanceDB `chunks` schema column `vector` (FixedSizeList(Float32,
768)) and the testkit's `StubEmbedPort` were bumped together so
production and tests stay in lockstep. The vector index `n_pq = dim/4`
goes from 96 to 192 (the IVF-PQ builder in
`crates/memento-lancedb/src/vector.rs` derives this from
`EMBEDDING_DIM`, no extra config). Tenants indexed with E5Small must
re-ingest against an E5Base tenant (or be re-embedded in place) before
queries return coherent results — vectors of different dimensions are
not comparable.

Bump policy: when fastembed ships a new E5 model version, update
`MODEL_VERSION` (and mirror it in `memento_application::embedding_model_version()`),
refresh the `Cargo.lock`, and re-run the Phase 1 v3 ES-paraphrase
queries to confirm the alignment improvement is preserved.

## Externally blocked work

- T-040+ (`memento-okf` index) depends on the okf-rs pin above.
- T-031 (`memento-parse` anydoc subprocess) depends on the anydoc pin above.

## Deviations from "latest stable" (decided at apply time, 2026-08-07)

| Dependency | Choice | Why |
|---|---|---|
| `fastembed` | `default-features = false` + `["ort-load-dynamic", "hf-hub-rustls-tls"]` | Defaults pull `image-models` (→ `image` → `zune-jpeg` — see below) and `ort-download-binaries-native-tls` (no prebuilt onnxruntime for `x86_64-pc-windows-gnu`; native-tls needs OpenSSL on GNU). `ort-load-dynamic` loads `onnxruntime` at runtime (ship the DLL/`.so` with the app; see `onnxruntime` row below), `hf-hub-rustls-tls` is pure-Rust TLS. Text models are unconditional in fastembed 5.x. CLIP/image models stay deferred per design; re-enabling `image-models` later requires the zune-jpeg fix below first. **Default text model is `MultilingualE5Base` (768d, ~250 MB ONNX) — see "MultilingualE5Base (2026-08-10 swap)" below for the rationale and the 2026-08-10 swap from E5Small.** |
| `onnxruntime` (vendored 1.28.0) | `crates/memento-embed-fastembed/vendor/onnxruntime/lib/onnxruntime.dll` (15.1 MB) | `ort` 2.0.0-rc.13 is compiled against the ONNX Runtime 1.28 API (`ort_sys::ORT_API_VERSION = 28`). The `ort-load-dynamic` feature resolves `onnxruntime.dll` via the standard Windows search order (cwd → PATH → `System32`), and `C:\Windows\System32\onnxruntime.dll` 1.17.260311 (shipped by Edge / DirectML / Microsoft Store) is rejected with `LoadError::BadVersion { version_str: "1.17.1" }` and poisons the ort `OnceLock`, panicking on the next embed call. Fix: vendor the 1.28.0 DLL inside the crate and let `dylib.rs` call `ort::init_from(<baked path>)` before any `fastembed::TextEmbedding::try_new`. Build script (`build.rs`) panics if the DLL is missing, baking the absolute path into `ORT_DYLIB_PATH_BAKED` for compile-time resolution. Override order at runtime: `ORT_DYLIB_PATH` env var > baked path > `<exe-dir>/onnxruntime.dll` > `./onnxruntime.dll`. Source: <https://github.com/microsoft/onnxruntime/releases/tag/v1.28.0> (`onnxruntime-win-x64-1.28.0.zip`, CPU provider). Bump policy: when fastembed upgrades to an ort version requiring a new ORT API major, update the vendored DLL to the matching `onnxruntime-win-x64-<version>.zip` release and verify `cargo test -p memento-embed-fastembed` plus an end-to-end `memento ingest text` pass. |
| `zune-jpeg` (transitive) | NOT patched; documented | `0.5.15` (latest stable, 2026-03-26) fails on rustc 1.97: `warn!(...)` in expression position (`mcu_prog.rs:463`). Upstream fix exists on `dev` (commit `0346b875169ed528e206441c97899d99002e17ca`, zune-image repo); only pre-release `0.5.16-rc1` (2026-08-07) exists. Recipe when needed: `[patch.crates-io] zune-jpeg = { git = "https://github.com/etemesi254/zune-image", rev = "<fixed-sha>" }`. |
| `tokenizers` | `"0.22"` instead of `"0.23"` | fastembed 5.x requires `^0.22.2`; aligning the workspace pin avoids building two tokenizers copies. |
| `text-splitter` | `"0.32"`, `tokenizers` feature **dropped** | The upstream `tokenizers` feature drags tokenizers `0.23` + `onig` into the tree (its `ChunkSizer` impl exists only for its own 0.23 copy), while the workspace pins 0.22 for fastembed. `memento-parse` implements `ChunkSizer` for a local wrapper (`SpanishTokenizer`, `src/chunk.rs`) over the single 0.22 copy, mirroring upstream counting semantics (padding skipped, truncation overflow accounted). |
| `lancedb` | `"0.33"`, default features | The `native-tls` feature no longer exists in lancedb ≥0.27 (removed upstream). MVP uses local file-based stores — TLS irrelevant; revisit if remote object stores are added. |
| `multilingual-e5-base` **int8 (P2, opt-in)** | user-defined ONNX via `TextEmbedding::try_new_from_user_defined`, behind `MEMENTO_QUANTIZED_MODEL` env | MultilingualE5Base has NO quantized variant in the fastembed enum, so the ONNX is self-quantized with `onnxruntime.quantization.quantize_dynamic` (dynamic, `QuantType.QInt8`, no calibration — 2026-08-11, `F:\target\tmp\memento-ir\quantize_e5base.py`). File 1058.6 MB → 265.3 MB (−75%). Wired in `crates/memento-embed-fastembed/src/model.rs`: when the env var is set, `FastEmbedBackend::try_new` reads the onnx + tokenizer files (same directory) and commits from memory with `Pooling::Mean` and `MAX_EMBED_LEN`, identical graph I/O and dim (768). **Opt-in by design**: the stock FP32 enum path remains the default until a broader corpus confirms the measured RRF MRR@5 cost (−0.036 on the 14-query golden set, gate PASS) is acceptable. Tokenizer files copied next to the onnx under `models/int8/` (git-ignored — not committed). Python `onnx` package was required by `onnxruntime.quantization` (installed alongside the existing `onnxruntime 1.28.0`). |

## Environment note (Windows local builds)

The bootstrap host has no MSVC Build Tools (and no free C: space for them), so
local checks run on the `x86_64-pc-windows-gnu` toolchain (rustup home and
cargo home redirected to `F:\OPENCODE proyectos\.toolchains\`, w64devkit gcc,
protoc 35.1 on PATH). CI runs `ubuntu-latest` (MSVC irrelevant there). To
switch a future session to this toolchain:

```powershell
$env:RUSTUP_HOME='F:\OPENCODE proyectos\.toolchains\rustup'
$env:CARGO_HOME='F:\OPENCODE proyectos\.toolchains\cargo'
$env:LIBRARY_PATH='F:\OPENCODE proyectos\.toolchains\w64devkit\lib\gcc\x86_64-w64-mingw32\16.1.0'
$env:Path="$env:USERPROFILE\.cargo\bin;F:\OPENCODE proyectos\.toolchains\w64devkit\bin;F:\OPENCODE proyectos\.toolchains\protoc\bin;$env:Path"
```

An empty `libgcc_eh.a` stub was added to the w64devkit gcc lib dir (w64devkit
omits it; rustc's windows-gnu link line references `-lgcc_eh`).

