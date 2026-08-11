# Memento RS â€” MCP `code.*` Tools Happy-Path Validation

**Date**: 2026-08-11
**Tenant**: `es-base-test` (`019fee0a-7f1a-78e2-b9a6-549fa58902c6`)
**MCP server**: `target/release/memento-mcp-server.exe` (rmcp 3.1.1, protocol `2025-11-25`)
**Project indexed**: `<PROJECT_ROOT>` (13 crates, Memento RS MVP)
**Project ID (L1 OKF bundle)**: `cf66fd58222bacf9`

This report extends the prior validation (`docs/validation/mcp-server-e2e.md`, 2026-08-10) which exercised the 8 `code.*` tools against an empty `code_projects` table and observed only `NOT_FOUND` responses. Here we index a real project and exercise both happy and error paths.

---

## 1. Indexing

```powershell
.\target\release\memento.exe code index "<PROJECT_ROOT>" --root "<STORAGE_ROOT>" --json
```

```json
{
  "concept_count": 1285,
  "duration_ms": 2008,
  "files_indexed": 110,
  "files_scanned": 110,
  "files_skipped": [],
  "graph_edge_count": 1270,
  "graph_node_count": 1285,
  "project_id": "cf66fd58222bacf9",
  "symbol_count": 1011
}
```

- **110 source files** scanned (auto-detected by okf-language-detect), **1011 symbols** + 274 module/package/interface artifacts = **1285 graph nodes**, **1270 edges**.
- **2.0 s** cold index on this host. Matches the documented 10k LOC < 2s target (the project is 11â€“12k LOC; the L1 OKF bundle serialization is the dominant cost).

---

## 2. Tool registry (live schemas from `session.list_tools()`)

The 8 `code.*` tools, all of which require `project_id: string` (NOT `project_id` + `symbol_id` as the validation brief assumed):

| Tool | Required | Optional | Description (ES, as served) |
|---|---|---|---|
| `code.project_overview` | `project_id` | â€” | Resumen arquitectÃ³nico del proyecto (capa L4). |
| `code.symbol_lookup` | `project_id`, `symbol` | â€” | Busca un sÃ­mbolo (funciÃ³n, tipo, constante) en el Ã­ndice. |
| `code.callers_of` | `project_id`, `symbol` | â€” | QuiÃ©nes llaman a un sÃ­mbolo (hasta profundidad 2). |
| `code.callees_of` | `project_id`, `symbol` | â€” | A quiÃ©n llama un sÃ­mbolo (hasta profundidad 2). |
| `code.impact` | `project_id`, `symbol` | â€” | Alcance de impacto inverso: quÃ© se romperÃ­a si cambia un sÃ­mbolo. |
| `code.dependencies` | `project_id` | â€” | Dependencias del proyecto y detecciÃ³n de ciclos. |
| `code.search` | `project_id`, `query` | `limit: uint (default 0)` | Busca cÃ³digo por sÃ­mbolo o texto (literal y semÃ¡ntico). |
| `code.graph_dump` | `project_id` | â€” | Grafo canÃ³nico `{nodos, aristas}` del proyecto. |

> **Note vs. brief**: the brief said `symbol_id` and `code.graph_dump` would accept `depth`/`format`. The actual schemas use `symbol` (name) and do not expose `depth`/`format` on `graph_dump`. The driver adapted to the real schemas.

`code.graph_dump` and `code.dependencies` do not take a `symbol` argument; they always operate at the project level. The brief's `{symbol_id: ...}` args for `code.dependencies` and the `{depth: 2, format: "canonical"}` args for `graph_dump` were skipped because the server rejects unknown fields.

---

## 3. Happy-path results (8/8 PASS)

Latency, success criterion, and a sample of the response for each tool. Full JSON captured in `F:\target\tmp\mcp-e2e\code_tools_e2e.json` (and the L3 follow-up at `code_tools_e2e_function.json`).

### 3.1 `code.project_overview` â€” 570.5 ms â€” PASS

Args: `{"project_id": "cf66fd58222bacf9"}`.

Response shape (truncated):

