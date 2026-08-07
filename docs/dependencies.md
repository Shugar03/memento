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
| `@firecrawl/anydoc` | `0.1.7` (npm latest as of 2026-08-07) | https://www.npmjs.com/package/@firecrawl/anydoc | bump the pinned version in the `package.json` / `npx` invocation (batch 4); re-run `memento-parse` tests; update this table; commit together |

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

## Externally blocked work

- T-040+ (`memento-okf` index) depends on the okf-rs pin above.
- T-031 (`memento-parse` anydoc subprocess) depends on the anydoc pin above.

## Deviations from "latest stable" (decided at apply time, 2026-08-07)

| Dependency | Choice | Why |
|---|---|---|
| `fastembed` | `default-features = false` + `["ort-load-dynamic", "hf-hub-rustls-tls"]` | Defaults pull `image-models` (→ `image` → `zune-jpeg` — see below) and `ort-download-binaries-native-tls` (no prebuilt onnxruntime for `x86_64-pc-windows-gnu`; native-tls needs OpenSSL on GNU). `ort-load-dynamic` loads `onnxruntime` at runtime (ship the DLL/`.so` with the app; see batch 3 model-loader task), `hf-hub-rustls-tls` is pure-Rust TLS. Text models are unconditional in fastembed 5.x. CLIP/image models stay deferred per design; re-enabling `image-models` later requires the zune-jpeg fix below first. |
| `zune-jpeg` (transitive) | NOT patched; documented | `0.5.15` (latest stable, 2026-03-26) fails on rustc 1.97: `warn!(...)` in expression position (`mcu_prog.rs:463`). Upstream fix exists on `dev` (commit `0346b875169ed528e206441c97899d99002e17ca`, zune-image repo); only pre-release `0.5.16-rc1` (2026-08-07) exists. Recipe when needed: `[patch.crates-io] zune-jpeg = { git = "https://github.com/etemesi254/zune-image", rev = "<fixed-sha>" }`. |
| `tokenizers` | `"0.22"` instead of `"0.23"` | fastembed 5.x requires `^0.22.2`; aligning the workspace pin avoids building two tokenizers copies. |
| `lancedb` | `"0.33"`, default features | The `native-tls` feature no longer exists in lancedb ≥0.27 (removed upstream). MVP uses local file-based stores — TLS irrelevant; revisit if remote object stores are added. |

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

