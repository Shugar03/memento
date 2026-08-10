# Referencia CLI — `memento`

Todas las subcommands comparten:

- `--root <path>` — raíz alternativa (default `~/.memento`). Override
  sobre `MEMENTO_ROOT`.
- `--json` — salida estructurada `{code, message, detail, exit_code}`
  en stderr.
- `--locale <es|en>` — forzar idioma en help y mensajes.
- `MEMENTO_TOKEN`, `MEMENTO_AGENT_ID` — credenciales del tenant.

Códigos de salida (REQ-CL-005) están definidos en `DomainError::exit_code`
y compartidos con el servidor MCP (REQ-MS-005).

## `memento tenant`

| Subcommand          | Descripción                                          |
|---------------------|------------------------------------------------------|
| `create --name <n>` | Bootstrap sin auth (sólo imprime el token una vez).  |
| `rotate-token`      | Invalida el token anterior inmediatamente.          |
| `delete --tenant`   | Ceremonia de confirmación por stdin → erase + limpieza de credenciales. |
| `retention`         | `--days N` muestra/establece el horizonte (REQ-ML-003, REQ-CG-002). `0` desactiva. |
| `export`            | Exporta chunks + provenance + feedback + config (JSONL → tar.gz). |
| `backup`            | Crea un respaldo cifrado AES-256-GCM en `backups/<tid>/<ts>/`. |
| `restore <dir>`     | Operación offline; el store debe estar quieto.       |
| `sweep`             | Ejecuta la rotación de retención al instante.       |

## `memento memory`

| Subcommand                         | Descripción                                  |
|------------------------------------|----------------------------------------------|
| `ingest-text --text <s>`           | Ingresa texto plano.                         |
| `ingest-document --source <path>`  | 14 formatos vía anydoc (fallback: md/txt).   |
| `bulk <dir> [--source <s>]`        | Ingesta en lote con reporte por archivo.     |
| `search --query <s> [--rrf]`       | FTS por defecto; `--rrf` activa híbrido.     |
| `get-chunk --chunk-id <id>`        | Recupera un fragmento con su procedencia.    |
| `feedback --chunk-id <id> --score` | `--score 1.0` (útil) / `0.0` (no útil).      |
| `delete --scope <chunk\|doc\|workspace\|tenant>` | Borrado permanente.            |
| `context-fit --query <s> --budget` | Selecciona fragmentos dentro de un presupuesto de tokens. |
| `stats [--workspace <id>]`         | Métricas por workspace (REQ-CL-006).         |
| `health`                           | Sonda de salud usada por docker-compose.     |

## `memento code`

| Subcommand                          | Descripción                            |
|-------------------------------------|----------------------------------------|
| `index <path>`                      | Indexa un repo (Rust + Python).        |
| `status [--project <id>]`           | Estado L1–L4 + overview L4.            |
| `debug <project-id>`                | Dump `{nodes, edges}` + verdict.       |
| `symbol-lookup --symbol <s>`        | Lookup < 5 ms vía LanceDB symbols.     |
| `callers-of / callees-of --symbol`  | Recorrido depth-2 (REQ-CK-005).        |
| `impact --symbol <s>`               | Reachability reverso (REQ-CK-006).     |
| `dependencies [--module <id>]`      | Aristas de módulo + ciclos.            |
| `search --query <s> [--literal]`    | Literal bajo `--no-embeddings`.        |
| `graph-dump --project <id>`         | Dump canónico (REQ-CK-009).            |

## `memento-worker`

Subproceso separado; **no requiere `MEMENTO_TOKEN`** (su identidad es
operativa, no de tenant). Configuración por env / flag:

- `--now` — one-shot: ejecuta sweep + maintenance + backup y sale. Exit
  1 si alguna falla (REQ-OP-002).
- `--root <path>` — raíz del store (default `~/.memento`).
- `--interval-hours <N>` — periodo del daemon (default 24).

Daemon: `Ctrl-C` / `SIGTERM` apaga **entre runs** (los jobs en vuelo
terminan).

## Mensajes estructurados (modo `--json`)

Todo error se imprime como:

```json
{
  "code": "AUTH_FAILED",
  "exit_code": 4,
  "message_es": "Falló la autenticación del tenant.",
  "message_en": "Tenant authentication failed.",
  "detail": "Token no coincide con credentials.toml"
}
```

El campo `code` es estable (D7); `message_es`/`message_en` se eligen
según `--locale` o `MEMENTO_LOCALE`.
