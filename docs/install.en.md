# Install — Memento RS

## Requirements

- Rust 1.83+ (stable toolchain). The project pins `rust-toolchain.toml`
  so `cargo` uses the correct version automatically.
- 2 vCPU / 2 GB RAM / 20 GB SSD minimum recommended (see
  `manifesto memoria.md` for honest numbers).
- No runtime network dependencies: embedded LanceDB, local ONNX, local
  FTS.

## Build

```bash
git clone <repo-url> memento-rs
cd memento-rs
cargo build --release --workspace -j 2
```

Binaries land in `target/release/`:

- `memento` — CLI (`tenant`, `memory`, `code`, `health`, `stats`).
- `memento-mcp` — stdio MCP server.
- `memento-worker` — 24 h rotation (`--now` to run immediately).

## Create the first tenant

```bash
# Creates ~/.memento/ if missing, and a tenant named "default".
# Prints the token ONCE — store it; it is never shown again.
memento tenant create --name default
```

Expected output (en-US):

```
Tenant created: 7f3e...
Token: memo_7f3e_...
(This token is shown once.)
```

Store the token in your `~/.bashrc` or shell profile:

```bash
export MEMENTO_TOKEN=memo_7f3e_...
export MEMENTO_AGENT_ID=claude-code   # or your agent id
```

## First ingest

```bash
# Plain text
memento memory ingest-text --text "Memento is a local multitenant memory engine."

# Document (PDF, DOCX, XLSX, EPUB, … 14 formats via anydoc)
memento memory ingest-document --source ./rfc.pdf

# Bulk from a directory (per-file report)
memento memory bulk ./docs/ --source markdown
```

> **Heads-up:** the first ingest with `--no-embeddings` does NOT download
> the ONNX model (~500 MB). Without that flag, Memento downloads
> `MultilingualE5Small` to `~/.memento/models/` on the first embedding
> call.

## Health check

```bash
memento health
```

If the tenant and agent env vars are correct, returns `status: ok` with
a workspace summary. That output is the health probe used by
`docker-compose` (see `docker-compose.yml`).

## Next step

→ [quickstart.en.md](quickstart.en.md) for a guided tour of hybrid
search, `context_fit`, feedback and `code.index`.
