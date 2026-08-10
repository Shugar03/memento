# scripts/dev-format.ps1
#
# Apply `cargo fmt --all` to the workspace (T-106). Without `--check`,
# this rewrites files in place — CI runs with `--check` to fail on
# drift.
#
# Usage:
#   scripts\dev-format.ps1           # apply formatting
#   cargo fmt --all -- --check       # CI equivalent

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\dev-setup.ps1"

& cargo fmt --all
exit $LASTEXITCODE
