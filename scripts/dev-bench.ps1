# scripts/dev-bench.ps1
#
# Run the reproducible benchmark suite (T-106). Delegates to
# scripts/bench.sh (POSIX) — this wrapper sets up the toolchain env
# and passes `-j 2` through CARGO_EXTRA so the link memory cap holds.
#
# On Windows, bash is required (Git for Windows ships bash.exe in
# PATH after `git install`). The wrapper resolves bash explicitly.
#
# Usage:
#   scripts\dev-bench.ps1            # reference run (100k chunks)
#   scripts\dev-bench.ps1 -Quick     # smoke run (5k chunks)

[CmdletBinding()]
param(
    [switch]$Quick,
    [switch]$Embed
)

$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\dev-setup.ps1"

$env:CARGO_EXTRA = '-j 2'

$benchArgs = @()
if ($Quick)  { $benchArgs += '--quick' }
if ($Embed)  { $benchArgs += '--embed' }

$bash = (Get-Command bash.exe -ErrorAction SilentlyContinue).Source
if (-not $bash) {
    Write-Error 'bash.exe not found in PATH (install Git for Windows).'
    exit 1
}

& $bash (Join-Path $PSScriptRoot 'bench.sh') @benchArgs
exit $LASTEXITCODE
