# Threat model — Memento RS MVP

> Status: **v0.1 — pre-audit draft** (batch 11 / T-107). The structure
> follows the [STRIDE](https://learn.microsoft.com/en-us/azure/security/develop/threat-modeling-stride-7) taxonomy; each
> category is mapped to the actual Memento RS architecture (hexagonal
> core in `crates/memento-domain`, ports in `crates/memento-ports`,
> adapters under `crates/memento-{lancedb,embed-fastembed,parse,okf,tenant}`,
> application use cases in `crates/memento-application`, surfaces in
> `crates/memento-{mcp,cli,worker}`).

## 1. System description

Memento RS is a **local-first multitenant memory engine** for AI agents.
Every byte lives under `<root>/db/tenants/<tid>/` on the host's disk.
There is no HTTP server, no telemetry, no third-party API at runtime.

```
┌────────────────────────────────────────────────────────────────┐
│  Agents (Claude Code, Codex, OpenCode, Goose)                  │
│       │   stdio JSON-RPC  (rmcp)                               │
│       ▼                                                        │
│  ┌──────────────┐    ┌──────────────┐    ┌──────────────────┐  │
│  │ memento-mcp  │    │ memento CLI  │    │ memento-worker   │  │
│  │ (process)    │    │ (one-shot)   │    │ (daemon, 24 h)   │  │
│  └──────┬───────┘    └──────┬───────┘    └────────┬─────────�  │
│         │                   │                     │            │
│         ▼                   ▼                     ▼            │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │ memento-application (use cases + TenantContext guard)    │  │
│  └──────┬───────────────┬───────────────�───────────┬────────┘  │
│         │               │               │           │           │
│         ▼               ▼               ▼           ▼           │
│  ┌──────────┐   ┌─────────────┐  ┌──────────┐  ┌──────────┐    │
│  │ lancedb  │   │ fastembed   │  │  parse   │  │  okf     │    │
│  │ adapter  │   │ (ONNX)      │  │ (anydoc) │  │ (tree-   │    │
│  │          │   │             │  │          │  │  sitter) │    │
│  └────┬─────┘   └──────┬──────┘  └────┬─────┘  └────┬─────┘    │
│       │                │              │             │           │
│       ▼                ▼              ▼             ▼           │
│  per-tenant Lance tables   model cache   staged   indexed       │
│                            ~/.memento/   tmp dir   bundles       │
│                            models/                              │
└────────────────────────────────────────────────────────────────┘
```

Trust boundaries (numbered for cross-references):

| TB | Boundary | What crosses it |
|----|----------|-----------------|
| TB-1 | Agent process ↔ `memento-mcp` | JSON-RPC frames on stdin/stdout (rmcp). |
| TB-2 | `memento-*` ↔ `~/.memento/db/tenants/<tid>/lancedb/` | LanceDB reads/writes. |
| TB-3 | `memento-*` ↔ `~/.memento/db/tenants/<tid>/auth/` | credential hash (read-only at startup). |
| TB-4 | `memento-*` ↔ `~/.memento/backups/<tid>/` | AES-256-GCM ciphertext + wrapped key. |
| TB-5 | `memento-parse` ↔ `npx @firecrawl/anydoc` (subprocess) | markdown / extracted text on stdout. |
| TB-6 | `memento-embed-fastembed` ↔ HF cache | first-run model download (~500 MB). |

## 2. STRIDE matrix

The matrix lists each threat category, the Memento RS asset(s) at risk,
the realized mitigations (with code/test references), and any residual
risk that survives for the external audit to weigh.

### S — Spoofing

| Threat | Where | Mitigation | Residual |
|--------|-------|------------|----------|
| Forged bearer token to another tenant | TB-1 | Argon2id verification at `TenantResolverImpl::resolve_from_env` (`memento-tenant/src/resolver.rs`); uniform `AUTH_FAILED` — no existence leak (REQ-TA-002). | None at MVP. |
| Agent-id spoofing across agents | TB-1 | `MEMENTO_AGENT_ID` is required (`INVALID_INPUT` if missing — REQ-TA-003) and stamped on every audit line. | Hostile process on the same UID can claim any agent id; the agent id is an honor system, not authenticated. **Documented as MVP limitation.** |
| `McpServer::from_app` cross-tenant rebound | in-process | `from_app` asserts the resolved tenant matches the store tenant (T-071). | None. |

### T — Tampering

| Threat | Where | Mitigation | Residual |
|--------|-------|------------|----------|
| Tamper with chunks / docs in LanceDB | TB-2 | Per-tenant directories (TB-2 containment); TenantContext guard is the first call in every adapter (`memento-application/src/lib.rs: ensure_bound_tenant`). Direct FS edits bypass the bound context. | Host root user can edit any file. MVP accepts the standard Linux/Windows DAC posture. |
| Tamper with audit JSONL | TB-2 | Append-only writes via `AuditLogger::record` (`memento-application/src/audit.rs`); the no-secrets test (`crates/memento-application/tests/audit_nosecrets.rs`) pins the line shape. | File-system attacker can truncate / replace. The MVP does NOT sign the audit log; HMAC chaining is the post-MVP upgrade. |
| Malicious document → extracted markdown → injected into memory | TB-5 | The anydoc subprocess boundary uses the 6-step defensive pattern (extension allowlist, basename-only path, 50 MiB stdout cap + kill, 60 s timeout, 64 KiB stderr cap, staging dir cleaned per path) — see T-031 + `memento-parse/src/anydoc.rs`. | None known. |

### R — Repudiation

| Threat | Where | Mitigation | Residual |
|--------|-------|------------|----------|
| Operator denies performing `erase` / `delete` / `rotate-token` | TB-2 | Every REQ-CG-003 action emits one JSONL line; the audit log is per-tenant at `logs/<tid>.jsonl` (D8). | See tampering row: signed audit is post-MVP. |
| Unattributable action across agents | TB-1 | `MEMENTO_AGENT_ID` is captured on every event. | Same MVP limitation as spoofing. |

### I — Information disclosure

| Threat | Where | Mitigation | Residual |
|--------|-------|------------|----------|
| Search across tenants | TB-2 | Workspace isolation matrix tested in `crates/memento-application/src/search.rs` (2 tenants × 2 workspaces, FTS + hybrid). Foreign `TenantContext` cannot be constructed outside `memento-tenant`. | None known. |
| Audit log leaks content / credentials / keys | TB-2 | `AuditEvent::target` carries ids/counts only; the no-secrets scan (`crates/memento-application/tests/audit_nosecrets.rs`) plants strings from every content surface and asserts none appear in any line. | None known. |
| Master key on disk in plaintext | TB-4 | `db/tenants/<tid>/keys/master.key` is wrapped per-backup; raw key material only lives in memory during a backup or restore. | Hostile process with read access to `<root>` can copy the master key. **Documented as MVP limitation — disk encryption is the operational answer.** |
| Backup cipher key on disk in plaintext | TB-4 | Per-backup AES-256-GCM key is wrapped by the master key (`backup.key.json`); the unwrapped form exists only during `tenant restore`. | Same as master key. |
| anydoc subprocess exfiltrates via stderr / env | TB-5 | 64 KiB stderr cap + kill-on-cap; env is NOT inherited (`std::env::remove_var` clears, then a fixed allowlist is set; see `memento-parse/src/anydoc.rs`). | None known. |
| Embedding model first-run download leaks host fingerprint | TB-6 | Model is pinned (sha256 verified by HF cache); the only network call is to `huggingface.co` for the model + tokenizer. | First-run only; subsequent runs are offline. |

### D — Denial of service

| Threat | Where | Mitigation | Residual |
|--------|-------|------------|----------|
| Quota-bypass via giant doc → super-linear chunker hang | TB-2 → TB-5 | O(n) `Chunker::token_count` quota pre-guard in `memento-application/src/ingest.rs:over_chunk_quota` (`tokens > MAX_CHUNKS_PER_DOC * MAX_TOKENS` short-circuits before the O(n²) splitter — discovery 2616). | Sub-quota docs (e.g., 2.25 M tokens ≈ 7.5 k chunks) still cost minutes; windowed chunking is the post-MVP mitigation. |
| Subprocess hang | TB-5 | 60 s timeout + kill-on-timeout. | None known. |
| Massive stdout from anydoc | TB-5 | 50 MiB stdout cap + kill-on-cap. | None known. |
| `memento-mcp` flooded with malformed frames | TB-1 | rmcp transport ignores unparsable JSON; the session survives (T-070: 3 RED tests pin the survival). | None known. |

### E — Elevation of privilege

| Threat | Where | Mitigation | Residual |
|--------|-------|------------|----------|
| Subprocess argument injection | TB-5 | Extension allowlist gate (`SUBPROCESS_ARGV_INVALID`) before staging; basename-only path arg (no `..` escape); argv constructed positionally — no shell. | None known. |
| `tenant delete` without ceremony | TB-1 | CLI ceremony requires 'yes' on stdin; aborts otherwise (data intact). `--json` mode suppresses the prompt so stderr stays pure JSON. | A non-interactive caller using `--json` could delete without a human in the loop. **Documented as MVP limitation — production deployments should wrap with their own confirmation.** |
| Bulk-ingest path traversal | TB-1 | Two gates in `memento-cli/src/commands/ingest.rs`: (1) reject `..` in the root argument before any walk; (2) `canonical_within` on every entry — Windows `\\?\` verbatim paths canonicalized BOTH sides. | None known. |
| Tenant override (process tries to bind a tenant it doesn't own) | in-process | `AppService::open` resolves the bearer, asserts the resolved tenant matches the store tenant (T-071 / REQ-TA-002). | None known. |
| Restore into a live store | TB-2 | `tenant restore` resolves only the bound context without opening the store; quiesce check rejects any tenant dir with a non-empty `lancedb/` (T-082 deviation — opening the store would create `lancedb/` and trip the check). | None known. |

## 3. Audit event matrix (REQ-CG-003)

The following actions emit one JSONL line per call. **Reads are
intentionally NOT audited** (per design D7 + REQ-CG-003):

| Action       | Emitted by                          | Target shape                                       |
|--------------|--------------------------------------|----------------------------------------------------|
| `search`     | `memory.search` (CLI + MCP)          | `{query_len, hits, hybrid}` — never the query text |
| `ingest`     | `memory.ingest_*` (CLI + MCP)        | `{doc_id, chunks, duplicate, source}`              |
| `delete`     | `memory.delete` (CLI + MCP)          | `{scope, count}`                                   |
| `tenant_admin` | `tenant create/rotate/delete/retention/export` | `{action, …}`                                  |
| `backup`     | `tenant backup` + worker job         | `{backup_dir, chunk_count, created_at}`            |
| `restore`    | `tenant restore`                     | `{backup_dir, chunks_restored}`                    |
| `rotate_token` | `tenant rotate-token`              | `{revoked_at}`                                     |
| `erase`      | `tenant delete` (after purge)        | `{deleted, backups_count, master_key_destroyed, destroyed_at}` |
| `sweep`      | `tenant sweep` + worker job          | `{retention_days, cutoff, expired_count}`          |
| `prune`      | worker `MaintenanceJob`              | `{pruned_versions, rotation_secs}`                 |

The audit retention policy is documented in
[`audit-log-retention.en.md`](audit-log-retention.en.md) (T-120).

## 4. Open items for the external audit

The audit-prep checklist (`audit-pre-shipped-checklist.md`) tracks the
evidence the audit firm will request. The MVP ships with:

- 308 unit + integration tests passing on the bootstrap host.
- `cargo audit` clean on the pinned dep set (CI gate, `scripts/audit-prep.sh`).
- `cargo geiger` report archived per release (no new unsafe introduced
  in batch 11; the workspace has zero `unsafe` blocks in product code).
- 13-crate hexagonal architecture + per-tenant directory containment as
  defense-in-depth (4 layers: bound context, repository guard, FS
  isolation, audit trail).

What we expect the audit to challenge:

- Honor-system agent id (spoofing row above).
- Disk-encryption assumption (information disclosure row above).
- HMAC chaining on the audit log (tampering / repudiation rows).
- Bulk-ingest in `--json` mode (no human in the loop).
- Windowed chunking for the sub-quota-large doc case.
