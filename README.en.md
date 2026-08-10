# Memento RS

Multitenant memory engine for AI agents. One binary, one root
(`~/.memento/`), zero external APIs.

- **Latency:** p50 &lt; 20 ms / p99 &lt; 100 ms search at 100k chunks
  (REQ-MR-007).
- **Privacy:** 100% local data. Zero network, zero token cost.
- **Isolation:** the tenant context is force-injected — searching without
  context is a compile-time error.
- **Resilience:** encrypted backups, GDPR erase by crypto-shredding,
  tamper-evident audit log.

## Install and first run

→ [docs/install.en.md](docs/install.en.md)

## 5-minute tour

→ [docs/quickstart.en.md](docs/quickstart.en.md)

## Connect MCP clients (Claude Code, Codex, OpenCode, Goose)

→ [docs/mcp-clients.en.md](docs/mcp-clients.en.md)

## CLI reference

→ [docs/cli-reference.en.md](docs/cli-reference.en.md)

## Spanish documentation

→ [README.es.md](README.es.md)
