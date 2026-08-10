# Referencia de configuración — Memento RS

> Todos los parámetros por tenant, variables de entorno, flags CLI y
> presets relevantes. Para el runbook operativo, véase
> [docs/ops.es.md](ops.es.md).

## Archivos de configuración

### `db/tenants/<tid>/config.toml` — por tenant

```toml
[tenant]
name = "mi-proyecto"

[retention]
days = 30            # 0 = desactivar; por defecto 30
audit_days = 365     # opcional; ausente = espejo de `days`; 0 = desactivar
```

| Campo | Tipo | Default | Significado |
|---|---|---|---|
| `tenant.name` | string | `""` | Etiqueta humana (no se usa en routing). |
| `retention.days` | u64 | `30` | Horizonte en días para el sweep de datos (REQ-ML-003). `0` desactiva. |
| `retention.audit_days` | u64? | `None` | Horizonte independiente para el audit log (T-120). `None` = mismo que `days`. `0` = desactivar. |

Las escrituras son atómicas (temp + rename). Un archivo corrupto o
ausente cae al default `30 / None`, **nunca** a un error.

### `~/.memento/config.toml` — raíz

Opcional. En su ausencia, los defaults son:

```toml
[memento]
# version del schema de raíz; presente si el usuario lo crea a mano.
# No hay campos requeridos en el MVP — la configuración por tenant
# vive en db/tenants/<tid>/config.toml.
```

## Variables de entorno

| Variable | Aplica a | Default | Significado |
|---|---|---|---|
| `MEMENTO_TOKEN` | `memento`, `memento-mcp` | requerido (REQ-TA-002) | Bearer token. `memento tenant create` lo imprime una sola vez. |
| `MEMENTO_AGENT_ID` | `memento`, `memento-mcp` | requerido para `mcp` (REQ-TA-003) | Identidad auditada en cada acción. |
| `MEMENTO_ROOT` | todo | `~/.memento` | Raíz del store. Override de `--root`. |
| `MEMENTO_LOCALE` | `memento` (CLI/MCP) | `es` | `es` o `en`. |
| `MEMENTO_INTERVAL_HOURS` | `memento-worker` | `24` | Periodo del timer del daemon. |
| `MEMENTO_MODELS_DIR` | fastembed | `<root>/models` | Caché de ONNX (MultilingualE5Small ~500 MB). |
| `MEMENTO_BENCH_CHUNKS` | bench | `100000` | Tamaño del corpus de search bench. |
| `MEMENTO_BENCH_LOC` | bench | `10000` (10k), `100000` (100k) | LOC del corpus de code-index bench. |
| `MEMENTO_BENCH_EMBED` | bench | `0` | Si `1`, fuerza la corrida de embed bench. |
| `RUST_LOG` | todo | `info` | Filtrado tracing. |

El `memento-worker` **no** requiere `MEMENTO_TOKEN` ni `MEMENTO_AGENT_ID`
(su identidad es operativa, no de tenant).

## Flags CLI (top-level)

| Flag | Aplica a | Significado |
|---|---|---|
| `--root <path>` | todo | Override de `MEMENTO_ROOT` (REQ-CL-005). |
| `--locale <es\|en>` | `memento` (CLI/MCP) | Override de `MEMENTO_LOCALE`. |
| `--json` | `memento` | Salida estructurada `{code, message, exit_code}`. |

`--json` se aplica **global**: cubre help y errores.

## Flags por subcomando

### `memento memory`

| Subcommand | Flag | Significado |
|---|---|---|
| `ingest-text` | `--text <s>` | Texto plano a ingestar. |
| `ingest-document` | `--source <path>` | Ruta al documento (14 formatos vía anydoc). |
| `bulk` | `<dir>` | Ingesta en lote con reporte por archivo (REQ-CL-002). |
| | `--source <name>` | Etiqueta `source` para provenance. |
| `search` | `--query <s>` | Query (REQ-MR-001). |
| | `--top-k <n>` | Número de hits (default 10). |
| | `--workspace <id>` | Filtra por workspace. |
| | `--rrf` | Habilita híbrido (dense + sparse, RRF k=60). |
| | `--doc-id <id>` | Filtra por doc. |
| `get-chunk` | `--chunk-id <id>` | Recupera un fragmento. |
| `feedback` | `--chunk-id <id>` | Fragmento a puntuar. |
| | `--score <0.0..1.0>` | Bonus positivo (REQ-ML-001). |
| | `--reason <s>` | Texto libre (auditado). |
| `delete` | `--scope <chunk\|doc\|workspace\|tenant>` | Alcance del borrado. |
| | `--id <uuid>` | Requerido para `chunk`/`doc`. |
| `context-fit` | `--query <s>` | Query de candidatos. |
| | `--budget <tokens>` | Presupuesto de tokens. |
| | `--top-k <n>` | Tope de candidatos. |
| `stats` | `--workspace <id>` | Métricas por workspace (REQ-CL-006). |
| `health` | (sin flags) | Sonda usada por docker-compose. |

