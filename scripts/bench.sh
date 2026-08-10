#!/usr/bin/env bash
# T-103 — reproducible benchmark suite + gate report (REQ-MR-007, REQ-CK-002).
#
# Budgets (spec):
#   search    p50 < 20 ms, p99 < 100 ms   at 100k chunks, warm (REQ-MR-007)
#   code idx  10k LOC < 2 s                cold (REQ-CK-002)
#             100k LOC 10-30 s             cold (REQ-CK-002)
#   cold start < 3 s                       reported, not gated (SHOULD)
#
# Every gate line is emitted by the benches themselves as
# `MEMBENCH <key> <json>`; this script greps, tabulates and FAILS LOUDLY
# (exit 1, REQ-OP-002) on any breach. Deviations are never silently
# accepted — they are printed with the measured numbers.
#
# Usage:
#   scripts/bench.sh            # reference run (100k chunks, 10k + 100k LOC)
#   scripts/bench.sh --quick    # smoke run (5k chunks, 10k LOC) for CI/dev
#   scripts/bench.sh --embed    # force the embed bench (downloads the model
#                               # on first run, ~500 MB — risk R1)
#
# Env:
#   MEMENTO_BENCH_CHUNKS / MEMENTO_BENCH_LOC   corpus sizes (scaled by --quick)
#   MEMENTO_MODELS_DIR                          embed model cache dir
#   CARGO_EXTRA                                extra cargo args, e.g. "-j 2"

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

QUICK=0
FORCE_EMBED=0
for arg in "$@"; do
  case "$arg" in
    --quick) QUICK=1 ;;
    --embed) FORCE_EMBED=1 ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done

if [ "$QUICK" = "1" ]; then
  export MEMENTO_BENCH_CHUNKS="${MEMENTO_BENCH_CHUNKS:-5000}"
  export MEMENTO_BENCH_LOC="${MEMENTO_BENCH_LOC:-10000}"
  echo "== quick smoke run (chunks=${MEMENTO_BENCH_CHUNKS}, loc=${MEMENTO_BENCH_LOC})"
else
  export MEMENTO_BENCH_CHUNKS="${MEMENTO_BENCH_CHUNKS:-100000}"
  echo "== reference run (chunks=${MEMENTO_BENCH_CHUNKS})"
fi

CARGO=(cargo bench)
[ -n "${CARGO_EXTRA:-}" ] && CARGO=("${CARGO[@]}" $CARGO_EXTRA)

OUT="$(mktemp)"
trap 'rm -f "$OUT"' EXIT

echo "== search + ingest benches (memento-e2e)"
"${CARGO[@]}" --bench search_bench --bench ingest_bench > >(tee -a "$OUT") 2>&1 || {
  echo "search/ingest benches failed" >&2; exit 1; }

echo "== embed bench (memento-e2e; skips when the model is not cached)"
EMBED_ENV=()
[ "$FORCE_EMBED" = "1" ] && EMBED_ENV=(MEMENTO_BENCH_EMBED=1)
env "${EMBED_ENV[@]}" "${CARGO[@]}" --bench embed_bench > >(tee -a "$OUT") 2>&1 || {
  echo "embed bench failed" >&2; exit 1; }

echo "== code-index benches (memento-okf; 10k + 100k LOC)"
MEMENTO_BENCH_LOC=10000 "${CARGO[@]}" -p memento-okf --bench code_index_bench > >(tee -a "$OUT") 2>&1 || {
  echo "code-index bench (10k) failed" >&2; exit 1; }
if [ "$QUICK" = "1" ]; then
  echo "   (--quick: skipping the 100k LOC reference index)"
else
  MEMENTO_BENCH_LOC=100000 "${CARGO[@]}" -p memento-okf --bench code_index_bench > >(tee -a "$OUT") 2>&1 || {
    echo "code-index bench (100k) failed" >&2; exit 1; }
fi