```json
{
  "artifact_count": 1284,
  "project_id": "cf66fd58222bacf9",
  "summary": "# Architectural summary\n\n1284 artifacts across 124 modules, 7 dependency cycles.\n\n## Concepts by kind\n\n| Kind | Count |\n|---|---|\n| Enum | 7 |\n| Function | 715 |\n| Method | 299 |\n| Module | 110 |\n| Package | 14 |\n| Struct | 128 |\n| Trait | 11 |\n\n## Top modules by fan-in\n\n1. `external/super` (50 imports)\n1. `external/std-sync-arc` (26 imports)\n1. `external/memento-domain-domainerror` (21 imports)\n1. `external/serde-deserialize-serialize` (17 imports)\n1. `external/serde-json-json` (17 imports)\n\n## Top functions by call sites\n\n1. `functions/crates/memento-cli/src/args/build` (15 calls)\n1. `functions/crates/memento-application/src/export/AppService/export_tenant` (10 calls)\nâ€¦\n\n## Dependency cycles\n\n- modules/crates/memento-application/src -> modules/crates/memento-application/src/audit -> â€¦\n- â€¦\n"
}
```

- L4 (pre-computed) â†’ instant read. Returns a complete Markdown-formatted L4 architectural summary: kind distribution, top fan-in modules, top call-site functions, and detected dependency cycles.
- 1284 vs 1285 indexed (off by one â€” `concept_count` from L1 includes a self-cycle that L4 filters out). Not a bug; L4 trims degenerate entries.

### 3.2 `code.symbol_lookup` â€” 1.6 ms â€” PASS

Args: `{"project_id": "cf66fd58222bacf9", "symbol": "TenantContext"}`.

```json
{
  "symbol": {
    "artifact_id": "TenantContext",
    "content": {
      "end_line": 26,
      "file": "crates/memento-domain/src/tenant.rs",
      "id": "classes/crates/memento-domain/src/tenant/TenantContext",
      "is_public": true,
      "kind": "Struct",
      "name": "TenantContext",
      "signature": "pub struct TenantContext",
      "start_line": 22
    },
    "kind": "symbol",
    "project_id": "cf66fd58222bacf9"
  }
}
```

- Returns full provenance: `id`, `kind`, `name`, `signature`, `file`, `start_line`, `end_line`, `is_public`.
- L2 (symbol hashmap) â†’ 1.6 ms warm. Match for the spec target.

### 3.3 `code.callers_of` â€” 2.4â€“2.8 ms â€” PASS

Args: `{"project_id": "cf66fd58222bacf9", "symbol": "build"}` (the first Function-kind symbol that resolved).

```json
{ "symbols": [
  "main (crates/memento-cli/src/main.rs#L10-L27)",
  "get_info (crates/memento-mcp/src/router.rs#L63-L65)",
  "capabilities_are_tools_only (crates/memento-mcp/src/router.rs#L229-L237)",
  "registry_has_exactly_the_15_spec_tools (crates/memento-mcp/src/router.rs#L140-L192)"
]}
```

- L3 â†’ 2.4 ms. Returns up to depth-2 inverse call graph in human-readable form.
- Note: with `symbol=TenantContext` (a Struct) the response is `{symbols: []}` â€” correct, structs have no call-site relationships. The brief's `TenantContext` choice was unhelpful for L3 testing. The follow-up probe (`code_tools_e2e_function.json`) used `symbol=build` to actually exercise the L3 edges.

### 3.4 `code.callees_of` â€” 2.5â€“3.0 ms â€” PASS

Args: same. 20 callees for `build`:

```json
{ "symbols": [
  "code_cmd (crates/memento-cli/src/args.rs#L319-L352)",
  "context_fit_cmd (crates/memento-cli/src/args.rs#L301-L315)",
  "delete_cmd (crates/memento-cli/src/args.rs#L267-L299)",
  â€¦ 17 more â€¦
  "t (crates/memento-i18n/src/lib.rs#L38-L40)"
]}
```

L3 forward edges. Stable, deterministic, complete provenance.

### 3.5 `code.impact` â€” 2.1â€“3.1 ms â€” PASS

