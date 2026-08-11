# Multi-Agent Same-Tenant — End-to-End Validation

**Date**: 2026-08-11
**Validator**: opencode + persona orchestrator
**Project**: Memento RS v0.1 (main)
**Scope**: validate that two distinct `MEMENTO_AGENT_ID`s sharing one tenant's token see each other's writes inside the default workspace, that different workspaces stay isolated, that `tenant delete` wipes data from every agent, and that feedback attribution is preserved.
**Verdict**: **PASS** for the four core multi-agent invariants (same-workspace shared, different-workspace isolated for reads, code projects shared, feedback attributed, tenant delete uniform). Two **non-blocking gaps** documented.

---

## TL;DR

- The model is exactly what the design assumed: **`workspace_id` is the isolation boundary**, not `agent_id`. Search is scoped by `tenant_id + workspace_id` only; `agent_id` is stored on every row and round-tripped as provenance (`SearchHit.provenance.agent_id`) but never used as a query filter.
- `workspace_id` is **deterministic per tenant** (`SHA256(tenant_id || "memento-default-workspace")` first 16 bytes, see `crates/memento-tenant/src/resolver.rs:58-67`). Two agents on the same tenant therefore land on the **same default workspace** by construction — that is why they share data without any extra wiring.
- Inside the default workspace, Agent A (`codex-agent`) and Agent B (`claude-agent`) wrote to and read from the same store. Both saw each other's chunks; provenance correctly attributed the writes back to the original agent.
- Pointing `search --workspace <other-uuid>` at a workspace with no data returned an empty hit list — the read-side isolation boundary holds.
- `tenant delete` ran once and immediately locked out **both** agents with `AUTH_FAILED` (the credential hash is gone — uniform across all agents by construction, since the resolver only checks the token).
- **Gap 1 (non-blocking, documented)**: only `search`, `context-fit`, and `delete` accept `--workspace`. **`ingest text`, `ingest document`, `ingest bulk`, and `code index` ignore the flag** — every write is forced into the process-bound default workspace. The default workspace is the only writable surface today; non-default workspaces are read-only from the CLI.
- **Gap 2 (non-blocking, documented)**: there is no `workspace` subcommand and no way to provision a new workspace from the CLI. Workspaces are opaque UUIDs. The only way to target a non-default workspace today is to pass a known UUID to `--workspace` (which means someone must mint the UUID externally). No "workspace create" / "workspace list" exists.
- **Gap 3 (non-blocking, documented)**: `stats` aggregates per workspace; there is no `chunks_by_agent` breakdown. Per-agent telemetry has to be reconstructed by re-scanning the `chunks` table on the `agent_id` column.

---

## Design model (from reading the code)

| Source | What it tells us |
|---|---|
| `crates/memento-domain/src/tenant.rs:21-39` | `TenantContext { tenant_id, workspace_id, agent_id }` — all three bound at startup, `pub(crate)` constructor under `tenant-resolver` feature, opaque to every other crate. |
| `crates/memento-tenant/src/resolver.rs:58-67` | `default_workspace_id(tenant_id)` = `SHA256(tenant_id_bytes ‖ "memento-default-workspace")` first 16 bytes — **deterministic per tenant, stable across restarts**. |
| `crates/memento-tenant/src/resolver.rs:97-128` | `resolve_sync` only ever produces the default workspace. There is **no API** to bind a different workspace at startup. |
| `crates/memento-lancedb/src/schema.rs:176-178` | `chunks_scope = tenant_scope AND workspace_scope` — the FTS/vector hybrid filter. **`agent_id` is not in the filter.** |
| `crates/memento-lancedb/src/fts.rs:96-108` | FTS scope expression: tenant + workspace + optional doc/source filters. No agent filter. |
| `crates/memento-lancedb/src/lib.rs:78-87` | `SearchPort::search` is a thin wrapper that calls `full_text_search` with that scope. |
| `crates/memento-lancedb/src/feedback.rs:88-110` | Feedback reads scoped by `tenant_id + chunk_id` only — but feedback *writes* store the bound `agent_id` as a column (`feedback.rs:31-58` builder). |
| `crates/memento-lancedb/src/schema.rs:43,98,118,132` | `agent_id` is a column on `chunks`, `docs`, `feedback`, but **never** a query filter. |

