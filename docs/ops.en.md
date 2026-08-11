# Ops runbook — Memento RS

> Day-to-day operations for the platform team: backup, restore,
> retention, audit log, GDPR erasure and diagnostics.
> For the configuration reference, see
> [docs/config-reference.en.md](config-reference.en.md).

## Encrypted backup

`memento tenant backup` runs `compact → copy → encrypt` and drops the
artifact under `~/.memento/backups/<tid>/<ts>/`:

```
backups/<tid>/<ts>/
├── backup.enc            # AES-256-GCM blob (data + signed manifest)
├── backup.key.json       # AES backup key wrapped by the tenant master key
└── manifest.json         # { tenant_id, ts, chunk_count, version }
```

The key wrap uses the tenant master key (`keys/master.key`,
REQ-ML-005 / D4). If you destroy that key, every backup of the tenant
becomes unrecoverable at once (crypto-shredding — GDPR Art. 17(3)(b)).

Recommended form (Docker):

```bash
export MEMENTO_TOKEN=...
export MEMENTO_AGENT_ID=cli
scripts/backup.sh
```

Local form (host-installed binary):

```bash
scripts/backup.sh --local --root /srv/memento
```

The script aborts with exit 1 and a clear message if:

- `MEMENTO_TOKEN` is not exported
- the tenant directory does not exist
- `docker` is not on `PATH` (compose mode)
- `memento` is not on `PATH` (local mode)

Every step emits a `[backup <ts>] ...` line for log pipelines.

## Restore

`tenant restore` is an **offline** op: the store must be quiet (the
quiesce check rejects a non-empty `lancedb/`). Under docker compose that
means stopping `worker` and `mcp` before the move. `scripts/restore.sh`
does that automatically and brings them back when done (unless you pass
`--keep-services`).

```bash
scripts/restore.sh ~/.memento/backups/<tid>/<ts>
# variants:
scripts/restore.sh /var/lib/memento/backups/<tid>/<ts> --keep-services
scripts/restore.sh /srv/memento/backups/<tid>/<ts> --local
```

Early validations (before touching the store):

- directory exists
- contains `backup.enc`, `backup.key.json` and `manifest.json`
- `MEMENTO_TOKEN` is exported

If the backup is corrupt, `memento tenant restore` returns
`BACKUP_CORRUPT` (REQ-MS-005) and leaves the store untouched. The
`manifest.json` structure is validated before any extract (see
`backup::restore_backup` in `memento-application`).

> **Important caveat:** `tenant restore` does **NOT** recover the
> credentials. The master-key destruction is destructive by design.
> After a restore you must re-issue `memento tenant create --name <n>`
> to obtain a fresh token.

## Retention and sweep

Default horizon is **30 days** from chunk creation (REQ-ML-003). The
sweep runs automatically every 24 h from the worker, or on demand:

```bash
memento tenant sweep
```

Human-mode output:

```
sweep: 12 expired chunks, 0 expired audit lines
```

`--json` mode:

```json
{ "expired_count": 12, "audit_expired_count": 0, "ran_at": "..." }
```

Per-tenant override (REQ-CG-002):

```bash
memento tenant retention --days 90          # apply and persist
memento tenant retention --days 0           # opt out (retain everything)
memento tenant retention                    # show the current value
```

The file edited is `db/tenants/<tid>/config.toml`:

```toml
[tenant]
name = "..."

[retention]
days = 90
audit_days = 365      # optional; missing → mirror of `days`
```

## Audit log

Every tenant has its own `logs/<tid>.jsonl` with structured lines. The
events covered are REQ-CG-003: `ingest`, `search`, `feedback`,
`delete`, `erase`, `backup`, `restore`, `rotate_token`, `sweep`,
`prune`, `audit_retention_change`.

The audit line **never** carries chunk content, the credential, or key
material. The test
`crates/memento-application/tests/audit_nosecrets.rs` enforces that.

Audit retention (T-120):

- by default, the audit log is swept together with the data
  (`audit_days` absent → mirrors `days`)
- `audit_days = 0` opts explicitly out of audit retention
  (kept until `tenant erase`)
- malformed lines are preserved (tamper-evidence)

## GDPR erasure (crypto-shredding)

`memento tenant delete --tenant` runs the full ceremony:

1. Deletes every row (`LanceDB delete`)
2. Compacts the table (`OptimizeAction::Compact`)
3. Prunes old versions (`OptimizeAction::Prune`)
4. Destroys `keys/master.key` → every backup of the tenant becomes
   unrecoverable at once (REQ-CG-001)
5. Writes the final `erase` audit line and removes `logs/<tid>.jsonl`
6. Removes `auth/credentials.toml` and `config.toml`

The ceremony asks for stdin confirmation (`yes`). Under `--json` there
is no interactive confirmation — wrap it with your own production
gatekeeper.

Verification drill:

```bash
scripts/e2e-drill.sh --keep        # drill data under compose project memento-e2e
```

After the drill, `docker compose -p memento-e2e down -v` cleans it up.

## Diagnostics

| Symptom | Command | Expected |
|---|---|---|
| Binary up? | `memento health` | `{"ok": true}` |
| Embeddings loaded? | `memento stats --json` | `models.cached: true` (after first ingest with embed) |
| Chunks per workspace? | `memento memory stats --workspace <id>` | `chunks: N, docs: M` |
| Disk usage? | `du -sh ~/.memento/db ~/.memento/models ~/.memento/backups` | sensible order of magnitude |
| Worker logs? | `docker compose logs -f worker` | `JOB ... ok` lines every 24 h |
| Errors in audit log? | `jq -r 'select(.ok == false) \| .error_code' logs/<tid>.jsonl \| sort \| uniq -c` | no unexpected growth |

## Pre-shipped audit prep (REQ-OP-004)

```bash
scripts/audit-prep.sh --archive   # runs cargo-audit + cargo-geiger
                               # archives evidence to audit-evidence/<ts>/
```

Fails on the first known vulnerability in the pinned dep set. The
policy is "zero CVE at RC" — no advisory without documented
remediation is accepted.

## Reproducible bench (REQ-MR-007, REQ-CK-002)

```bash
scripts/bench.sh                  # reference run (100k chunks, 10k+100k LOC)
scripts/bench.sh --quick          # CI smoke (5k chunks, 10k LOC)
scripts/bench.sh --embed          # includes the embed bench (~500 MB download)
```

The script ends with a "gate report" that fails the moment any metric
falls outside the budget:

- search p50 < 20 ms / p99 < 100 ms (warm, 100k chunks)
- code index 10k LOC < 2 s cold
- code index 100k LOC ≤ 30 s cold
- cold start < 3 s reported, not gated

Deviations are printed with the measured value; never silently accepted.

## Bulk ingest — prefer the persistent server

For bulk ingestion, prefer the persistent MCP server over the one-shot CLI:

- Every CLI invocation reloads the ONNX model (cold ~7s, warm ~3.8s) — a new process each time.
- The server keeps the model resident: pays the load ONCE and stays warm.
- Recommended flow: `memento-mcp-server.exe` with MEMENTO_TOKEN/MEMENTO_AGENT_ID/MEMENTO_ROOT, then `memory.ingest_document`/`memory.ingest_text` over MCP.

## Directory layout

```
~/.memento/
├── config.toml              # root (optional; defaults if missing)
├── db/
│   └── tenants/<tid>/
│       ├── config.toml      # [tenant] name, [retention] days + audit_days
│       ├── auth/credentials.toml    # Argon2id hashes; 0600
│       ├── keys/master.key         # tenant master AES-256-GCM key
│       ├── lancedb/                # tables: chunks, docs, feedback, symbols
│       ├── okf-bundles/<project_id>/   # L1 OKF bundles
│       └── conversation/
├── models/                  # ONNX cache (~500 MB MultilingualE5Small)
├── tmp/                     # ephemeral staging
├── backups/<tid>/<ts>/      # encrypted artifacts
└── logs/<tid>.jsonl         # per-tenant audit
```

`auth/` + `keys/` live inside the tenant dir (coherent with
backup / erase).

## When NOT to use this runbook

- **Host disaster recovery**: if the disk dies, the encrypted backups
  remain valid on any host that holds the master key. Without the
  master key, backups are useless bytes — that is the GDPR posture;
  see §"Crypto-shredding".
- **Migration across major versions**: the backup format can change.
  Check `manifest.version` before restoring on a different version.