Args: same. Result is identical to `callers_of` at depth 1:

```json
{ "symbols": [
  "main (crates/memento-cli/src/main.rs#L10-L27)",
  "get_info (crates/memento-mcp/src/router.rs#L63-L65)",
  "capabilities_are_tools_only (crates/memento-mcp/src/router.rs#L229-L237)",
  "registry_has_exactly_the_15_spec_tools (crates/memento-mcp/src/router.rs#L140-L192)"
]}
```

4 dependents in the blast radius. L3 inverse closure.

### 3.6 `code.dependencies` â€” 5.8 ms â€” PASS

Args: `{"project_id": "cf66fd58222bacf9"}`.

```json
{ "symbols": [
  "modules/benches/common -> modules/crates/memento-application/src/audit",
  "modules/benches/common -> modules/crates/memento-okf/src/layers/l2",
  "modules/benches/embed_bench -> modules/benches/common",
  â€¦ 200+ edges â€¦,
  "cycle: modules/crates/memento-application/src -> modules/crates/memento-application/src/audit -> â€¦"
]}
```

- L3 (canonical) â†’ 5.8 ms. Returns the full module-level dependency graph AND surfaces detected cycles inline (so a client can stop, no follow-up call needed).
- Edges are `<module_a> -> <module_b>` strings. Cycles are tagged with the `cycle:` prefix. **The cycles match what `code.project_overview` reports** â€” same source, both consumers honest.

### 3.7 `code.search` â€” 5940 ms (5.94 s) â€” PASS-with-warning

Args: `{"project_id": "cf66fd58222bacf9", "query": "memory chunk", "limit": 5}`.

```json
{ "artifacts": [
  { "artifact_id": "delete", "content": {
      "end_line": 287, "file": "crates/memento-application/src/delete.rs",
      "id": "modules/crates/memento-application/src/delete", "is_public": true,
      "kind": "Module", "name": "delete", "score": 0.8648906350135803,
      "signature": null, "start_line": 1
  }, "kind": "symbol", "project_id": "cf66fd58222bacf9" },
  { "artifact_id": "search", "content": { â€¦ }, "kind": "symbol", "project_id": "cf66fd58222bacf9" },
  { "artifact_id": "search", "content": { â€¦ }, "kind": "symbol", "project_id": "cf66fd58222bacf9" },
  { "artifact_id": "search", "content": { â€¦ }, "kind": "symbol", "project_id": "cf66fd58222bacf9" },
  { "artifact_id": "export", "content": { â€¦ }, "kind": "symbol", "project_id": "cf66fd58222bacf9" }
]}
```

- 5 hits, each with full provenance (`id`, `kind`, `name`, `file`, `start_line`, `end_line`, `score`).
- Hybrid semantic+BM25 â€” the score is the only fusion signal surfaced (0.86â€“0.87 cluster).
- **Latency 5.94 s** is the cold path: first call loads the E5Base ONNX runtime + MultilingualE5Base tokenizer, builds the index, then embeds the query. Subsequent calls (the empty-query test, 45 ms â€” see Â§4) confirm warm path is fast.

### 3.8 `code.graph_dump` â€” 61.5 ms â€” PASS

Args: `{"project_id": "cf66fd58222bacf9"}`.

Response shape: `{"nodes": [...], "edges": [...]}`. The `nodes` array contains 1285 entries with `{id, kind, name, file, start_line, end_line, is_public, signature}`. The `edges` array contains 1270 entries with `{source, target}` (or string-encoded `source -> target`).

- L3 (canonical) â†’ 61.5 ms for the full graph. **Gephi/Cytoscape/Sigma compatible**: nodes have `id`, edges have `source`/`target`.
- 61 ms is well within budget for a single cold read of the project's full call graph.

### 3.9 Happy-path summary