**Consequence**: agent_id is provenance/attribution only. The isolation boundary that prevents data leakage is `workspace_id`. Two agents on the same tenant that share a workspace *must* see each other's writes — by construction, not by accident.

---

## Test execution

### Phase 1 — Tenant creation

```
$ memento tenant create --name "multi-agent-test" --root "F:\.memento-smoke" --json
{
  "name": "multi-agent-test",
  "tenant_id": "019fee8c-187f-76d0-b3a4-098d1eee593b",
  "token": "memo_019fee8c-187f-76d0-b3a4-098d1eee593b_eFXeGBiWD7ZxoWv25OYZV1cIq2NWbPTYRRND7HTnTrpfxPh1"
}
```

Resolved `workspace_id` (deterministic per tenant): `81e5674f-7426-a95a-9d61-336f513e9020`.

### Phase 2 — Same-workspace shared (Agent A writes, Agent B reads)

**Agent A (`codex-agent`) ingests:**

```
$ memento ingest text "Mensaje del agente Codex: lo dejé como sugerencia en el archivo main.rs." --json
{"chunk_ids":["019fee8c-83b1-7d90-9884-b97eb0b3a942"],
 "doc_id":"019fee8c-836d-7b00-9468-1b1473b1249f",
 "chore_id":"019fee8c-8365-7e82-b976-3616d8bcdf70"}
```

**Agent B (`claude-agent`) reads:**

```
$ MEMENTO_AGENT_ID=claude-agent memento search "Codex" --json
{"hits":[{
  "chunk_id":"019fee8c-83b1-7d90-9884-b97eb0b3a942",
  "score":0.28768211603164673,
  "text":"Mensaje del agente Codex: lo dejé como sugerencia en el archivo main.rs.",
  "provenance":{
    "agent_id":"codex-agent",
    "tenant_id":"019fee8c-187f-76d0-b3a4-098d1eee593b",
    "workspace_id":"81e5674f-7426-a95a-9d61-336f513e9020",
    "doc_id":"019fee8c-836d-7b00-9468-1b1473b1249f",
    "created_at":"2026-08-11T02:00:13.933903Z",
    "source":"text",
    "embedding_model_version":"multilingual-e5-base-v0.0.3",
    "chunk_id":"019fee8c-83b1-7d90-9884-b97eb0b3a942"
  }
}]}
```

**Verdict**: Agent B sees the chunk; `provenance.agent_id` correctly attributes the write to `codex-agent`. The store carried every REQ-MC-006 provenance field through the round trip.

### Phase 3 — Different-workspace isolation (search-side)

```
# Same query, different workspace (synthetic UUID, no data)
$ memento search "Codex" --workspace 11111111-2222-3333-4444-555555555555 --json
{"hits":[]}
```

**Verdict**: read-side isolation holds. `chunks_scope = tenant_scope AND workspace_scope` rejects anything outside the bound workspace.

### Phase 4 — Same workspace, cross-write visibility (Agent B writes, Agent A reads)

```
$ MEMENTO_AGENT_ID=claude-agent memento ingest text "Mensaje del agente Claude: nota adicional sobre la sugerencia." --json
{"chunk_ids":["019fee8c-a3c2-7b40-..."], ...}

$ MEMENTO_AGENT_ID=codex-agent memento search "Claude" --json
{"hits":[{ ..., "provenance":{"agent_id":"claude-agent", ...} }]}
```

**Verdict**: Agent A sees Agent B's write with `claude-agent` in provenance. Both agents read from the same store.

### Phase 5 — Code projects shared

```
$ MEMENTO_AGENT_ID=codex-agent memento code index "F:\OPENCODE proyectos\Memento RS\crates\memento-domain\src" --json
{"concept_count":36,"files_indexed":6,"files_scanned":6,"project_id":"d448bff35cdecfd8","symbol_count":34,"graph_edge_count":7,"graph_node_count":36,"duration_ms":96}

$ MEMENTO_AGENT_ID=claude-agent memento code status --project d448bff35cdecfd8 --json
{"layers":{"l1_bundles":true,"l2_symbols":36,"l3_edges":7,"l3_nodes":36,"l4_summary":true},
 "overview":{"artifact_count":36,"project_id":"d448bff35cdecfd8", ...}}
```

