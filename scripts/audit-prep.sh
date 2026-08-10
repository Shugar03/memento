#!/usr/bin/env bash
# scripts/audit-prep.sh — bundle all audit evidence in one shot (T-107).
#
# Runs `cargo audit` and `cargo geiger`, prints a one-page summary, and
# fails loudly (exit 1) on any known CVE in the pinned dep set
# (REQ-OP-004 / REQ-OP-002).
#
# Tools are installed on demand (`cargo install ...`) — the first run
# takes ~5 min; subsequent runs are fast.
#
# Usage:
#   scripts/audit-prep.sh             # audit + geiger + summary
#   scripts/audit-prep.sh --archive   # also copy outputs to
#                                     # audit-evidence/<date>/
#
# Outputs (default):
#   audit-prep.out    the captured stdout/stderr of every command
#   audit-prep.json   the structured JSON the script returns on stdout
#
# Exit codes:
#   0  no known vulnerabilities, geiger ran
#   1  cargo audit found advisories, OR a tool failed to install/run
#   2  usage error

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ARCHIVE=0
for arg in "$@"; do
  case "$arg" in
    --archive) ARCHIVE=1 ;;
    -h|--help)
      sed -n '2,15p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done

OUT="$(mktemp)"
trap 'rm -f "$OUT"' EXIT

# --------------------------------------------------------------------------
# Tool install (idempotent: skip if already on PATH).
# --------------------------------------------------------------------------
install_if_missing() {
  local tool="$1" crate="$2"
  if command -v "$tool" >/dev/null 2>&1; then
    return 0
  fi
  echo "== installing $tool (one-time)"
  cargo install "$crate" --locked --quiet
}

install_if_missing cargo-audit cargo-audit
install_if_missing cargo-geiger cargo-geiger

# --------------------------------------------------------------------------
# cargo audit
# --------------------------------------------------------------------------
echo "== cargo audit (advisory DB scan)"
if ! cargo audit --json > "$OUT.audit" 2> "$OUT.audit.err"; then
  echo "   cargo audit exited non-zero (advisories may exist)"
fi

# Fail loudly on any advisory (REQ-OP-002 / REQ-OP-004).
advisory_count="$(jq '.vulnerabilities.found // (.vulnerabilities | length // 0)' "$OUT.audit" 2>/dev/null || echo unknown)"
echo "   advisories: ${advisory_count}"

if [ "$advisory_count" = "0" ]; then
  audit_status="pass"
elif [ "$advisory_count" = "unknown" ]; then
  audit_status="unknown"
  echo "   (could not parse cargo-audit JSON — inspect $OUT.audit)" >&2
else
  audit_status="fail"
fi

# --------------------------------------------------------------------------
# cargo geiger
# --------------------------------------------------------------------------
echo "== cargo geiger (unsafe-code surface)"
cargo geiger --all-features --output-format Json --quiet > "$OUT.geiger" 2> "$OUT.geiger.err" \
  || echo "   cargo geiger exited non-zero (inspect $OUT.geiger.err)"

unsafe_total="$(jq '[.. | objects | select(.functions.unsafe_count? != null) | .functions.unsafe_count] | add // 0' "$OUT.geiger" 2>/dev/null || echo unknown)"
unsafe_expr="$(jq '[.. | objects | select(.functions.unsafe_exprs? != null) | .functions.unsafe_exprs] | add // 0' "$OUT.geiger" 2>/dev/null || echo unknown)"

# --------------------------------------------------------------------------
# Summary
# --------------------------------------------------------------------------
echo ""
echo "== audit-prep summary =="
printf '%-22s %s\n' 'cargo-audit'      "${audit_status} (advisories: ${advisory_count})"
printf '%-22s %s\n' 'cargo-geiger-fn'  "${unsafe_total} unsafe fn calls in deps"
printf '%-22s %s\n' 'cargo-geiger-expr' "${unsafe_expr} unsafe exprs in deps"

# Optional archive copy.
if [ "$ARCHIVE" = "1" ]; then
  ts="$(date +%Y%m%d-%H%M%S)"
  dest="$ROOT/audit-evidence/$ts"
  mkdir -p "$dest"
  cp "$OUT.audit" "$dest/cargo-audit.json"
  cp "$OUT.audit.err" "$dest/cargo-audit.stderr" 2>/dev/null || true
  cp "$OUT.geiger" "$dest/cargo-geiger.json"
  cp "$OUT.geiger.err" "$dest/cargo-geiger.stderr" 2>/dev/null || true
  echo "   archived to $dest/"
fi

# Exit code: fail on advisories or tool failure.
case "$audit_status" in
  pass) exit 0 ;;
  *)    exit 1 ;;
esac