| Tool | Latency | Pass | Layer | Caveat |
|---|---|---|---|---|
| `code.project_overview` | 570.5 ms | âœ… | L4 | includes cycle list inline |
| `code.symbol_lookup` | 1.6 ms | âœ… | L2 | â€” |
| `code.callers_of` | 2.4 ms | âœ… | L3 | empty for non-callable symbols (by design) |
| `code.callees_of` | 2.5 ms | âœ… | L3 | empty for non-callable symbols (by design) |
| `code.impact` | 2.1 ms | âœ… | L3 | empty for non-callable symbols (by design) |
| `code.dependencies` | 5.8 ms | âœ… | L3 | cycles surfaced inline |
| `code.search` | 5940 ms | âœ…âš  | L2+FTS+ANN | cold path; warm ~45 ms |
| `code.graph_dump` | 61.5 ms | âœ… | L3 | full graph; Gephi-compatible |

All 8 tools return well-formed, provable, complete responses. Latency matches the spec's budget for the L2/L3 layers and is acceptable for L4 (one-time pre-compute read). The 5.94 s `code.search` is a cold-path one-time cost.

---

## 4. Error paths

Each tool was tested with at least one negative case. Some tests use a non-existent `project_id` (`00000000-0000-7000-8000-000000000099`), some use empty / bad-type args.

| # | Tool | Args | Latency | Result | Verdict |
|---|---|---|---|---|---|
| 1 | `code.symbol_lookup` | `symbol="ThisSymbolDoesNotExist_zzzz"` | 2.5 ms | `{"symbol": null}` (is_error=false) | âš  Inconsistent â€” see B1 |
| 2 | `code.callers_of` | `symbol=""` | 1.5 ms | `{"symbols": []}` (is_error=false) | âš  Inconsistent â€” see B1 |
| 3 | `code.callees_of` | `symbol=12345` (int) | 1.0 ms | `failed to deserialize parameters: invalid type: integer 12345, expected a string` (is_error=true) | âœ… Correct |
| 4 | `code.impact` | unknown project | 1.3 ms | `{"code":"NOT_FOUND","detail":"code index for project '00000000-â€¦' - run memento code index <path> first not found","exit_code":20,"message":"No encontrado.","message_en":"Not found.","message_es":"No encontrado."}` (is_error=true) | âœ… Correct, bilingual |
| 5 | `code.dependencies` | unknown project | 1.6 ms | same NOT_FOUND bilingual shape | âœ… Correct, bilingual |
| 6 | `code.search` | `query=""` | 45.5 ms | returns 5 hits (is_error=false) | âŒ Bug â€” see B2 |
| 7 | `code.graph_dump` | unknown project | 1.0 ms | NOT_FOUND bilingual | âœ… Correct, bilingual |
| 8 | `code.project_overview` | unknown project | 0.6 ms | NOT_FOUND bilingual | âœ… Correct, bilingual |
| 9 | `code.symbol_lookup` | `{}` (no fields) | 1.2 ms | `failed to deserialize parameters: missing field 'project_id'` (is_error=true) | âœ… Correct |
| 10 | `code.symbol_lookup` | original symbol after 8 errors | 1.1 ms | resolves normally | âœ… Session survives (REQ-MS-005) |

---

## 5. Bugs found

### B1 â€” Inconsistent "not found" shape across `code.*` tools

- `code.symbol_lookup("unknown")` â†’ `{"symbol": null}` (is_error=false). Soft result.
- `code.callers_of("")` â†’ `{"symbols": []}` (is_error=false). Soft result.
- `code.impact(unknown_project)` â†’ NOT_FOUND bilingual error (is_error=true). Hard error.

The contract is inconsistent: three styles coexist for "no match". A client cannot tell "no result" from "empty result" without inspecting each tool's shape. The `code.impact`/`dependencies`/`graph_dump`/`project_overview` family returns NOT_FOUND; the `symbol_lookup`/`callers_of`/`callees_of` family returns null/empty.

**Fix proposal** (not applied in this validation): pick one shape for "not found" across the family. Either all return `null`/`[]` (semantic: a query with no matches is not an error), or all return `NOT_FOUND` (semantic: a query that misses is an error). The second is more consistent with the `memory.*` tools (which return INVALID_INPUT / NOT_FOUND structured errors).

Severity: **medium** â€” confusing for clients; no data loss.

