# Connecting MCP clients — Memento RS

The MCP server runs as a stdio subprocess. **Any MCP client that supports
`stdio` works** with the same recipe: binary + two environment variables.

## Environment variables (always required)

| Variable           | Example                 | Notes                                          |
|--------------------|-------------------------|------------------------------------------------|
| `MEMENTO_TOKEN`    | `memo_7f3e_…`           | Tenant token (`memento tenant create`).       |
| `MEMENTO_AGENT_ID` | `claude-code`           | Invoking agent id (audit attribution).        |
| `MEMENTO_ROOT`     | `/var/lib/memento`      | Optional: alternate root (default `~/.memento`). |
| `MEMENTO_LOCALE`   | `en`                    | Optional: force English descriptions.          |

The binary is `memento-mcp` (built to `target/release/memento-mcp`).

---

## Claude Code

`.claude/settings.json`:

```json
{
  "mcpServers": {
    "memento": {
      "command": "/absolute/path/to/memento-mcp",
      "env": {
        "MEMENTO_TOKEN": "memo_7f3e_...",
        "MEMENTO_AGENT_ID": "claude-code"
      }
    }
  }
}
```

The tools surface as `mcp__memento__memory_search`,
`mcp__memento__memory_ingest_text`, etc.

## Codex (OpenAI CLI)

`~/.codex/config.toml`:

```toml
[mcp_servers.memento]
command = "/absolute/path/to/memento-mcp"

[mcp_servers.memento.env]
MEMENTO_TOKEN = "memo_7f3e_..."
MEMENTO_AGENT_ID = "codex"
```

## OpenCode

`~/.config/opencode/opencode.json`:

```json
{
  "mcp": {
    "memento": {
      "command": "/absolute/path/to/memento-mcp",
      "environment": {
        "MEMENTO_TOKEN": "memo_7f3e_...",
        "MEMENTO_AGENT_ID": "opencode"
      }
    }
  }
}
```

## Goose (Block)

`~/.config/goose/config.yaml`:

```yaml
extensions:
  memento:
    type: stdio
    cmd: /absolute/path/to/memento-mcp
    args: []
    envs:
      MEMENTO_TOKEN: "memo_7f3e_..."
      MEMENTO_AGENT_ID: "goose"
    timeout: 30000
```

## Exposed tools

The server publishes 15 tools (REQ-MS-002):

- **memory** (7): `search`, `ingest_text`, `ingest_document`,
  `get_chunk`, `feedback`, `delete`, `context_fit`.
- **code** (8): `project_overview`, `symbol_lookup`, `callers_of`,
  `callees_of`, `impact`, `dependencies`, `search`, `graph_dump`.

Descriptions are served in Spanish or English per `MEMENTO_LOCALE` or
the CLI `--locale` flag.

## Troubleshooting

- **`AUTH_FAILED` on startup** → `MEMENTO_TOKEN` does not match the
  hash in `~/.memento/db/tenants/<tid>/auth/credentials.toml`. Verify
  you copied the full token.
- **`is_error: true` with `INVALID_INPUT`** → missing
  `MEMENTO_AGENT_ID` (REQ-MS-003).
- **Server exits on first error** → should not happen: sessions
  survive malformed frames (T-070). If you see this, open an issue
  with the log at `~/.memento/logs/<tid>.jsonl`.
- **`@firecrawl/anydoc` not resolvable** → `memento-mcp` fails to
  start: `ParseService::auto` cannot find Node. Install Node 20+ to
  enable the 14 document formats. The CLI degrades cleanly without
  Node; the MCP server does not (it cannot serve documents at all).
