# Development — Memento RS

## Environment recipe (Windows + w64devkit)

The dev host has no MSVC linker (`winget install` fails because C: is
almost full). We use `windows-gnu` + `w64devkit`. Before **every** new
shell, export:

```powershell
$env:RUSTUP_HOME = "F:\OPENCODE proyectos\.toolchains\rustup"
$env:CARGO_HOME  = "F:\OPENCODE proyectos\.toolchains\cargo"
$env:LIBRARY_PATH = "F:\OPENCODE proyectos\.toolchains\w64devkit\lib\gcc\x86_64-w64-mingw32\16.1.0"
$env:Path = "$env:USERPROFILE\.cargo\bin;F:\OPENCODE proyectos\.toolchains\w64devkit\bin;F:\OPENCODE proyectos\.toolchains\protoc\bin;$env:Path"
```

A helper script applies the same recipe:

```powershell
scripts/dev-setup.ps1
```

## Cross-platform dev scripts

| Script                     | What it does                                                                            |
|----------------------------|----------------------------------------------------------------------------------------|
| `scripts/dev-setup.ps1`    | Exports the toolchain env vars (above). Run once per shell.                            |
| `scripts/dev-test.ps1`     | `cargo test --workspace -j 2 -- --test-threads=1` (serializes link, avoids ld OOM).     |
| `scripts/dev-clippy.ps1`   | `cargo clippy --workspace --all-targets --all-features -j 2 -- -D warnings`.           |
| `scripts/dev-bench.ps1`    | `scripts/bench.sh` with `CARGO_EXTRA=-j 2` (full benches; gates REQ-MR-007/REQ-CK-002). |
| `scripts/dev-format.ps1`   | `cargo fmt --all` (no `--check`: applies formatting).                                  |

POSIX equivalents (same commands, no `dev-setup`):

```bash
cargo test  --workspace -j 2 -- --test-threads=1
cargo clippy --workspace --all-targets --all-features -j 2 -- -D warnings
./scripts/bench.sh --quick
cargo fmt --all
```

## Why `-j 2`

`w64devkit`'s `ld` runs out of memory when two large test-binary links
happen in parallel. `-j 2` serializes the link while keeping compile
parallelism. `CARGO_BUILD_JOBS=1` works but is much slower.

## Dev profile: line-tables-only

`.cargo/config.toml` defines:

```toml
[profile.dev.package."*"]
debug = "line-tables-only"
```

Reason: fully-debugged test binaries exceed the Windows PE image limit
(2 GiB); `CreateProcess` rejects them with error 193.
`line-tables-only` keeps `file:line` in panic backtraces and shrinks
binaries ~2.5×.

## Commit conventions

- Conventional commits (`feat:`, `fix:`, `chore:`, `docs:`, `ci:`,
  `refactor:`, `test:`) with optional scope.
- One work unit per commit.
- No AI attribution (`Co-Authored-By`).
- Tests and docs ship with their code.

See `docs/contributing.md` and `docs/ci.md`.

## Workspace layout (13 crates)

Hexagonal: `domain → ports → application`, thin adapters (`lancedb`,
`embed-fastembed`, `parse`, `okf`, `tenant`) and surfaces (`mcp`,
`cli`, `worker`). `memento-i18n` is the ES-first + EN fallback table;
`memento-testkit` provides fakes + fixtures + injectable clock.

## Testing

```bash
# Whole workspace, serialized
scripts/dev-test.ps1

# Single crate (dev loop)
cargo test -p memento-application -j 2 -- --test-threads=1

# Benches compile-only (no execution)
cargo bench --no-run
```

Strict TDD mode is off (`strict_tdd: false`); each task defines its own
acceptance criteria and the tests that verify them.

## Next step

- [cli-reference.en.md](cli-reference.en.md) — what every subcommand
  does.
- [mcp-clients.en.md](mcp-clients.en.md) — how to connect MCP agents.
