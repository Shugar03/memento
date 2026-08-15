# Configuration reference — Memento RS

> Every per-tenant knob, env var, CLI flag and relevant constant. For
> the ops runbook, see [docs/ops.en.md](ops.en.md).

## Configuration files

### `db/tenants/<tid>/config.toml` — per tenant

```toml
[tenant]
name = "my-project"

[retention]
days = 30            # 0 = opt out; default 30
audit_days = 365     # optional; absent = mirror of `days`; 0 = opt out
```

| Field | Type | Default | Meaning |
|---|---|---|---|
| `tenant.name` | string | `""` | Human label (not used in routing). |
| `retention.days` | u64 | `30` | Data retention horizon in days (REQ-ML-003). `0` opts out. |
| `retention.audit_days` | u64? | `None` | Independent horizon for the audit log (T-120). `None` = same as `days`. `0` = opt out. |

Writes are atomic (temp + rename). A corrupt or absent file falls back
to `30 / None`, **never** to an error.

### `~/.memento/config.toml` — root

Optional. In its absence the defaults are:

```toml
[memento]
# root schema version; present if the user creates it by hand.
# No required fields in the MVP — per-tenant config lives in
# db/tenants/<tid>/config.toml.
```

## Environment variables

| Variable | Applies to | Default | Meaning |
|---|---|---|---|
| `MEMENTO_TOKEN` | `memento`, `memento-mcp` | required (REQ-TA-002) | Bearer token. `memento tenant create` prints it once. |
| `MEMENTO_AGENT_ID` | `memento`, `memento-mcp` | required for `mcp` (REQ-TA-003) | Identity audited on every action. |
| `MEMENTO_ROOT` | all | `~/.memento` | Store root. Overrides `--root`. |
| `MEMENTO_LOCALE` | `memento` (CLI/MCP) | `es` | `es` or `en`. |
| `MEMENTO_INTERVAL_HOURS` | `memento-worker` | `24` | Daemon timer period. |
| `MEMENTO_MODELS_DIR` | fastembed | `<root>/models` | ONNX cache (MultilingualE5Small ~500 MB). |
| `MEMENTO_BENCH_CHUNKS` | bench | `100000` | Search bench corpus size. |
| `MEMENTO_BENCH_LOC` | bench | `10000` (10k), `100000` (100k) | Code-index bench corpus size. |
| `MEMENTO_BENCH_EMBED` | bench | `0` | If `1`, force the embed bench to run. |
| `RUST_LOG` | all | `info` | tracing filter. |
| `MEMENTO_LOG` | `memento` | `0` | `1` = enable the CLI tracing subscriber on stderr (REQ-OBS-001). |
| `MEMENTO_LOG_FORMAT` | all | `pretty` | `pretty` or `json` for the CLI/MCP/worker subscribers (REQ-OBS-002). |
| `MEMENTO_METRICS` | all | `0` | `1` = record Prometheus counters/histograms in memory; no HTTP listener ever bound (REQ-OBS-006/007). |
| `MEMENTO_METRICS_FILE` | `memento` | stdout | Destination override for the `observability metrics` dump (REQ-OBS-007). |
| `MEMENTO_EVENTS` | all | `0` | `1` = append operational events to `logs/<tid>.events.jsonl`; ids and counts only, never content, queries, or credentials (REQ-OBS-008/009). |
| `MEMENTO_OBSERVE_SAMPLES` | `memento-worker` | `0` | `1` = sample RSS bytes + thread count every 30s into the bound tenant's events file (REQ-OBS-011). |
| `MEMENTO_NO_DAEMON` | `memento` (CLI) | `0` | `1` = disable the persistent daemon; force the pre-change one-shot in-process path on every command. Mirrors the `--no-daemon` global flag (REQ-DAEMON-004). |
| `MEMENTO_DAEMON_PIPE_TIMEOUT` | `memento`, `memento-mcp` | `5` | Seconds the daemon bounds a single framed write / handshake before failing the request without stalling (REQ-DAEMON-006 G2). |

`memento-worker` does **not** require `MEMENTO_TOKEN` or
`MEMENTO_AGENT_ID` (operational identity, not tenant-bound).

## Top-level CLI flags

| Flag | Applies to | Meaning |
|---|---|---|
| `--root <path>` | all | Override `MEMENTO_ROOT` (REQ-CL-005). |
| `--locale <es\|en>` | `memento` (CLI/MCP) | Override `MEMENTO_LOCALE`. |
| `--json` | `memento` | Structured output `{code, message, exit_code}`. |
| `--no-daemon` | `memento` (CLI) | Same as `MEMENTO_NO_DAEMON=1`, per invocation. Sets the env var before any startup logic runs, so every transport / spawner / startup check short-circuits without touching the named pipe (REQ-DAEMON-004). |

`--json` is **global**: covers help and errors.

## Per-subcommand flags

### `memento memory`

