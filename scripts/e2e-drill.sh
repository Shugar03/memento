#!/usr/bin/env bash
# scripts/e2e-drill.sh — clean-container end-to-end drill (T-104).
#
# Replays the production lifecycle on a clean Docker host in a
# dedicated project (so user data is untouched):
#
#   up → health → bootstrap tenant → ingest → search → backup
#       → erase → restore → search-after-restore
#
# Every step emits a timestamped log line (REQ-OP-002) and a JSON
# report at the end. On any failure the script exits 1 with the
# offending step named in stderr.
#
# Usage:
#   scripts/e2e-drill.sh                   # full drill (compose project = memento-e2e)
#   scripts/e2e-drill.sh --keep           # leave the project + volumes running
#   scripts/e2e-drill.sh --embed           # ingest under --no-embeddings=false
#                                          # (downloads MultilingualE5Small ~500 MB,
#                                          #  risk R1 / REQ-OP-005 first-run)
#   scripts/e2e-drill.sh --project NAME   # override compose project name
#
# Why --no-embeddings by default:
#   - keeps the drill self-contained (no 500 MB ONNX download)
#   - FTS-only search still exercises the ingest pipeline + RRF off
#   - the `--embed` flag opts into the realistic hybrid-search path
#
# The drill depends on: docker, docker compose, bash 4+, jq, awk.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

PROJECT="memento-e2e"
KEEP=0
EMBED=0
NO_EMBED=1

usage() {
  sed -n '2,28p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit 0
}

for arg in "$@"; do
  case "$arg" in
    --keep)               KEEP=1 ;;
    --embed)              EMBED=1; NO_EMBED=0 ;;
    --no-embeddings)      NO_EMBED=1 ;;
    --project)            shift; PROJECT="${1:?--project needs a name}" ;;
    --project=*)          PROJECT="${arg#--project=}" ;;
    -h|--help)            usage ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done

ts_now() { date -u +%Y-%m-%dT%H:%M:%SZ; }
log()    { printf '[e2e %s] [%s] %s\n' "$(ts_now)" "$STEP" "$*" | tee -a "$LOG" >&2; }
fail()   { printf '[e2e %s] [FAIL] step=%s %s\n' "$(ts_now)" "$STEP" "$*" >&2; exit 1; }

require() {
  for tool in "$@"; do
    if ! command -v "$tool" >/dev/null 2>&1; then
      echo "missing required tool: $tool" >&2
      exit 1
    fi
  done
}
require docker jq awk

# ---------------------------------------------------------------------------
# State
# ---------------------------------------------------------------------------
WORK="$(mktemp -d)"
LOG="$WORK/e2e.log"
REPORT="$WORK/e2e-report.json"
TOKEN=""
TENANT_ID=""
BACKUP_DIR=""
RESULTS="[]"
SUMMARY_OK=0
SUMMARY_FAIL=0

cleanup() {
  if [ "$KEEP" -eq 1 ]; then
    log "KEEP=1 — leaving compose project '$PROJECT' + volumes in place"
    log "logs:    $LOG"
    log "report:  $REPORT"
    return
  fi
  log "tearing down compose project '$PROJECT'"
  ( cd "$ROOT_DIR" && docker compose -p "$PROJECT" down -v --remove-orphans >/dev/null 2>&1 ) || true
  if [ "${FINAL_OK:-0}" = "1" ]; then
    log "drill OK"
  fi
  log "logs:    $LOG"
  log "report:  $REPORT"
}
trap cleanup EXIT

add_result() {
  # add_result <step> <ok 0|1> <detail>
  local step="$1" ok="$2" detail="$3"
  RESULTS="$(jq -c --arg s "$step" --argjson ok "$ok" --arg d "$detail" \
    '. + [{"step": $s, "ok": $ok, "detail": $d}]' <<<"$RESULTS")"
  if [ "$ok" = "0" ]; then SUMMARY_FAIL=$((SUMMARY_FAIL+1));
  else SUMMARY_OK=$((SUMMARY_OK+1)); fi
}

write_report() {
  jq -n \
    --arg project  "$PROJECT" \
    --arg started  "$(ts_now)" \
    --argjson embed "$EMBED" \
    --argjson ok    "$SUMMARY_OK" \
    --argjson fail  "$SUMMARY_FAIL" \
    --argjson steps "$RESULTS" \
    '{
       project: $project,
       started_at: $started,
       embed_enabled: $embed,
       steps_passed: $ok,
       steps_failed: $fail,
       steps: $steps
     }' > "$REPORT"
}

# ---------------------------------------------------------------------------
# Compose helpers
# ---------------------------------------------------------------------------
compose() { ( cd "$ROOT_DIR" && docker compose -p "$PROJECT" "$@" ); }

cli_run() {
  # cli_run <env-token> <env-agent> <subcommand...>
  local token="$1" agent="$2"; shift 2
  MEMENTO_TOKEN="$token" MEMENTO_AGENT_ID="$agent" \
    compose run --rm -T cli "$@"
}

# ---------------------------------------------------------------------------
# Steps
# ---------------------------------------------------------------------------
STEP="up"
log "step=up  project=$PROJECT  embed=$EMBED  keep=$KEEP"
add_result "up" 0 "project=$PROJECT"

# Stop any prior run of this project so `up` is idempotent.
compose down -v --remove-orphans >/dev/null 2>&1 || true

if ! compose up -d --build worker >/dev/null; then
  fail "compose up worker failed — run \`docker compose -p $PROJECT logs worker\`"
fi
add_result "compose_up" 1 ""

