# Quickstart — 5 minutos con Memento RS

> Asume que ya creaste un tenant y exportaste `MEMENTO_TOKEN` y
> `MEMENTO_AGENT_ID`. Si no, vuelve a [install.es.md](install.es.md).

## 1. Ingestar y buscar (30 s)

```bash
memento memory ingest-text \
  --text "Memento usa LanceDB embebido y fastembed para búsqueda híbrida local."

memento memory search --query "LanceDB" --top-k 5
```

Salida (es-ES):

```
{
  "hits": [
    {
      "chunk_id": "...",
      "score": 1.0,
      "text": "Memento usa LanceDB embebido...",
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

## 2. Activar búsqueda híbrida (RRF) (20 s)

```bash
# Habilita dense + sparse fusion para ESTE tenant (config.toml).
memento tenant retention --days 30   # (u otro valor; ver REQ-ML-003)
# (RRF se activa en la búsqueda con --rrf, o por defecto en próximas versiones.)
memento memory search --query "búsqueda híbrida" --rrf --top-k 5
```

> **Aviso:** con `--no-embeddings` la búsqueda híbrida devuelve
> `INVALID_INPUT` (REQ-MR-003): sin embeddings no hay vector denso.

## 3. context_fit (20 s)

`context_fit` elige los fragmentos que mejor caben en un presupuesto de
tokens. Útil cuando tu prompt tiene límite y no quieres overflow.

```bash
memento memory context-fit \
  --query "cómo funciona crypto-shredding" \
  --budget 800
```

Devuelve los chunks de mayor score que sumen ≤ 800 tokens, con un bonus
de hasta +0.5 por feedback positivo (cap explícito — REQ-MR-004).

## 4. Feedback (20 s)

```bash
CHUNK=$(memento memory search --query "LanceDB" --top-k 1 --json | jq -r '.hits[0].chunk_id')

memento memory feedback --chunk-id "$CHUNK" --score 1.0 --reason "útil"
memento memory feedback --chunk-id "$CHUNK" --score 0.0 --reason "no específico"
```

El feedback se persiste con atribución (REQ-ML-001) y mejora el orden
de los resultados siguientes (sin reentrenar).

## 5. Indexar código (60 s)

```bash
memento code index /ruta/a/mi/repo
memento code status
memento code symbol-lookup --symbol "ingest_text"
memento code dependencies
```

Para repos grandes (100k LOC), el primer index toma 10–30 s; ver
`docs/cli-reference.es.md` (sección `code`) para opciones.

## 6. Backup y borrado (20 s)

```bash
# Backup cifrado AES-256-GCM con clave envuelta por la maestra del tenant.
memento tenant backup
# Salida: ~/.memento/backups/<tid>/<ts>/{backup.enc, backup.key.json}

# Restaurar (operación offline: el store debe estar quieto).
memento tenant restore ~/.memento/backups/<tid>/<ts>

# Borrado total (purga + crypto-shredding de la clave maestra).
memento tenant delete --tenant
# Pide confirmación por stdin (escribe 'yes').
```

## 7. Localización

Por defecto, todo se imprime en español. Para inglés:

```bash
memento --locale en memory search --query "LanceDB"
# o:
MEMENTO_LOCALE=en memento health
```

## Siguiente paso

- [cli-reference.es.md](cli-reference.es.md) — todas las subcommands y
  flags.
- [mcp-clients.es.md](mcp-clients.es.md) — conectar Claude Code /
  Codex / OpenCode / Goose.
