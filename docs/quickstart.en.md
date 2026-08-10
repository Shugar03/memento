# Quickstart — 5 minutes with Memento RS

> Assumes you already created a tenant and exported `MEMENTO_TOKEN` and
> `MEMENTO_AGENT_ID`. If not, go back to [install.en.md](install.en.md).

## 1. Ingest and search (30 s)

```bash
memento memory ingest-text \
  --text "Memento uses embedded LanceDB and fastembed for local hybrid search."

memento memory search --query "LanceDB" --top-k 5
```

Output (en-US):

```
{
  "hits": [
    {
      "chunk_id": "...",
      "score": 1.0,
      "text": "Memento uses embedded LanceDB...",
      "provenance": {
        "source": "text",
        "doc_id": null,
        "workspace_id": "default",
        "agent_id": "claude-code",
        "tenant_id": "7f3e...",
        "created_at": "2026-08-10T..."
      }
    }
  ]
}
```

## 2. Hybrid search (RRF) (20 s)

```bash
# Enable dense + sparse fusion for THIS tenant (config.toml).
memento tenant retention --days 30   # (or any value; see REQ-ML-003)
# (RRF is opted-in per query with --rrf, on by default in future releases.)
memento memory search --query "hybrid search" --rrf --top-k 5
```

> **Heads-up:** with `--no-embeddings`, hybrid search returns
> `INVALID_INPUT` (REQ-MR-003): without embeddings there is no dense
> vector.

## 3. context_fit (20 s)

`context_fit` picks the chunks that best fit a token budget. Useful when
your prompt has a limit and you do not want overflow.

```bash
memento memory context-fit \
  --query "how does crypto-shredding work" \
  --budget 800
```

Returns the highest-scoring chunks summing to ≤ 800 tokens, with a bonus
of up to +0.5 from positive feedback (explicit cap — REQ-MR-004).

## 4. Feedback (20 s)

```bash
CHUNK=$(memento memory search --query "LanceDB" --top-k 1 --json | jq -r '.hits[0].chunk_id')

memento memory feedback --chunk-id "$CHUNK" --score 1.0 --reason "useful"
memento memory feedback --chunk-id "$CHUNK" --score 0.0 --reason "not specific"
```

Feedback is persisted with attribution (REQ-ML-001) and improves the
order of subsequent results (without retraining).

## 5. Index code (60 s)

```bash
memento code index /path/to/my/repo
memento code status
memento code symbol-lookup --symbol "ingest_text"
memento code dependencies
```

For large repos (100k LOC), the first index takes 10–30 s; see
`docs/cli-reference.en.md` (`code` section) for options.

## 6. Backup and erase (20 s)

```bash
# Encrypted backup (AES-256-GCM, key wrapped by tenant master key).
memento tenant backup
# Output: ~/.memento/backups/<tid>/<ts>/{backup.enc, backup.key.json}

# Restore (offline op: store must be quiesced).
memento tenant restore ~/.memento/backups/<tid>/<ts>

# Full erase (purge + crypto-shredding of master key).
memento tenant delete --tenant
# Requires stdin confirmation (type 'yes').
```

## 7. Locale

By default everything prints in Spanish. For English:

```bash
memento --locale en memory search --query "LanceDB"
# or:
MEMENTO_LOCALE=en memento health
```

## Next step

- [cli-reference.en.md](cli-reference.en.md) — every subcommand and flag.
- [mcp-clients.en.md](mcp-clients.en.md) — connect Claude Code / Codex
  / OpenCode / Goose.
