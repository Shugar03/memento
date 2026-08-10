# Instalación — Memento RS

## Requisitos

- Rust 1.83 o superior (toolchain estable). El proyecto pinea
  `rust-toolchain.toml` para que `cargo` use la versión correcta.
- 2 vCPU / 2 GB RAM / 20 GB SSD como mínimo recomendado (ver
  `manifesto memoria.md` para números honestos).
- Sin dependencias de red en tiempo de ejecución: LanceDB embebido, ONNX
  local, FTS local.

## Compilar

```bash
git clone <repo-url> memento-rs
cd memento-rs
cargo build --release --workspace -j 2
```

Los binarios quedan en `target/release/`:

- `memento` — CLI (`tenant`, `memory`, `code`, `health`, `stats`).
- `memento-mcp` — servidor MCP stdio.
- `memento-worker` — rotación 24 h (`--now` para ejecutar al instante).

## Crear el primer tenant

```bash
# Crea ~/.memento/ si no existe, y un tenant llamado "default".
# Imprime UNA VEZ el token: guárdalo, no se vuelve a mostrar.
memento tenant create --name default
```

Salida esperada (es-ES):

```
Tenant creado: 7f3e...
Token: memo_7f3e_...
(Este token se muestra una sola vez.)
```

Copia el token en tu `~/.bashrc` o equivalente:

```bash
export MEMENTO_TOKEN=memo_7f3e_...
export MEMENTO_AGENT_ID=claude-code   # o el id de tu agente
```

## Primer ingest

```bash
# Texto plano
memento memory ingest-text --text "Memento es un motor de memoria local multitenent."

# Documento (PDF, DOCX, XLSX, EPUB, … 14 formatos vía anydoc)
memento memory ingest-document --source ./rfc.pdf

# Bulk de un directorio (con reporte por archivo)
memento memory bulk ./docs/ --source markdown
```

> **Aviso:** la primera ingest con `--no-embeddings` no descarga el modelo
> ONNX (~500 MB). Sin esa flag, Memento descarga
> `MultilingualE5Small` a `~/.memento/models/` en la primera llamada de
> embedding.

## Verificación de salud

```bash
memento health
```

Si el tenant y el agente están bien configurados, devuelve `status: ok`
con un resumen del workspace. Esa salida es la sonda de salud usada por
`docker-compose` (ver `docker-compose.yml`).

## Siguiente paso

→ [quickstart.es.md](quickstart.es.md) para un tour guiado de búsqueda
híbrida, `context_fit`, feedback y `code.index`.