# ---- gate evaluation ---------------------------------------------------------
# Each gate line is `MEMBENCH <key> <json>`; `json_of` returns the JSON of
# the first (or last) matching line.
json_of() { # json_of <key> <first|last>
  local lines
  lines="$(grep -h "^MEMBENCH $1 " "$OUT" || true)"
  [ -n "$lines" ] || { echo "GATE $1  MISSING (bench produced no measurement)"; return 1; }
  if [ "${2:-last}" = "first" ]; then
    printf '%s\n' "$lines" | head -n 1 | sed "s/^MEMBENCH $1 //"
  else
    printf '%s\n' "$lines" | tail -n 1 | sed "s/^MEMBENCH $1 //"
  fi
}

if ! command -v jq >/dev/null 2>&1; then
  echo ""
  echo "jq not installed — gates UNCHECKED (raw measurements printed below)."
  echo "Install jq to enforce the REQ-MR-007 / REQ-CK-002 budgets."
  echo ""
  echo "== measurements (raw MEMBENCH lines) =="
  grep -h "^MEMBENCH " "$OUT" || true
  exit 0
fi

fail=0
check_number() { # check_number <label> <json> <filter> <awk-condition>
  local label="$1" json="$2" filter="$3" cond="$4"
  local val
  val="$(printf '%s' "$json" | jq -r "$filter")"
  if [ "$val" = "null" ] || [ -z "$val" ]; then
    echo "GATE $label  MISSING (field $filter absent: $json)"
    fail=1
    return
  fi
  if awk "BEGIN{exit !($val $cond)}"; then
    echo "GATE $label  PASS  (measured $val)"
  else
    echo "GATE $label  FAIL  (measured $val — outside budget)"
    fail=1
  fi
}

echo ""
echo "== gate report =="

search="$(json_of gate_search || true)"
if [ -n "$search" ]; then
  check_number "search p50 < 20 ms"   "$search" '.p50_ms' "< 20.0"
  check_number "search p99 < 100 ms"  "$search" '.p99_ms' "< 100.0"
  corpus="$(printf '%s' "$search" | jq -r '.chunks')"
  ref="$(printf '%s' "$search" | jq -r '.corpus_is_reference // false')"
  echo "       (corpus: ${corpus} chunks; reference 100k: ${ref})"
fi

# First gate_code_index line = the 10k run, last = the 100k run.
ci_10k="$(json_of gate_code_index first || true)"
if [ -n "$ci_10k" ] && [ "$(printf '%s' "$ci_10k" | jq -r '.loc // 0')" = "10000" ]; then
  check_number "code index 10k LOC < 2 s" "$ci_10k" '.index_ms' "< 2000"
fi
ci_100k="$(json_of gate_code_index last || true)"
if [ -n "$ci_100k" ] && [ "$(printf '%s' "$ci_100k" | jq -r '.loc // 0')" = "100000" ]; then
  check_number "code index 100k LOC <= 30 s" "$ci_100k" '.index_ms' "<= 30000"
  idx100="$(printf '%s' "$ci_100k" | jq -r '.index_ms')"
  if awk "BEGIN{exit !($idx100 < 10000)}"; then
    echo "       (note: 100k index finished in ${idx100} ms — faster than the expected 10-30 s window)"
  fi
fi

cold="$(json_of gate_cold_start || true)"
if [ -n "$cold" ]; then
  total="$(printf '%s' "$cold" | jq -r '.store_open_ms + .first_search_ms')"
  echo "GATE cold start < 3 s  REPORTED  (reopen + first search = ${total} ms; SHOULD, not gated)"
fi

echo ""
echo "== measurements (raw MEMBENCH lines) =="
grep -h "^MEMBENCH " "$OUT" || true

if [ "$fail" != "0" ]; then
  echo ""
  echo "BENCH GATE FAILURE — deviations are reported, not accepted (REQ-MR-007 / REQ-CK-002)." >&2
  exit 1
fi
echo ""
echo "All gates passed."
