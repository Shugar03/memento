#!/usr/bin/env bash
# scripts/backup.sh — operator-facing backup helper (T-111).
#
# Wraps `memento tenant backup` (compact → copy → encrypt with per-backup
# AES-256-GCM key wrapped by the tenant master key, D4 / REQ-ML-005) and
# writes a one-line, timestamped log line suitable for ops pipelines.
#
# Fail-loud contract (REQ-OP-002): any non-zero exit from `memento` or
# any pre-condition failure returns 1 with a clear stderr message.
#
# Usage:
#   scripts/backup.sh                       # backup via docker compose
#   scripts/backup.sh --local               # backup via local memento binary
#   scripts/backup.sh --root /srv/memento   # override --root / MEMENTO_ROOT
#
# Env:
#   MEMENTO_TOKEN      required (read on the host, passed to the container)
#   MEMENTO_AGENT_ID   optional, default `cli`
#   MEMENTO_ROOT       default ~/.memento (matches docker-compose)

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

MODE="compose"
MEMENTO_ROOT="${MEMENTO_ROOT:-$HOME/.memento}"

usage() {
  sed -n '2,18p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit 0
}

for arg in "$@"; do
  case "$arg" in
    --local)       MODE="local" ;;
    --root)        shift; MEMENTO_ROOT="${1:?--root needs a path}" ;;
    --root=*)      MEMENTO_ROOT="${arg#--root=}" ;;
    -h|--help)     usage ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done

ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
log() { printf '[backup %s] %s\n' "$ts" "$*"; }

if [ -z "${MEMENTO_TOKEN:-}" ]; then
  echo "MEMENTO_TOKEN is not set — export it before running backup.sh (REQ-TA-002)" >&2
  exit 1
fi

# Pre-flight: tenant root must exist (otherwise `memento` returns AUTH_FAILED).
if [ ! -d "$MEMENTO_ROOT/db/tenants" ] && [ ! -d "$MEMENTO_ROOT/tenants" ]; then
  echo "no tenant directory under $MEMENTO_ROOT — run 'memento tenant create' first" >&2
  exit 1
fi

if [ "$MODE" = "compose" ]; then
  if ! command -v docker >/dev/null 2>&1; then
    echo "docker not on PATH — use --local or install Docker" >&2
    exit 1
  fi
  log "docker compose run --rm cli memento tenant backup"
  output="$(cd "$ROOT_DIR" && \
    MEMENTO_ROOT="$MEMENTO_ROOT" MEMENTO_TOKEN="$MEMENTO_TOKEN" \
    MEMENTO_AGENT_ID="${MEMENTO_AGENT_ID:-cli}" \
    docker compose run --rm cli memento tenant backup 2>&1)"
  rc=$?
  printf '%s\n' "$output"
  if [ "$rc" -ne 0 ]; then
    echo "backup FAILED (memento exit=$rc, REQ-OP-002)" >&2
    exit 1
  fi
else
  if ! command -v memento >/dev/null 2>&1; then
    echo "memento not on PATH — install it or use docker compose mode" >&2
    exit 1
  fi
  log "memento --root $MEMENTO_ROOT tenant backup"
  if ! MEMENTO_TOKEN="$MEMENTO_TOKEN" MEMENTO_AGENT_ID="${MEMENTO_AGENT_ID:-cli}" \
       memento --root "$MEMENTO_ROOT" tenant backup; then
    echo "backup FAILED (memento exit=$?, REQ-OP-002)" >&2
    exit 1
  fi
fi

log "OK"
log "artifacts: $MEMENTO_ROOT/backups/<tid>/<ts>/{backup.enc,backup.key.json,manifest.json}"