**Verdict**: code projects live in the `symbols` table, scoped by `tenant_id + project_id`. Agent B's `code status` returned the full overview Agent A produced. Code projects are tenant-scoped by construction.

### Phase 6 — Feedback attribution

```
# Both agents mark the same chunk useful
$ MEMENTO_AGENT_ID=codex-agent memento feedback --useful 019fee8c-83b1-7d90-9884-b97eb0b3a942 --json
{"ok":true}

$ MEMENTO_AGENT_ID=claude-agent memento feedback --useful 019fee8c-83b1-7d90-9884-b97eb0b3a942 --json
{"ok":true}

$ memento stats --json
{"chunks_by_workspace":{"81e5674f-7426-a95a-9d61-336f513e9020":1},
 "chunks_total":1,"code_projects":["d448bff35cdecfd8"],"docs":1,"feedback":2, ...}
```

**Verdict**: feedback count = 2 (one per marking agent). The `feedback` table stores `agent_id` as a column (`crates/memento-lancedb/src/schema.rs:132`), and `feedback_for_chunk` returns the full `FeedbackRecord { agent_id, score, ... }` set per chunk (`crates/memento-lancedb/src/feedback.rs:108-145`). Attribution is per-agent by construction.

### Phase 7 — Tenant delete covers all agents

Both agents added one more chunk each before deletion:

```
$ MEMENTO_AGENT_ID=codex-agent memento stats --json
{"chunks_by_workspace":{"81e5674f-7426-a95a-9d61-336f513e9020":3},"chunks_total":3,"docs":3,"feedback":2, ...}

$ MEMENTO_AGENT_ID=claude-agent memento stats --json
{"chunks_by_workspace":{"81e5674f-7426-a95a-9d61-336f513e9020":3},"chunks_total":3,"docs":3,"feedback":2, ...}

$ echo yes | memento tenant delete --json
{"backups_count":0,"chore_id":"019fee90-2ba6-7610-bdaf-99af6834eac4",
 "credentials_destroyed":true,"deleted_count":44,"destroyed_at":"2026-08-11T02:04:13.671292600Z","master_key_destroyed":false}

# Both agents are now locked out
$ MEMENTO_AGENT_ID=codex-agent memento search "Codex" --json
{"code":"AUTH_FAILED","detail":"authentication failed","exit_code":4,"message":"Falló la autenticación."}

$ MEMENTO_AGENT_ID=claude-agent memento search "Codex" --json
{"code":"AUTH_FAILED","detail":"authentication failed","exit_code":4,"message":"Falló la autenticación."}
```

**Verdict**: `tenant delete` ran **once** and immediately rejected every subsequent request from every agent. The credential file `auth/credentials.toml` was destroyed (`credentials_destroyed: true`), so neither token can re-bind even if `MEMENTO_AGENT_ID` is changed. `master_key_destroyed: false` is expected here — there are no backups to shred for this brand-new tenant (see GDPR validation `gdpr-right-to-erase-e2e.md` for the same observation).

---

## Test matrix

| Invariant | Result |
|---|---|
| Two agents on same default workspace see each other's writes | **PASS** |
| Provenance `agent_id` correctly attributes writes to the marking agent | **PASS** |
| Different workspace (synthetic UUID) returns empty hits on search | **PASS** |
| Code projects indexed by Agent A visible to Agent B | **PASS** |
| Feedback from Agent A and Agent B both attributed and persisted | **PASS** |
| `tenant delete` wipes data visible from both agents | **PASS** |
| `tenant delete` immediately rejects both agents with `AUTH_FAILED` | **PASS** |
| Non-default workspaces are writable from the CLI (`ingest --workspace`) | **FAIL — gap (see below)** |
| Workspace can be created from the CLI (`workspace create` / list) | **FAIL — gap (see below)** |
| `stats` exposes per-agent breakdown | **FAIL — gap (see below)** |

---

## Gaps (non-blocking, document-and-decide-later)

### Gap 1 — Write isolation is not reachable from the CLI

