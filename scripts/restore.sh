#!/usr/bin/env bash
# scripts/restore.sh — operator-facing restore helper (T-111).
#
# Wraps `memento tenant restore <backup-dir>` (decrypt → validate
# BACKUP_VERSION / tenant match → stage → move into a quiesced tenant
# dir, REQ-ML-005). Restore is an OFFLINE op: the store must be quiet,
# which in docker-compose terms means the `worker` and `mcp` services
# are stopped while the restore happens.
#
# Fail-loud contract (REQ-OP-002): any non-zero exit or pre-condition
# failure returns 1 with a clear stderr message.
#
# Usage:
#   scripts/restore.sh /var/lib/memento/backups/<tid>/<ts>            # docker compose
#   scripts/restore.sh /var/lib/memento/backups/<tid>/<ts> --local    # local binary
#   scripts/restore.sh <dir> --root /srv/memento                      # override --root
#   scripts/restore.sh <dir> --keep-services                          # do NOT stop
#                                                                    # compose services
#
# Env:
#   MEMENTO_TOKEN      required (read on the host, passed to the container)
#   MEMENTO_AGENT_ID   optional, default `cli`
#   MEMENTO_ROOT       default ~/.memento (matches docker-compose)

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

MODE="compose"
KEEP_SERVICES=0
MEMENTO_ROOT="${MEMENTO_ROOT:-$HOME/.memento}"
BACKUP_DIR=""

usage() {
  sed -n '2,22p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit 0
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --local)          MODE="local"; shift ;;
    --keep-services)  KEEP_SERVICES=1; shift ;;
    --root)           shift; MEMENTO_ROOT="${1:?--root needs a path}"; shift ;;
    --root=*)         MEMENTO_ROOT="${1#--root=}"; shift ;;
    -h|--help)        usage ;;
    -*)               echo "unknown flag: $1" >&2; exit 2 ;;
    *)                BACKUP_DIR="$1"; shift ;;
  esac
done

ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
log() { printf '[restore %s] %s\n' "$ts" "$*"; }

if [ -z "$BACKUP_DIR" ]; then
  echo "usage: scripts/restore.sh <backup-dir> [--local] [--root PATH] [--keep-services]" >&2
  exit 2
fi

if [ ! -d "$BACKUP_DIR" ]; then
  echo "backup dir does not exist: $BACKUP_DIR" >&2
  exit 1
fi

# A backup directory must contain backup.enc + backup.key.json + manifest.json.
# If any of the three is missing, the restore will fail anyway — fail earlier
# with a clearer message.
for f in backup.enc backup.key.json manifest.json; do
  if [ ! -f "$BACKUP_DIR/$f" ]; then
    echo "missing required file: $BACKUP_DIR/$f" >&2
    exit 1
  fi
done

if [ -z "${MEMENTO_TOKEN:-}" ]; then
  echo "MEMENTO_TOKEN is not set — export it before running restore.sh (REQ-TA-002)" >&2
  exit 1
fi

# Quiesce: stop the worker and the mcp server so the on-disk store is not
# being mutated while we move bytes into it. We do NOT force-stop the
# `cli` one-shot service because it is restart: "no".
stop_services() {
  log "stopping docker compose services (worker, mcp)"
  ( cd "$ROOT_DIR" && docker compose stop worker mcp ) >/dev/null
}
start_services() {
  log "starting docker compose services (worker, mcp)"
  ( cd "$ROOT_DIR" && docker compose up -d worker mcp ) >/dev/null
}

cleanup() {
  if [ "$KEEP_SERVICES" -ne 0 ]; then return; fi
  if [ "${SERVICES_STOPPED:-0}" -eq 1 ]; then
    start_services || echo "warning: failed to restart services after restore" >&2
  fi
}
trap cleanup EXIT

if [ "$MODE" = "compose" ]; then
  if ! command -v docker >/dev/null 2>&1; then
    echo "docker not on PATH — use --local or install Docker" >&2
    exit 1
  fi
  if [ "$KEEP_SERVICES" -eq 0 ]; then
    stop_services
    SERVICES_STOPPED=1
  else
    log "--keep-services: NOT stopping worker/mcp; the restore may FAIL if the store is live"
  fi

  log "docker compose run --rm cli memento tenant restore $BACKUP_DIR"
  output="$(cd "$ROOT_DIR" && \
    MEMENTO_ROOT="$MEMENTO_ROOT" MEMENTO_TOKEN="$MEMENTO_TOKEN" \
    MEMENTO_AGENT_ID="${MEMENTO_AGENT_ID:-cli}" \
    docker compose run --rm cli memento tenant restore "$BACKUP_DIR" 2>&1)"
  rc=$?
  printf '%s\n' "$output"
  if [ "$rc" -ne 0 ]; then
    echo "restore FAILED (memento exit=$rc, REQ-OP-002)" >&2
    # cleanup trap will restart services
    exit 1
  fi
else
  if ! command -v memento >/dev/null 2>&1; then
    echo "memento not on PATH — install it or use docker compose mode" >&2
    exit 1
  fi
  log "memento --root $MEMENTO_ROOT tenant restore $BACKUP_DIR"
  if ! MEMENTO_TOKEN="$MEMENTO_TOKEN" MEMENTO_AGENT_ID="${MEMENTO_AGENT_ID:-cli}" \
       memento --root "$MEMENTO_ROOT" tenant restore "$BACKUP_DIR"; then
    echo "restore FAILED (memento exit=$?, REQ-OP-002)" >&2
    exit 1
  fi
fi

log "OK"
log "the next 'memento tenant sweep' will honor any tenant config changes"