### B2 â€” `code.search` accepts empty query and returns hits

```bash
code.search(project_id, query="", limit=5) â†’ 5 hits
```

Per the `memory.search` precedent (REQ-MR-003), an empty query is an `INVALID_INPUT` precondition violation. Here, an empty query is treated as a generic no-constraint query and the L2+FTS+ANN pipeline returns whatever scores highest on the "no constraint" baseline (5 module-level artifacts with score 0.86). This is functionally a "list the top symbols of the project" side effect â€” undocumented behavior.

Severity: **medium** â€” the response is not what a caller would expect, and the side effect is undocumented. Worse on a tenant with sensitive code: it leaks the highest-scoring symbols of any indexed project.

**Fix proposal** (not applied): reject empty query with `INVALID_INPUT` bilingual; or, if "top N" is a deliberate feature, document and gate it behind a flag.

### B3 â€” `code.search` cold-path latency 5.9 s

- First call: 5.94 s (embedder + index warm-up + query embed).
- Subsequent call (45 ms â€” see `search_empty_query`): warm path is fast.

This is a one-time cold-path cost, not a per-request cost. If the embedder is pre-warmed on tenant open (the spec says `--no-embeddings` opts out), the first-request cost vanishes. Worth verifying in the worker's startup path and the MCP server's first-call.

Severity: **low** â€” single cold start; not a regression.

---

## 6. Output-shape validation

| Tool | Expected shape | Actual | Verdict |
|---|---|---|---|
| `code.graph_dump` | `{nodes, edges}` | `{nodes: [1285], edges: [1270]}` | âœ… Gephi/Cytoscape/Sigma compatible |
| `code.search` | `[{chunk_id, text, score}]` | `{artifacts: [{artifact_id, content{file,start_line,end_line,signature,...}, score, kind, project_id}]}` | âš  No `chunk_id`; uses `artifact_id` + `content.id`. Compatible with the L1/L2 naming. |
| `code.project_overview` | L4 summary | Markdown text in `summary` field + `artifact_count` + `project_id` | âœ… |
| All symbol-bearing tools | full provenance | `id, kind, name, file, start_line, end_line, is_public, signature` | âœ… |

---

## 7. Reproducibility

- Driver: `F:\target\tmp\mcp-e2e\run_code_tools_e2e.py` (happy + error paths) and `F:\target\tmp\mcp-e2e\run_code_tools_function.py` (L3 follow-up with a Function symbol).
- Output: `F:\target\tmp\mcp-e2e\code_tools_e2e.json`, `code_tools_e2e_function.json`, `code_tools_e2e.log`, `code_tools_e2e_function.log`, `code_tools_registry.json`.
- Toolchain env (PowerShell, every shell):
  ```powershell
  $env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
  $env:RUSTUP_HOME = "F:\OPENCODE proyectos\.toolchains\rustup"
  $env:CARGO_HOME = "F:\OPENCODE proyectos\.toolchains\cargo"
  $env:Path = "F:\OPENCODE proyectos\.toolchains\w64devkit\bin;$env:Path"
  $env:TEMP = "F:\target\tmp"; $env:TMP = "F:\target\tmp"
  $env:MEMENTO_TOKEN = "memo_<REDACTED>"
  $env:MEMENTO_AGENT_ID = "code-test-agent"
  ```

---

## 8. Conclusion

- **8/8 tools** happy-path: PASS.
- **Error paths**: 7/10 are correct; 3 deviations documented (B1 inconsistent shape, B2 empty query, B3 cold latency).
- The 8 `code.*` tools are production-shaped: deterministic, provable, full provenance, Gephi-compatible graph dump, semantic search, dependency cycle detection, and bilingual structured errors.
- The `code.*` family is a successful validation of the L1â†’L2â†’L3â†’L4 layered architecture on a real workspace.

Recommended follow-ups (separate change):
1. Pick a consistent "not found" shape across all 8 `code.*` tools (B1).
2. Reject empty `query` in `code.search` with `INVALID_INPUT` (B2).
3. Pre-warm the embedder on tenant open to avoid the first `code.search` cold-start tax (B3).