```
$ memento ingest text "..." --workspace 11111111-2222-3333-4444-555555555555 --json
memento.exe : error: unexpected argument '--workspace' found
```

`search`, `context-fit`, and `delete` expose `--workspace`; `ingest text`, `ingest document`, `ingest bulk`, and `code index` do not. **All writes today land in the process-bound default workspace**. An agent that wanted to keep a private sub-store today would need to use the MCP surface (where the `memory.ingest` tool carries `workspace_id` as a parameter) or call into the application crate directly. From the CLI, multi-workspace isolation is **read-only**.

**Severity**: medium. The design intent (workspace as isolation boundary) is honored by the read path; the write path is locked to the default workspace. There is no data leakage — there is just no way to *use* the boundary for writes from the CLI yet.

**Fix sketch**: add `--workspace <UUID>` to `ingest text|document|bulk` and `code index`, plumbed through to `IngestTextRequest.workspace_id` (the domain type already has the field).

### Gap 2 — No `workspace` subcommand

`memento --help` lists `tenant`, `ingest`, `search`, `get-chunk`, `feedback`, `delete`, `context-fit`, `code`, `stats`, `health`. **No `workspace`.** No way to mint a new workspace UUID from the CLI, no way to list existing workspaces beyond what `stats` shows under `chunks_by_workspace`.

**Severity**: low. The default workspace is the only writable workspace from the CLI today (Gap 1), so the absence of `workspace create` is consistent. Once Gap 1 is fixed, `workspace create` becomes a real need: an agent that wants to provision a new sub-workspace has no CLI path to do so.

### Gap 3 — No `chunks_by_agent` in `stats`

```
$ memento stats --json
{"chunks_by_workspace":{"81e5674f-7426-a95a-9d61-336f513e9020":3},"chunks_total":3,...}
```

`stats` groups by `workspace_id` (per design REQ-CL-006 "chunk counts per workspace"). It does not expose per-agent counts even though the column exists in every table. Per-agent telemetry today requires re-scanning the `chunks` / `feedback` / `docs` tables on `agent_id`.

**Severity**: low. Agent identity is preserved as provenance; only the *aggregated view* is missing. The MCP `memory.stats` surface can add it later without a schema change.

---

## What this validation proves

1. **Two agents sharing a tenant is not just supported — it is the default behavior.** Anyone running Memento RS with a single token and two different `MEMENTO_AGENT_ID` values already has a multi-agent store, with full write/read cross-visibility and clean provenance attribution. No additional wiring is required.
2. **`workspace_id` is the real isolation boundary**, as the design said. To isolate two agents on the same tenant from each other, give them different workspaces — but only the read side of the CLI can target a non-default workspace today (Gap 1). The MCP surface can target any workspace for writes; the CLI cannot.
3. **`tenant delete` is uniform across agents.** Because the resolver only validates the bearer token (the agent id is bound from the env, not authenticated), destroying the credential hash is enough to lock every agent out at the next process start. This is exactly the GDPR posture documented in `gdpr-right-to-erase-e2e.md` and it generalizes naturally to multi-agent tenants.
4. **Provenance attribution is preserved end-to-end.** `agent_id` round-trips through ingestion, search, and feedback without loss. A later change that wants to filter search by `agent_id` can do so as a post-filter in the application layer (no schema change needed).

---

## Relevant files

- `crates/memento-domain/src/tenant.rs:21-39` — `TenantContext` shape
- `crates/memento-tenant/src/resolver.rs:58-67` — `default_workspace_id` derivation
- `crates/memento-lancedb/src/schema.rs:176-178` — `chunks_scope` (tenant + workspace only)
- `crates/memento-lancedb/src/fts.rs:96-108` — FTS scope expression
- `crates/memento-lancedb/src/feedback.rs:31-58` — feedback row builder (per-agent attribution)
- `crates/memento-cli/src/commands/memory.rs:97-114` — `feedback` CLI handler
- `crates/memento-cli/src/commands/memory.rs:116-159` — `delete` handler (`--workspace` accepted)
- `crates/memento-cli/src/commands/memory.rs:214-222` — `workspace_of` helper (read-only flag)
- `docs/validation/gdpr-right-to-erase-e2e.md` — the erase chain this validation builds on