# Desarrollo — Memento RS

## Receta de entorno (Windows + w64devkit)

El host de desarrollo no tiene MSVC linker (`winget install` falla por
espacio en disco). Trabajamos con `windows-gnu` + `w64devkit`. Antes de
**cada** shell, exporta:

```powershell
$env:RUSTUP_HOME = "F:\OPENCODE proyectos\.toolchains\rustup"
$env:CARGO_HOME  = "F:\OPENCODE proyectos\.toolchains\cargo"
$env:LIBRARY_PATH = "F:\OPENCODE proyectos\.toolchains\w64devkit\lib\gcc\x86_64-w64-mingw32\16.1.0"
$env:Path = "$env:USERPROFILE\.cargo\bin;F:\OPENCODE proyectos\.toolchains\w64devkit\bin;F:\OPENCODE proyectos\.toolchains\protoc\bin;$env:Path"
```

Hay un script que lo hace por ti:

```powershell
scripts/dev-setup.ps1
```

## Scripts de desarrollo (cross-platform)

| Script                     | Qué hace                                                                                  |
|----------------------------|-------------------------------------------------------------------------------------------|
| `scripts/dev-setup.ps1`    | Exporta las variables de toolchain (arriba). Ejecutar una vez por shell.                  |
| `scripts/dev-test.ps1`     | `cargo test --workspace -j 2 -- --test-threads=1` (serializa el link, evita OOM en ld).    |
| `scripts/dev-clippy.ps1`   | `cargo clippy --workspace --all-targets --all-features -j 2 -- -D warnings`.               |
| `scripts/dev-bench.ps1`    | `scripts/bench.sh` con `CARGO_EXTRA=-j 2` (benches completos, gate REQ-MR-007/REQ-CK-002). |
| `scripts/dev-format.ps1`   | `cargo fmt --all` (sin `--check`: aplica formato).                                        |

Equivalentes POSIX (los mismos comandos, sin el `dev-setup`):

```bash
cargo test  --workspace -j 2 -- --test-threads=1
cargo clippy --workspace --all-targets --all-features -j 2 -- -D warnings
./scripts/bench.sh --quick
cargo fmt --all
```

## Por qué `-j 2`

`w64devkit`'s `ld` se queda sin memoria cuando dos enlaces de binarios
grandes corren en paralelo (test bins). `-j 2` serializa el link pero
mantiene paralelismo de compilación. `CARGO_BUILD_JOBS=1` también
funciona pero es mucho más lento.

## Perfil dev: line-tables-only

`.cargo/config.toml` define:

```toml
[profile.dev.package."*"]
debug = "line-tables-only"
```

Razón: los binarios de test con debug completo exceden el límite de
imagen PE de Windows (2 GiB) y `CreateProcess` los rechaza con error
193. `line-tables-only` mantiene `file:line` en los backtraces de panic
y reduce el binario ~2.5×.

## Convenciones de commits

- Conventional commits (`feat:`, `fix:`, `chore:`, `docs:`, `ci:`,
  `refactor:`, `test:`) con scope opcional.
- Una unidad de trabajo por commit.
- Sin atribución de IA (`Co-Authored-By`).
- Tests y docs viajan con el código.

Ver `docs/contributing.md` y `docs/ci.md`.

## Estructura del workspace (13 crates)

Hexagonal: `domain → ports → application`, adaptadores finos
(`lancedb`, `embed-fastembed`, `parse`, `okf`, `tenant`) y superficies
(`mcp`, `cli`, `worker`). `memento-i18n` es la tabla ES-first + EN
fallback; `memento-testkit` provee fakes + fixtures + reloj inyectable.

## Pruebas

```bash
# Todo el workspace, serializado
scripts/dev-test.ps1

# Sólo un crate (ciclo de desarrollo)
cargo test -p memento-application -j 2 -- --test-threads=1

# Sólo los benches (sin ejecutar)
cargo bench --no-run
```

El modo TDD estricto está deshabilitado (`strict_tdd: false`); cada
task define sus criterios de aceptación y las pruebas que los verifican.

## Siguiente paso

- [cli-reference.es.md](cli-reference.es.md) — qué hace cada
  subcommand.
- [mcp-clients.es.md](mcp-clients.es.md) — cómo conectar agentes MCP.
