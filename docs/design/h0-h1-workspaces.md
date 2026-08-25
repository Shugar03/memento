# H0 + H1 — contracts and workspaces

## H0 — no silent degradation + provenance

| Contract | Surface | Gate |
|----------|---------|------|
| Hybrid (`--rrf` / `rrf_enabled`) without embeddings → `INVALID_INPUT` | application + CLI (`h0_h1_workspaces`) + existing unit test | REQ-MR-003 |
| Search hits carry full provenance; `agent_id` matches bound agent | CLI JSON | REQ-MC-006 / REQ-CL-003 |

Rerank on FTS-only remains a **warn + keep order** (intentional soft degrade); not changed in H0.

## H1 — workspaces (REQ-WS-*)

| REQ | Behavior |
|-----|----------|
| REQ-WS-001 | `memento workspace create [--name]` mints a UUID and appends to `workspaces.jsonl` |
| REQ-WS-002 | `memento workspace list` always includes the deterministic default + registered |
| REQ-WS-003 | `ingest text\|document\|bulk --workspace <uuid>` and MCP optional `workspace_id` stamp writes |

Isolation boundary remains `tenant_id + workspace_id`. `agent_id` is provenance only.

Policy: **same tenant = shared by default**; isolation = workspace.
