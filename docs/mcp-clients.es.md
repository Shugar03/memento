# Conectar clientes MCP — Memento RS

El servidor MCP se lanza como subproceso stdio. **Todos los clientes MCP
que soporten `stdio` funcionan** con la misma receta: binario + dos
variables de entorno.

## Variables de entorno (siempre requeridas)

| Variable            | Ejemplo                  | Notas                                          |
|---------------------|--------------------------|------------------------------------------------|
| `MEMENTO_TOKEN`     | `memo_7f3e_…`            | Token del tenant (`memento tenant create`).   |
| `MEMENTO_AGENT_ID`  | `claude-code`            | Id del agente que invoca (auditoría).         |
| `MEMENTO_ROOT`      | `/var/lib/memento`       | Opcional: raíz alternativa (default `~/.memento`). |
| `MEMENTO_LOCALE`    | `en`                     | Opcional: fuerza inglés en descripciones.      |

El binario es `memento-mcp` (compilado en `target/release/memento-mcp`).

---

## Claude Code

`.claude/settings.json`:

```json
{
  "mcpServers": {
    "memento": {
      "command": "/ruta/absoluta/a/memento-mcp",
      "env": {
        "MEMENTO_TOKEN": "memo_7f3e_...",
        "MEMENTO_AGENT_ID": "claude-code"
      }
    }
  }
}
```

Las herramientas aparecen como `mcp__memento__memory_search`,
`mcp__memento__memory_ingest_text`, etc.

## Codex (CLI de OpenAI)

`~/.codex/config.toml`:

```toml
[mcp_servers.memento]
command = "/ruta/absoluta/a/memento-mcp"

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
      "command": "/ruta/absoluta/a/memento-mcp",
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
    cmd: /ruta/absoluta/a/memento-mcp
    args: []
    envs:
      MEMENTO_TOKEN: "memo_7f3e_..."
      MEMENTO_AGENT_ID: "goose"
    timeout: 30000
```

## Herramientas expuestas

El servidor publica 15 herramientas (REQ-MS-002):

- **memoria** (7): `search`, `ingest_text`, `ingest_document`,
  `get_chunk`, `feedback`, `delete`, `context_fit`.
- **código** (8): `project_overview`, `symbol_lookup`, `callers_of`,
  `callees_of`, `impact`, `dependencies`, `search`, `graph_dump`.

Las descripciones se sirven en español o inglés según
`MEMENTO_LOCALE` o `--locale` del CLI.

## Solución de problemas

- **`AUTH_FAILED` al iniciar** → `MEMENTO_TOKEN` no coincide con el
  hash en `~/.memento/db/tenants/<tid>/auth/credentials.toml`. Revisa
  que copiaste el token completo.
- **`is_error: true` con `INVALID_INPUT`** → falta `MEMENTO_AGENT_ID`
  (REQ-MS-003).
- **El servidor se cierra al primer error** → no debería: las sesiones
  sobreviven a frames malformados (T-070). Si lo ves, abre un issue
  con el log `~/.memento/logs/<tid>.jsonl`.
- **`@firecrawl/anydoc` no resuelto** → `memento-mcp` falla al
  arrancar: `ParseService::auto` no encuentra Node. Instala Node 20+
  para habilitar 14 formatos. El CLI degrada limpiamente sin Node; el
  servidor MCP, no (porque no podría servir documentos en absoluto).
