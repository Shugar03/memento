# Memento RS

Motor de memoria multitenent para agentes de IA. Un binario, una raíz
(`~/.memento/`), cero APIs externas.

- **Latencia:** p50 &lt; 20 ms / p99 &lt; 100 ms en búsqueda, 100 k chunks
  (REQ-MR-007).
- **Privacidad:** datos 100 % locales. Cero red, cero tokens.
- **Aislamiento:** el contexto del tenant se inyecta forzado — buscar sin
  contexto es un error de compilación.
- **Resiliencia:** respaldo cifrado, borrado GDPR por crypto-shredding,
  auditoría inalterable.

## Instalación y primer arranque

→ [docs/install.es.md](docs/install.es.md)

## Tour de 5 minutos

→ [docs/quickstart.es.md](docs/quickstart.es.md)

## Conectar clientes MCP (Claude Code, Codex, OpenCode, Goose)

→ [docs/mcp-clients.es.md](docs/mcp-clients.en.md)

## Referencia CLI

→ [docs/cli-reference.es.md](docs/cli-reference.es.md)

## Runbook de operaciones

→ [docs/ops.es.md](docs/ops.es.md) — backup/restore, retención, auditoría,
borrado GDPR, drill E2E.

→ [docs/config-reference.es.md](docs/config-reference.es.md) — todos los
knobs por tenant, env vars, flags CLI y constantes.

## Documentación en inglés

→ [README.en.md](README.en.md)
