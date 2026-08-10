# Política de retención del audit log (T-120)

> Estado: **decidido** para el MVP. Este documento es la fuente de verdad
> sobre la vida útil del audit JSONL; el código vive en
> `crates/memento-application/src/{tenant_config,audit,sweep,erase}.rs`.

## Resumen de la decisión

| Aspecto                              | Política                                                                   |
|--------------------------------------|----------------------------------------------------------------------------|
| Default                              | Audit retention **espeja** la retención de datos (30 d).                   |
| Override por tenant                  | `[retention] audit_days = N` en `db/tenants/<tid>/config.toml`.            |
| `audit_days = 0`                     | Opt-out: el audit se conserva hasta el borrado manual o la `erase` del tenant. |
| Mecanismo de barrido                 | `AppService::retention_sweep` barre ambos: datos (Lance) + JSONL.          |
| `tenant delete --tenant` (ceremonia) | Borra `logs/<tid>.jsonl` como parte del borrado (Art. 17 GDPR).            |
| Reporte                              | `SweepReport.audit_expired_count` se imprime en el CLI y se serializa.    |

## Por qué esta política

Se consideraron tres opciones en la propuesta original (obs 2588,
T-120):

1. **Mismo horizonte que los datos** (30 d default) — opción aceptada.
2. **`audit_retention_days` por tenant, override explícito** — opción
   aceptada y combinada con (1).
3. **7 días de gracia post-`erase`** — descartado para el MVP; el coste
   de mantener un quarantine dir separado no se justifica. La
   alternativa más simple: el `erase` deja el audit log físicamente
   eliminado. Si se necesita gracia en producción, se puede añadir un
   flag `--audit-grace-days` post-MVP sin romper el contrato.

## Cómo se aplica el barrido

`AppService::retention_sweep` ahora:

1. Lee `TenantConfig` del tenant enlazado.
2. Si `retention_days == 0` → salta el barrido de datos pero igual
   aplica el de auditoría si `audit_retention_days > 0`.
3. Si `audit_retention_days == 0` (opt-out) → no toca el audit log.
4. En cualquier otro caso: `audit_cutoff = clock.now() - audit_days`
   llama a `AuditLogger::sweep_expired(cutoff)` que:
   - lee el JSONL línea por línea;
   - parsea `ts` como RFC 3339;
   - conserva líneas con `ts >= cutoff`;
   - descarta líneas con `ts < cutoff` (**malformadas se conservan**
     para preservar evidencia);
   - reescribe el archivo atómicamente (temp + rename).

El reporte (`SweepReport`) lleva dos campos:

```rust
pub struct SweepReport {
    pub expired_count: usize,          // chunks eliminados de Lance
    pub freed_bytes: u64,
    pub chore_id: ChoreId,
    pub audit_expired_count: usize,    // líneas JSONL eliminadas (T-120)
}
```

El CLI imprime ambos en modo humano:

```
sweep: 5 chunks expirados, 12 líneas de auditoría expiradas
```

Y en `--json` los dos campos van en el envelope.

## Cómo se aplica `erase`

`AppService::erase` corre la cadena de crypto-shredding:

1. Purga del store (delete → Compact → Prune) — REQ-CG-001.
2. Destrucción de `keys/master.key` — D4 (los backups viejos quedan
   ilegibles inmediatamente).
3. Eliminación de `okf-bundles/`, `conversation/`, `config.toml`.
4. Emisión de la línea `erase` en el audit (con `chore_id` y `ts`).
5. **Eliminación del archivo `logs/<tid>.jsonl`** (T-120).

El paso 4 ocurre antes del 5, así la línea `erase` es la última línea
escrita antes del borrado del archivo. Cualquier intento posterior de
auditoría lee "este tenant fue borrado en este instante" en cualquier
otro artefacto que sobreviva al `erase`.

## Override por tenant

El archivo `db/tenants/<tid>/config.toml` admite:

```toml
[tenant]
name = "Mi tenant"

[retention]
days = 30                  # datos: 30 d
audit_days = 365           # auditoría: 1 año (compliance)
```

Reglas del parser:

- `audit_days` ausente → espejo de `days` (privacy-forward default).
- `audit_days = 0` → opt-out (audit retenido indefinidamente).
- `audit_days = basura` → fallback a espejo (nunca error; la
  corrupción no debe romper la lectura).

## Compatibilidad

- `SweepReport` gana el campo `audit_expired_count` con
  `#[serde(default)]` → los JSONs existentes deserializan con `0`.
- `TenantConfig` gana `audit_retention_days: Option<u64>` →
  serialización hacia atrás / hacia adelante sin migraciones.
- `set_retention_days` ahora preserva el valor previo de
  `audit_retention_days` (no lo borra al cambiar `days`); para tocar
  la auditoría independientemente existe
  `AppService::set_audit_retention_days`.

## Operatoria

Comandos CLI relevantes:

```bash
# Ver retención efectiva (datos + auditoría):
memento tenant retention
# Muestra: "retención: 30 días (0 = desactivada)"
# (El audit horizon no se imprime aquí todavía — se ve en --json.)

# Cambiar SOLO la retención de auditoría:
# (No expuesto en CLI todavía; vía AppService::set_audit_retention_days.
# Post-MVP: añadir `memento tenant retention --audit-days N`.)

# Forzar el barrido ahora:
memento tenant sweep
```

## Lo que NO se audita

Per D7 + REQ-CG-003, **las lecturas NO se auditan** — search /
context_fit son `info` puro, no aparecen en JSONL. Ver
[`threat-model.md`](threat-model.md) §3.