### `memento tenant`

| Subcommand | Flag | Significado |
|---|---|---|
| `create` | `--name <s>` | Nombre humano del tenant. |
| `rotate-token` | (sin flags) | Invalida el token anterior al instante. |
| `delete` | `--tenant` | Ceremonia de borrado GDPR. |
| `retention` | `--days <n>` | Ver o modificar el horizonte. |
| `export` | (sin flags) | Exporta chunks + provenance + feedback + config (JSONL → tar.gz). |
| `backup` | (sin flags) | Crea respaldo cifrado (compact → copy → encrypt). |
| `restore` | `<dir>` | Restore offline (store quieto). |
| `sweep` | (sin flags) | Ejecuta el sweep al instante. |

### `memento code`

| Subcommand | Flag | Significado |
|---|---|---|
| `index` | `<path>` | Indexa un repo (Rust + Python). |
| `status` | `--project <id>` | Estado L1–L4 + overview L4. |
| `debug` | `<project-id>` | Dump `{nodes, edges}` + verdict (REQ-CK-009). |
| `symbol-lookup` | `--symbol <s>` | Lookup < 5 ms vía LanceDB symbols. |
| `callers-of` / `callees-of` | `--symbol <s>` | Recorrido depth-2 (REQ-CK-005). |
| `impact` | `--symbol <s>` | Reachability reverso (REQ-CK-006). |
| `dependencies` | `--module <id>` | Aristas de módulo + ciclos. |
| `search` | `--query <s>` | Búsqueda literal/semántica. |
| | `--literal` | Sólo literal (para `--no-embeddings`). |
| `graph-dump` | `--project <id>` | Dump canónico `{nodes, edges}` (REQ-CK-009). |

### `memento-worker`

| Flag | Significado |
|---|---|
| `--now` | One-shot: ejecuta sweep + maintenance + backup y sale. |
| `--root <path>` | Override de `MEMENTO_ROOT`. |
| `--interval-hours <N>` | Periodo del daemon (default `MEMENTO_INTERVAL_HOURS` o 24). |

## Constantes operativas

| Constante | Valor | Fuente |
|---|---|---|
| Default `retention_days` | 30 | `memento_application::tenant_config::DEFAULT_RETENTION_DAYS` |
| RRF `k` | 60 | `memento_application::search::RRF_K` |
| Token format | `memo_<tid>_<48×base62>` | REQ-TA-006 |
| Argon2id params | `m=19MiB, t=2, p=1` | REQ-TA-006 / D3 |
| Chunk bounds | `[256, 300]` tokens | REQ-MC-003 |
| Chunk overlap | `[26, 45]` tokens (10–15%) | REQ-MC-003 |
| Embed batch size | 64 | D1 |
| Embed model | MultilingualE5Small 384d | D1 |
| Embed cache | `~/.memento/models` | D1 |
| Subprocess stdout cap | 50 MiB | T-031 |
| Subprocess timeout | 60 s | T-031 |
| Ingest blob limit | 10 MiB | T-060 |
| Ingest chunk limit | 10 000 | T-060 |
| Bulk ingest walker | canonical_within | T-080 / T-083 |
| Audit retention default | mirror de `retention_days` | T-120 |

## Códigos de error (REQ-CL-005 / REQ-MS-005)

Compartidos entre CLI exit codes y MCP `CallToolResult::error`:

| Código | Exit | Significado |
|---|---|---|
| `AUTH_FAILED` | 4 | Credencial inválida o ausente (REQ-TA-002). |
| `INVALID_INPUT` | 5 | Parámetro mal formado o ausente. |
| `NOT_FOUND` | 6 | Recurso inexistente o cross-tenant. |
| `CONFLICT` | 7 | Versión/schema no coincide. |
| `INTERNAL` | 10 | Error interno (bug o IO). |
| `BACKUP_CORRUPT` | 11 | Restore: backup no verificable (REQ-ML-005). |
| `CHUNK_OVERFLOW` | 12 | Ingesta: > 10k chunks estimados (REQ-MC-005). |
| `SUBPROCESS_ARGV_INVALID` | 13 | Subproceso: argv no permitido (REQ-MC-002). |
| `SUBPROCESS_TIMEOUT` | 14 | Subproceso: timeout 60 s (REQ-MC-002). |
| `SUBPROCESS_OUTPUT_TOO_LARGE` | 15 | Subproceso: stdout > 50 MiB. |
| `TENANT_EXISTS` | 16 | `tenant create`: tid ya presente. |

Lista exhaustiva en `crates/memento-domain/src/error.rs`.
