<p align="center"><a href="README.en.md"><strong>English</strong></a> &nbsp;·&nbsp; <strong>Español</strong></p>

<p align="center">
  <img src="assets/memento-logo.png" alt="Memento RS" width="220" />
</p>

# Memento RS

> **La memoria persistente para agentes de IA. Un binario. Cero APIs externas. Cero egress.**

Memento RS es un motor de memoria multitenant escrito en Rust, pensado para equipos que ejecutan agentes de IA (Claude Code, Codex, OpenCode, Goose y agentes MCP custom) sobre la misma base de conocimiento. Indexa documentos reales, navega código fuente, y da a cada agente una memoria compartida con aislamiento por tenant — sin pagar la "ops tax" de los stacks Python o las suscripciones de vector stores gestionados.

**El SQLite de la memoria de agentes**: pequeño, embebible, predecible, estándar. No el Oracle.

---

## ¿Por qué Memento?

Los agentes de IA pierden contexto entre sesiones. La memoria fragmentada en chats, documentos y código no les sirve. Las alternativas son pesadas:

| | Memento RS | Stack Python típico (Mem0/Letta) | Vector store gestionado |
|---|---|---|---|
| **Despliegue** | 1 binario Rust | 5+ servicios + Postgres | SaaS, egress |
| **RAM** | ~700 MB warm (10k docs) | 2-3 GB+ | N/A |
| **Datos** | 100% locales | En tu infra | En el vendor |
| **Código** | Knowledge graph incluido | No | No |
| **Costo** | $0 licencias | $0, pero infra | Por token/MB |

**Validado en benchmarks reales** (no aspiracionales):

- Búsqueda híbrida: **p50 10 ms / p99 14 ms** sobre 132.5k chunks (objetivo: 20/100 ms)
- Indexación de código: **100k LOC en 15.4 s**
- Cold start del store: **75 ms**
- Ingest en estado estable: **14.4 ms/documento**
- Español-first: embeddings multilingües (E5-Base) con búsqueda semántica por defecto

---

## Lo que hace

### 🧠 Memoria compartida para agentes
Varios agentes sobre un mismo tenant ven la misma memoria; workspaces distintos quedan aislados. Cada resultado lleva proveniencia completa (fuente, documento, agente, versión del modelo, timestamp).

### 📄 Ingestión de documentos reales
PDF, DOCX, XLSX, Markdown, HTML y texto plano → Markdown limpio → chunking determinista (256-300 tokens, overlap) → embeddings + BM25. Chunking en español optimizado.

### 🔍 Búsqueda híbrida
BM25/FTS + embeddings densos fusionados con RRF. La búsqueda literal es excelente; la semántica captura paráfrasis y sinónimos (incluso ES↔EN con corpus en inglés).

### 💻 Code knowledge
Indexa repositorios Rust/Python y construye un grafo real: símbolos, llamadas, imports, ciclos de dependencia, resúmenes arquitectónicos. Navegá código como un humano senior: *"¿quién llama a esta función?"*, *"¿qué se rompe si cambio esto?"*.

### 🔐 Aislamiento + privacidad por diseño
El contexto del tenant se inyecta de forma forzada — buscar sin contexto es un error de compilación. Todo corre local: cero red, cero telemetría, cero tokens.

### 📜 Compliance GDPR lista para auditar
Borrado con derecho al olvido verificado (delete → compact → prune), export por tenant, retención configurable, respaldo cifrado AES-256-GCM, auditoría estructurada. **[Validado end-to-end](docs/validation/gdpr-right-to-erase-e2e.md)**.

### 🔌 Interfaz estándar para agentes
Servidor **MCP stdio** (15 tools: `memory.*` + `code.*`) + **CLI** bilingüe. Cualquier agente que hable MCP se conecta en minutos.

---

## Primeros pasos

```bash
# 1. Crear un tenant (te da el token de acceso)
memento tenant create --name "mi-equipo"

# 2. Ingestar un documento
memento ingest document --source 'document:pdf' mi_libro.pdf

# 3. Buscar
memento search "cómo escribir titulares magnéticos"
```

Conectá tu agente favorito vía MCP:

```json
// .mcp.json (Claude Code, Cursor, etc.)
{
  "mcpServers": {
    "memento": {
      "command": "memento-mcp-server",
      "env": { "MEMENTO_TOKEN": "<token>", "MEMENTO_AGENT_ID": "codex-agent" }
    }
  }
}
```

Más: [Instalación](docs/install.es.md) · [Tour de 5 minutos](docs/quickstart.es.md) · [Clientes MCP](docs/mcp-clients.es.md)

---

## Estado del proyecto

**MVP funcional y validado** — 14 rondas de validación sobre datos reales, todos los gates PASS:

- ✅ Smoke test + ingesta real de libros (PDF) + búsqueda semántica ES
- ✅ Benchmark de retrieval: 10 queries mixtas (literal EN / paráfrasis ES)
- ✅ Adversarial: cross-tenant, path traversal, PDF malformado, shell metachars, quota
- ✅ MCP server end-to-end (15 tools) + code tools sobre proyecto real
- ✅ GDPR right-to-erase + backup/restore + multi-agent same-tenant
- ✅ 339+ tests, `clippy -D warnings` limpio

**Lo que sigue (post-MVP):** auditoría de seguridad externa, pilotos de 2+ semanas, OCR para PDFs escaneados, perfiles de RAM reducida (modelo E5-Small opt-in), workers de larga duración.

## Arquitectura

Workspace Rust de 13 crates, hexagonal: dominio → ports → aplicación → adapters finos (MCP/CLI/worker). Almacenamiento: LanceDB embebido (vectores + FTS), OKF-RS para código, `anydoc` para documentos. Detalle en [docs/development.es.md](docs/development.es.md).

## Documentación

- [README en inglés](README.en.md)
- [Referencia CLI](docs/cli-reference.es.md) · [Configuración](docs/config-reference.es.md) · [Operaciones](docs/ops.es.md)
- [Seguridad: threat model](docs/security/threat-model.md) · [Checklist de auditoría](docs/security/audit-pre-shipped-checklist.md)
- [Validaciones](docs/validation/) · [Benchmarks](docs/perf/)

## Licencia

MIT — libre para uso comercial, sin atribución. Hecho para equipos que quieren control total de su memoria.