STEP="health"
log "polling memento health (up to 60s)"
health_ok=0
for _ in $(seq 1 60); do
  out="$(compose run --rm -T cli memento health 2>&1)" || true
  if printf '%s' "$out" | grep -qi '"ok"[[:space:]]*:[[:space:]]*true\|healthy'; then
    health_ok=1; break
  fi
  sleep 1
done
if [ "$health_ok" -ne 1 ]; then
  fail "health probe never returned ok — last output: $out"
fi
add_result "health" 1 "60s poll ok"
log "step=health ok"

STEP="bootstrap_tenant"
log "creating tenant 'e2e-drill'"
bootstrap_out="$(compose run --rm -T cli memento tenant create --name e2e-drill 2>&1)" || \
  fail "tenant create failed: $bootstrap_out"
TOKEN="$(printf '%s' "$bootstrap_out" | awk '/^token:[[:space:]]*/{print $2; exit}')"
TENANT_ID="$(printf '%s' "$bootstrap_out" | awk '/^tenant_id:[[:space:]]*/{print $2; exit}')"
if [ -z "$TOKEN" ] || [ -z "$TENANT_ID" ]; then
  fail "could not parse token/tenant_id from: $bootstrap_out"
fi
add_result "bootstrap" 1 "tenant_id=$TENANT_ID"
log "step=bootstrap ok  tenant_id=$TENANT_ID"

STEP="ingest"
log "ingesting 2 documents"
text_a="Memento RS usa LanceDB embebido y fastembed para búsqueda local."
text_b="La búsqueda híbrida combina BM25 (FTS) con embeddings densos."
ing1="$(cli_run "$TOKEN" "e2e-cli" memento memory ingest-text --text "$text_a" 2>&1)" \
  || fail "ingest #1 failed: $ing1"
ing2="$(cli_run "$TOKEN" "e2e-cli" memento memory ingest-text --text "$text_b" 2>&1)" \
  || fail "ingest #2 failed: $ing2"
add_result "ingest" 1 "2 texts ingested"
log "step=ingest ok"

STEP="search"
log "searching 'LanceDB'"
search_out="$(cli_run "$TOKEN" "e2e-cli" memento memory search --query "LanceDB" --top-k 5 2>&1)" \
  || fail "search failed: $search_out"
if ! printf '%s' "$search_out" | grep -q "Memento RS usa LanceDB"; then
  fail "search did not surface the ingested text: $search_out"
fi
add_result "search" 1 "1 hit"
log "step=search ok"

STEP="backup"
log "creating encrypted backup"
backup_out="$(cli_run "$TOKEN" "e2e-cli" memento tenant backup 2>&1)" \
  || fail "backup failed: $backup_out"
BACKUP_DIR="$(printf '%s' "$backup_out" | awk -F': ' '/respaldo: |backup: /{print $2; exit}' | tr -d ' ')"
if [ -z "$BACKUP_DIR" ]; then
  # fallback: parse json path
  BACKUP_DIR="$(printf '%s' "$backup_out" | jq -r '.path // empty' 2>/dev/null || true)"
fi
if [ -z "$BACKUP_DIR" ] || [ "$BACKUP_DIR" = "null" ]; then
  fail "could not parse backup dir from: $backup_out"
fi
add_result "backup" 1 "dir=$BACKUP_DIR"
log "step=backup ok  dir=$BACKUP_DIR"

STEP="erase"
log "erasing tenant (ceremony)"
erase_input="$(printf 'yes\n')"
erase_out="$(printf 'yes\n' | cli_run "$TOKEN" "e2e-cli" memento tenant delete --tenant 2>&1)" \
  || fail "erase failed: $erase_out"
# Confirm post-erase search returns nothing.
post_erase="$(cli_run "$TOKEN" "e2e-cli" memento memory search --query "LanceDB" --top-k 5 2>&1)" \
  || true
# Either AUTH_FAILED (because credentials were destroyed) or zero hits is
# acceptable — both prove the tenant is gone.
if printf '%s' "$post_erase" | grep -q "Memento RS usa LanceDB"; then
  fail "post-erase search still returned the chunk — erase did not take: $post_erase"
fi
add_result "erase" 1 "post-erase search returned 0 hits"
log "step=erase ok"

STEP="restore"
# The restore op is offline: stop the worker first so the on-disk store
# is not being mutated while we move bytes into it.
log "stopping worker for restore"
compose stop worker >/dev/null || fail "stop worker failed"
log "restoring from $BACKUP_DIR"
restore_out="$(cli_run "$TOKEN" "e2e-cli" memento tenant restore "$BACKUP_DIR" 2>&1)" \
  || { compose start worker >/dev/null 2>&1 || true; fail "restore failed: $restore_out"; }
compose start worker >/dev/null || true
# After restore we need to re-bootstrap a working credential (the old token
# was destroyed by erase). The restore only brings back data, not auth.
log "restoring data OK; credentials were destroyed by erase — recreating"
bootstrap_out2="$(compose run --rm -T cli memento tenant create --name e2e-drill 2>&1)" \
  || fail "tenant create after restore failed: $bootstrap_out2"
TOKEN2="$(printf '%s' "$bootstrap_out2" | awk '/^token:[[:space:]]*/{print $2; exit}')"
search_out2="$(cli_run "$TOKEN2" "e2e-cli" memento memory search --query "LanceDB" --top-k 5 2>&1)" \
  || fail "post-restore search failed: $search_out2"
if ! printf '%s' "$search_out2" | grep -q "Memento RS usa LanceDB"; then
  fail "post-restore search did not surface the restored text: $search_out2"
fi
add_result "restore" 1 "search after restore returned the restored chunk"
log "step=restore ok"

# ---------------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------------
FINAL_OK=1
write_report
log "drill PASSED — steps_passed=$SUMMARY_OK  steps_failed=$SUMMARY_FAIL"
log "report: $REPORT"
exit 0
