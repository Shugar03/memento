# Runbook de operaciones — Memento RS

> Operaciones diarias para el equipo de plataforma. Cubre backup,
> restore, retención, auditoría, borrado GDPR y diagnóstico.
> Para la referencia de configuración, véase
> [docs/config-reference.es.md](config-reference.es.md).

## Backup cifrado

`memento tenant backup` ejecuta `compact → copy → encrypt` y deposita el
artefacto en `~/.memento/backups/<tid>/<ts>/`:

```
backups/<tid>/<ts>/
├── backup.enc            # blob AES-256-GCM (datos + manifest firmados)
├── backup.key.json       # clave AES del backup envuelta por la maestra
└── manifest.json         # { tenant_id, ts, chunk_count, version }
```

El envoltorio de clave usa la clave maestra del tenant (`keys/master.key`,
REQ-ML-005 / D4). Si destruyes esa clave, todos los backups del tenant
quedan irrecuperables (crypto-shredding — GDPR Art. 17(3)(b)).

Forma recomendada (Docker):

```bash
export MEMENTO_TOKEN=...
export MEMENTO_AGENT_ID=cli
scripts/backup.sh
```

Forma local (binario instalado en el host):

```bash
scripts/backup.sh --local --root /srv/memento
```

El script aborta con código 1 y mensaje claro si:

- `MEMENTO_TOKEN` no está exportada
- el directorio del tenant no existe
- `docker` no está en el `PATH` (modo compose)
- `memento` no está en el `PATH` (modo local)

Cada paso emite una línea `[backup <ts>] ...` para pipelines de logs.

## Restore

`tenant restore` es una operación **offline**: el store debe estar quieto
(la quiesce check rechaza un `lancedb/` no vacío). En docker compose eso
significa detener `worker` y `mcp` antes del move. `scripts/restore.sh`
lo hace automáticamente y los reactiva al terminar (a menos que pases
`--keep-services`).

```bash
scripts/restore.sh ~/.memento/backups/<tid>/<ts>
# variantes:
scripts/restore.sh /var/lib/memento/backups/<tid>/<ts> --keep-services
scripts/restore.sh /srv/memento/backups/<tid>/<ts> --local
```

Validaciones tempranas (antes de tocar el store):

- el directorio existe
- contiene `backup.enc`, `backup.key.json` y `manifest.json`
- `MEMENTO_TOKEN` está exportada

Si el backup está corrupto, `memento tenant restore` retorna
`BACKUP_CORRUPT` (REQ-MS-005) y deja el store sin modificar. La
estructura del `manifest.json` se valida antes de extraer (ver
`backup::restore_backup` en `memento-application`).

> **Nota importante:** `tenant restore` **NO** recupera las credenciales.
> El borrado (crypto-shredding de la clave maestra) es destructivo por
> diseño. Después de un restore tendrás que volver a emitir
> `memento tenant create --name <n>` para obtener un token nuevo.

## Retención y sweep

El horizonte por defecto es **30 días** desde la creación del chunk
(REQ-ML-003). El sweep se ejecuta automáticamente cada 24 h desde el
worker, o bajo demanda:

```bash
memento tenant sweep
```

Salida (modo humano):

```
sweep: 12 chunks expirados, 0 líneas de auditoría expiradas
```

En modo `--json`:

```json
{ "expired_count": 12, "audit_expired_count": 0, "ran_at": "..." }
```

Override por tenant (REQ-CG-002):

```bash
memento tenant retention --days 90          # aplica y persiste
memento tenant retention --days 0           # desactiva (conserva todo)
memento tenant retention                    # muestra el valor actual
```

El archivo editado es `db/tenants/<tid>/config.toml`:

```toml
[tenant]
name = "..."

[retention]
days = 90
audit_days = 365      # opcional; sin esta línea, espejo de `days`
```

## Auditoría

Cada tenant tiene su propio `logs/<tid>.jsonl` con líneas estructuradas.
Los eventos cubiertos son los de REQ-CG-003: `ingest`, `search`,
`feedback`, `delete`, `erase`, `backup`, `restore`, `rotate_token`,
`sweep`, `prune`, `audit_retention_change`.

La línea de auditoría **nunca** incluye el contenido del chunk, la
credencial ni el material de clave. El test
`crates/memento-application/tests/audit_nosecrets.rs` lo verifica.

Retención de auditoría (T-120):

- por defecto, la auditoría se barre junto con los datos
  (`audit_days` ausente → mirror de `days`)
- `audit_days = 0` opta explícitamente por **no** barrer la auditoría
  (se conserva hasta el `tenant erase`)
- las líneas malformadas se preservan (evidencia inalterable)

## Borrado GDPR (crypto-shredding)

`memento tenant delete --tenant` ejecuta la ceremonia completa:

