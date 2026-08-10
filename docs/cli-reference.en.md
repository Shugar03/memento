# CLI Reference — `memento`

All subcommands share:

- `--root <path>` — alternate root (default `~/.memento`). Overrides
  `MEMENTO_ROOT`.
- `--json` — structured output `{code, message, detail, exit_code}` on
  stderr.
- `--locale <es|en>` — force help / message language.
- `MEMENTO_TOKEN`, `MEMENTO_AGENT_ID` — tenant credentials.

Exit codes (REQ-CL-005) live on `DomainError::exit_code` and are shared
with the MCP server (REQ-MS-005).

## `memento tenant`

| Subcommand          | Description                                          |
|---------------------|------------------------------------------------------|
| `create --name <n>` | Bootstrap, no auth (prints the token once).          |
| `rotate-token`      | Invalidates the previous token immediately.          |
| `delete --tenant`   | Stdin confirmation ceremony → erase + credentials cleanup. |
| `retention`         | `--days N` shows/sets the horizon (REQ-ML-003, REQ-CG-002). `0` disables. |
| `export`            | Exports chunks + provenance + feedback + config (JSONL → tar.gz). |
| `backup`            | Creates an AES-256-GCM encrypted backup at `backups/<tid>/<ts>/`. |
| `restore <dir>`     | Offline op; the store must be quiesced.              |
| `sweep`             | Runs the retention sweep immediately.                |

## `memento memory`

| Subcommand                         | Description                                  |
|------------------------------------|----------------------------------------------|
| `ingest-text --text <s>`           | Ingest plain text.                           |
| `ingest-document --source <path>`  | 14 formats via anydoc (fallback: md/txt).    |
| `bulk <dir> [--source <s>]`        | Bulk ingest with per-file report.            |
| `search --query <s> [--rrf]`       | FTS by default; `--rrf` enables hybrid.      |
| `get-chunk --chunk-id <id>`        | Retrieve a chunk with full provenance.       |
| `feedback --chunk-id <id> --score` | `--score 1.0` (useful) / `0.0` (not useful). |
| `delete --scope <chunk\|doc\|workspace\|tenant>` | Hard delete.                       |
| `context-fit --query <s> --budget` | Select chunks that fit a token budget.       |
| `stats [--workspace <id>]`         | Per-workspace metrics (REQ-CL-006).          |
| `health`                           | Health probe used by docker-compose.         |

## `memento code`

| Subcommand                          | Description                            |
|-------------------------------------|----------------------------------------|
| `index <path>`                      | Index a repo (Rust + Python).          |
| `status [--project <id>]`           | L1–L4 state + L4 overview.             |
| `debug <project-id>`                | `{nodes, edges}` dump + integrity verdict. |
| `symbol-lookup --symbol <s>`        | Lookup < 5 ms via LanceDB symbols.     |
| `callers-of / callees-of --symbol`  | Depth-2 traversal (REQ-CK-005).        |
| `impact --symbol <s>`               | Reverse reachability (REQ-CK-006).     |
| `dependencies [--module <id>]`      | Module edges + cycles.                 |
| `search --query <s> [--literal]`    | Literal under `--no-embeddings`.       |
| `graph-dump --project <id>`         | Canonical dump (REQ-CK-009).           |

## `memento-worker`

A separate process; **does NOT require `MEMENTO_TOKEN`** (its identity
is operational, not tenant-scoped). Config via env / flag:

- `--now` — one-shot: runs sweep + maintenance + backup, then exits.
  Exit 1 if any job fails (REQ-OP-002).
- `--root <path>` — store root (default `~/.memento`).
- `--interval-hours <N>` — daemon period (default 24).

Daemon: `Ctrl-C` / `SIGTERM` shuts down **between runs** (in-flight jobs
always complete).

## Structured messages (--json mode)

Every error prints as:

```json
{
  "code": "AUTH_FAILED",
  "exit_code": 4,
  "message_es": "Falló la autenticación del tenant.",
  "message_en": "Tenant authentication failed.",
  "detail": "Token does not match credentials.toml"
}
```

`code` is stable (D7); `message_es`/`message_en` are chosen per
`--locale` or `MEMENTO_LOCALE`.
