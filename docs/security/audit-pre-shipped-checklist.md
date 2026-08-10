# Pre-shipped audit checklist — Memento RS MVP

> Use this checklist when engaging the external security audit firm
> before the MVP ships. Every item maps to a concrete artifact (test
> output, doc, code reference). The audit-prep scripts (see
> `scripts/audit-prep.sh` / `scripts/audit-prep.ps1`) regenerate the
> evidence in one shot.

## 1. Dependency hygiene

- [ ] **Cargo dependency pins**: `Cargo.lock` is committed; every dep
      version is reproducible from `cargo metadata --frozen --locked`.
      - Artifact: `Cargo.lock`, `docs/dependencies.md`.
- [ ] **`cargo audit` clean**: zero advisories on the pinned set.
      - Regenerate: `scripts/audit-prep.sh` (POSIX) or
        `scripts/audit-prep.ps1` (Windows).
      - Archive: `audit-evidence/cargo-audit-<date>.txt`.
- [ ] **License scan**: every dep is MIT / Apache-2.0 / BSD-compatible.
      - Command: `cargo deny check license` (install via `cargo-deny`).
      - Archive: `audit-evidence/cargo-deny-license-<date>.txt`.

## 2. Unsafe-code surface

- [ ] **`cargo geiger` zero new `unsafe`**: the workspace introduces
      zero `unsafe` blocks in product code (every `unsafe` is in a
      pinned transitive dep, recorded in the geiger report).
      - Regenerate: `scripts/audit-prep.sh`.
      - Archive: `audit-evidence/cargo-geiger-<date>.txt`.

## 3. Test evidence

- [ ] **Workspace tests**: `cargo test --workspace -j 2 -- --test-threads=1`
      passes (308 tests expected at MVP freeze).
      - Archive: `audit-evidence/cargo-test-<date>.log`.
- [ ] **Audit no-secrets scan**: `cargo test -p memento-application --test audit_nosecrets`
      passes (T-066 — content / credential / key-material scan over
      every JSONL line).
      - Archive: `audit-evidence/audit-nosecrets-<date>.log`.
- [ ] **Threat-matrix RED tests** pass:
      - `cargo test -p memento-parse --test subprocess_red` (T-030: path
        traversal, arg injection, output bomb, hang).
      - `cargo test -p memento-mcp --test protocol_red` (T-070: malformed
        frames, schema-violating params, session survives).
      - `cargo test -p memento-cli --test bulk_red` (T-080: dotdot
        traversal, canonical containment, symlink escape).

## 4. Architecture evidence

- [ ] **Threat model** (`docs/security/threat-model.md`) — STRIDE
      matrix, audit event matrix, open items for the audit.
- [ ] **Audit-log retention policy** (`docs/security/audit-log-retention.{es,en}.md`)
      — T-120 decision: data retention matches audit TTL by default,
      with per-tenant override.
- [ ] **Crypto-shredding posture** (`docs/security/threat-model.md`,
      §2 I row): `db/tenants/<tid>/keys/master.key` destruction makes
      every prior backup unrecoverable immediately (GDPR Art. 17(3)(b)
      technical-necessity posture).
- [ ] **Per-tenant FS containment** (`<root>/db/tenants/<tid>/`,
      `<root>/logs/<tid>.jsonl`, `<root>/backups/<tid>/<ts>/`) —
      defense-in-depth layer 3.

## 5. Operational evidence

- [ ] **`memento health` works inside Docker** (`docker compose run --rm cli`):
      exit 0 with `status: ok`.
- [ ] **`memento-worker --now` succeeds end-to-end** in CI (sweep +
      maintenance + backup jobs all OK).
- [ ] **Backup round-trip drill** (T-065 acceptance): wipe live store
      → `tenant restore` → search equivalence + provenance intact.

## 6. Bilingual evidence

- [ ] **CLI bilingual help** — `memento --help` (ES) and
      `memento --locale en --help` (EN) snapshot tests pass (T-081).
- [ ] **MCP bilingual descriptions** — tools/list returns Spanish
      strings by default, English when `MEMENTO_LOCALE=en`.
- [ ] **Error rendering** — every `DomainError` has a stable code
      (`code` field) + bilingual `message_es` / `message_en`
      (REQ-CL-004, REQ-MS-004).

## 7. Open commitments to the audit

Items we explicitly want the audit firm to challenge:

1. Honor-system `MEMENTO_AGENT_ID` (no authentication of the calling
   agent beyond the bearer token).
2. Disk-encryption assumption for `db/tenants/<tid>/keys/master.key`
   (the host's full-disk encryption is the operational answer).
3. Append-only audit log without HMAC chaining (signed audit log is
   post-MVP).
4. Bulk-ingest + `tenant delete` in `--json` mode (no human in the
   loop — production deployments should wrap with their own
   confirmation).
5. Windowed chunking for sub-quota-large documents (current cap: 10 k
   chunks per doc via the O(n) pre-guard).

See `docs/security/threat-model.md` §4 for the full list.