| Subcommand | Flag | Meaning |
|---|---|---|
| `ingest-text` | `--text <s>` | Plain text to ingest. |
| `ingest-document` | `--source <path>` | Document path (14 formats via anydoc). |
| `bulk` | `<dir>` | Batch ingest with per-file report (REQ-CL-002). |
| | `--source <name>` | `source` label for provenance. |
| `search` | `--query <s>` | Query (REQ-MR-001). |
| | `--top-k <n>` | Hit count (default 10). |
| | `--workspace <id>` | Workspace filter. |
| | `--rrf` | Enable hybrid (dense + sparse, RRF k=60). |
| | `--doc-id <id>` | Doc filter. |
| `get-chunk` | `--chunk-id <id>` | Fetch a chunk. |
| `feedback` | `--chunk-id <id>` | Chunk to score. |
| | `--score <0.0..1.0>` | Positive bonus (REQ-ML-001). |
| | `--reason <s>` | Free text (audited). |
| `delete` | `--scope <chunk\|doc\|workspace\|tenant>` | Delete scope. |
| | `--id <uuid>` | Required for `chunk`/`doc`. |
| `context-fit` | `--query <s>` | Candidate query. |
| | `--budget <tokens>` | Token budget. |
| | `--top-k <n>` | Candidate cap. |
| `stats` | `--workspace <id>` | Per-workspace metrics (REQ-CL-006). |
| `health` | (no flags) | Probe used by docker-compose. |

### `memento tenant`

| Subcommand | Flag | Meaning |
|---|---|---|
| `create` | `--name <s>` | Human tenant name. |
| `rotate-token` | (no flags) | Invalidates the previous token immediately. |
| `delete` | `--tenant` | GDPR erasure ceremony. |
| `retention` | `--days <n>` | View or change the horizon. |
| `export` | (no flags) | Export chunks + provenance + feedback + config (JSONL → tar.gz). |
| `backup` | (no flags) | Create encrypted backup (compact → copy → encrypt). |
| `restore` | `<dir>` | Offline restore (store quiet). |
| `sweep` | (no flags) | Run sweep on demand. |

### `memento code`

| Subcommand | Flag | Meaning |
|---|---|---|
| `index` | `<path>` | Index a repo (Rust + Python). |
| `status` | `--project <id>` | L1–L4 status + L4 overview. |
| `debug` | `<project-id>` | `{nodes, edges}` dump + verdict (REQ-CK-009). |
| `symbol-lookup` | `--symbol <s>` | Lookup < 5 ms via LanceDB symbols. |
| `callers-of` / `callees-of` | `--symbol <s>` | Depth-2 traversal (REQ-CK-005). |
| `impact` | `--symbol <s>` | Reverse reachability (REQ-CK-006). |
| `dependencies` | `--module <id>` | Module edges + cycles. |
| `search` | `--query <s>` | Literal / semantic search. |
| | `--literal` | Literal-only (for `--no-embeddings`). |
| `graph-dump` | `--project <id>` | Canonical `{nodes, edges}` dump (REQ-CK-009). |

### `memento-worker`

| Flag | Meaning |
|---|---|
| `--now` | One-shot: runs sweep + maintenance + backup and exits. |
| `--root <path>` | Override `MEMENTO_ROOT`. |
| `--interval-hours <N>` | Daemon period (default `MEMENTO_INTERVAL_HOURS` or 24). |

## Operating constants

| Constant | Value | Source |
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
| Bulk ingest walker | `canonical_within` | T-080 / T-083 |
| Audit retention default | mirrors `retention_days` | T-120 |

## Error codes (REQ-CL-005 / REQ-MS-005)

Shared between CLI exit codes and MCP `CallToolResult::error`:

| Code | Exit | Meaning |
|---|---|---|
| `AUTH_FAILED` | 4 | Invalid or missing credential (REQ-TA-002). |
| `INVALID_INPUT` | 5 | Malformed or missing parameter. |
| `NOT_FOUND` | 6 | Resource missing or cross-tenant. |
| `CONFLICT` | 7 | Version / schema mismatch. |
| `INTERNAL` | 10 | Internal error (bug or IO). |
| `BACKUP_CORRUPT` | 11 | Restore: backup not verifiable (REQ-ML-005). |
| `CHUNK_OVERFLOW` | 12 | Ingest: > 10k chunks estimated (REQ-MC-005). |
| `SUBPROCESS_ARGV_INVALID` | 13 | Subprocess: argv not allowed (REQ-MC-002). |
| `SUBPROCESS_TIMEOUT` | 14 | Subprocess: 60 s timeout (REQ-MC-002). |
| `SUBPROCESS_OUTPUT_TOO_LARGE` | 15 | Subprocess: stdout > 50 MiB. |
| `TENANT_EXISTS` | 16 | `tenant create`: tid already present. |

Exhaustive list in `crates/memento-domain/src/error.rs`.