1. Borra todas las filas (`LanceDB delete`)
2. Compacta la tabla (`OptimizeAction::Compact`)
3. Purga las versiones antiguas (`OptimizeAction::Prune`)
4. Destruye `keys/master.key` → todos los backups del tenant quedan
   irrecuperables al instante (REQ-CG-001)
5. Escribe la línea `erase` final en el audit log y borra
   `logs/<tid>.jsonl`
6. Borra `auth/credentials.toml` y `config.toml`

La ceremonia pide confirmación por stdin (`yes`). En modo `--json` no
hay confirmación interactiva — envuélvelo en tu propio gatekeeper de
producción.

Drill de verificación:

```bash
scripts/e2e-drill.sh --keep        # datos del drill en compose project memento-e2e
```

Después del drill, `docker compose -p memento-e2e down -v` lo limpia.

## Diagnóstico

| Síntoma | Comando | Esperado |
|---|---|---|
| ¿El binario arranca? | `memento health` | `{"ok": true}` |
| ¿Embeddings cargados? | `memento stats --json` | `models.cached: true` (después del primer ingest con embed) |
| ¿Cuántos chunks por workspace? | `memento memory stats --workspace <id>` | `chunks: N, docs: M` |
| ¿Tamaño en disco? | `du -sh ~/.memento/db ~/.memento/models ~/.memento/backups` | orden de magnitud coherente |
| ¿Logs del worker? | `docker compose logs -f worker` | líneas `JOB ... ok` cada 24 h |
| ¿Errores en el audit log? | `jq -r 'select(.ok == false) | .error_code' logs/<tid>.jsonl \| sort \| uniq -c` | sin crecimiento inesperado |

## Audit prep pre-shipped (REQ-OP-004)

```bash
scripts/audit-prep.sh --archive   # corre cargo-audit + cargo-geiger
                               # archiva evidencia en audit-evidence/<ts>/
```

Falla en cuanto aparece una vulnerabilidad conocida en las dependencias
pineadas. La política es "cero CVE al RC" — no se aceptan advisories sin
remediación documentada.

## Bench reproducible (REQ-MR-007, REQ-CK-002)

```bash
scripts/bench.sh                  # corrida de referencia (100k chunks, 10k+100k LOC)
scripts/bench.sh --quick          # smoke para CI (5k chunks, 10k LOC)
scripts/bench.sh --embed          # incluye embed bench (descarga ~500 MB)
```

El script termina con un "gate report" que falla en cuanto una métrica
sale del presupuesto:

- search p50 < 20 ms / p99 < 100 ms (warm, 100k chunks)
- code index 10k LOC < 2 s cold
- code index 100k LOC ≤ 30 s cold
- cold start < 3 s reportado, no gateado

Las desviaciones se imprimen con el valor medido; nunca se aceptan en
silencio.

## Ingestión masiva — preferí el servidor persistente

Para ingestión masiva, preferí el servidor MCP persistente (`memento-mcp-server.exe`) sobre el CLI one-shot:

- Cada invocación del CLI recarga el modelo ONNX (cold ~7s, warm ~3.8s) — proceso nuevo cada vez.
- El servidor mantiene el modelo residente: paga la carga UNA vez y queda caliente.
- Flujo recomendado: `memento-mcp-server.exe` con MEMENTO_TOKEN/MEMENTO_AGENT_ID/MEMENTO_ROOT, luego `memory.ingest_document`/`memory.ingest_text` por MCP.

## Estructura de directorios

```
~/.memento/
├── config.toml              # raíz (opcional; defaults si falta)
├── db/
│   └── tenants/<tid>/
│       ├── config.toml      # [tenant] name, [retention] days + audit_days
│       ├── auth/credentials.toml    # hashes Argon2id; 0600
│       ├── keys/master.key         # clave maestra AES-256-GCM
│       ├── lancedb/                # tablas: chunks, docs, feedback, symbols
│       ├── okf-bundles/<project_id>/   # L1 OKF bundles
│       └── conversation/
├── models/                  # caché ONNX (~500 MB MultilingualE5Small)
├── tmp/                     # staging efímero
├── backups/<tid>/<ts>/      # artefactos cifrados
└── logs/<tid>.jsonl         # auditoría por tenant
```

`auth/` + `keys/` viven dentro del dir del tenant (coherencia con
backup/erase).

## Cuándo NO usar este runbook

- **Recuperación de desastre del host**: si el disco muere, los backups
  cifrados siguen siendo válidos en cualquier host que tenga la clave
  maestra. Sin la clave maestra, los backups son bytes inútiles (es la
  postura GDPR; ver §"Crypto-shredding").
- **Migración entre versiones mayores**: el formato del backup puede
  cambiar. Verifica el campo `manifest.version` antes de restaurar en
  otra versión.
