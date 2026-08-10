# Audit-log retention policy (T-120)

> Status: **decided** for MVP. This document is the source of truth on
> the audit JSONL lifetime; the code lives in
> `crates/memento-application/src/{tenant_config,audit,sweep,erase}.rs`.

## Decision summary

| Aspect                             | Policy                                                                  |
|------------------------------------|-------------------------------------------------------------------------|
| Default                            | Audit retention **mirrors** the data retention (30 d).                  |
| Per-tenant override                | `[retention] audit_days = N` in `db/tenants/<tid>/config.toml`.         |
| `audit_days = 0`                   | Opt-out: audit is retained until manual deletion or `tenant erase`.     |
| Sweep mechanism                    | `AppService::retention_sweep` sweeps BOTH the data store AND the JSONL. |
| `tenant delete --tenant` ceremony  | Removes `logs/<tid>.jsonl` as part of the erasure (GDPR Art. 17).       |
| Report                             | `SweepReport.audit_expired_count` is printed in CLI and serialized.     |

## Why this policy

Three options were considered in the original proposal (obs 2588,
T-120):

1. **Same horizon as data** (30 d default) — accepted.
2. **`audit_retention_days` per tenant, explicit override** — accepted
   and combined with (1).
3. **7-day grace post-`erase`** — dropped for MVP; the cost of a
   quarantine dir is not justified. The simpler alternative is to let
   `erase` physically remove the audit log. If production deployments
   need a grace window, a post-MVP `--audit-grace-days` flag can be
   added without breaking the contract.

## How the sweep applies

`AppService::retention_sweep` now:

1. Reads `TenantConfig` for the bound tenant.
2. If `retention_days == 0` → skips the data sweep BUT still applies
   the audit sweep when `audit_retention_days > 0`.
3. If `audit_retention_days == 0` (opt-out) → does not touch the
   audit log.
4. Otherwise: `audit_cutoff = clock.now() - audit_days` calls
   `AuditLogger::sweep_expired(cutoff)` which:
   - reads the JSONL line-by-line;
   - parses `ts` as RFC 3339;
   - keeps lines with `ts >= cutoff`;
   - drops lines with `ts < cutoff` (**malformed lines are kept** to
     preserve evidence);
   - rewrites the file atomically (temp + rename).

The report (`SweepReport`) carries two fields:

```rust
pub struct SweepReport {
    pub expired_count: usize,          // chunks removed from Lance
    pub freed_bytes: u64,
    pub chore_id: ChoreId,
    pub audit_expired_count: usize,    // JSONL lines removed (T-120)
}
```

The CLI prints both in human mode:

```
sweep: 5 chunks expirados, 12 líneas de auditoría expiradas
```

And in `--json` both fields go in the envelope.

## How `erase` applies

`AppService::erase` runs the full crypto-shredding chain:

1. Store purge (delete → Compact → Prune) — REQ-CG-001.
2. Master-key destruction (`keys/master.key`) — D4 (older backups
   become unrecoverable immediately).
3. Removal of `okf-bundles/`, `conversation/`, `config.toml`.
4. Emission of the `erase` audit line (with `chore_id` and `ts`).
5. **Removal of the `logs/<tid>.jsonl` file** (T-120).

Step 4 happens before step 5 so the `erase` line is the LAST line
written before the file is deleted. Any later forensic attempt reads
"this tenant was erased at this instant" from any other artifact that
survived `erase`.

## Per-tenant override

The file `db/tenants/<tid>/config.toml` accepts:

```toml
[tenant]
name = "My tenant"

[retention]
days = 30                  # data: 30 d
audit_days = 365           # audit: 1 year (compliance)
```

Parser rules:

- `audit_days` absent → mirror `days` (privacy-forward default).
- `audit_days = 0` → opt-out (audit retained indefinitely).
- `audit_days = garbage` → fall back to mirror (never error;
  corruption must not break reads).

## Compatibility

- `SweepReport` gains the field `audit_expired_count` with
  `#[serde(default)]` → existing JSONs deserialize with `0`.
- `TenantConfig` gains `audit_retention_days: Option<u64>` → backward /
  forward serialization without migrations.
- `set_retention_days` now preserves the previous `audit_retention_days`
  value (does not erase it when `days` changes); to touch the audit
  independently use `AppService::set_audit_retention_days`.

## Operations

Relevant CLI commands:

```bash
# Show effective retention (data + audit):
memento tenant retention
# Prints: "retención: 30 días (0 = desactivada)"
# (The audit horizon is not printed here yet — see --json for full detail.)

# Change ONLY the audit retention:
# (Not exposed in CLI yet; via AppService::set_audit_retention_days.
# Post-MVP: add `memento tenant retention --audit-days N`.)

# Force the sweep now:
memento tenant sweep
```

## What is NOT audited

Per D7 + REQ-CG-003, **reads are NOT audited** — `search` /
`context_fit` are pure `info`, they do not appear in the JSONL. See
[`threat-model.md`](threat-model.md) §3.
