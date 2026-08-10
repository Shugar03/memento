# scripts/audit-prep.ps1 — Windows equivalent of scripts/audit-prep.sh
# (T-107 in batch 11).
#
# Same contract: installs `cargo-audit` and `cargo-geiger` on demand,
# runs both, prints a one-page summary, fails loudly (exit 1) on any
# known advisory.
#
# Usage:
#   scripts\audit-prep.ps1
#   scripts\audit-prep.ps1 -Archive   # also copies outputs to
#                                    # audit-evidence\<date>\

[CmdletBinding()]
param(
    [switch]$Archive
)

$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\dev-setup.ps1"

$work = [System.IO.Path]::GetTempFileName()
$auditJson  = "$work.audit"
$auditErr   = "$work.audit.err"
$geigerJson = "$work.geiger"
$geigerErr  = "$work.geiger.err"

function Install-IfMissing {
    param([string]$Tool, [string]$Crate)
    if (Get-Command $Tool -ErrorAction SilentlyContinue) { return }
    Write-Host "== installing $Tool (one-time)"
    & cargo install $Crate --locked
    if ($LASTEXITCODE -ne 0) {
        throw "failed to install $Tool"
    }
}

Install-IfMissing -Tool 'cargo-audit' -Crate 'cargo-audit'
Install-IfMissing -Tool 'cargo-geiger' -Crate 'cargo-geiger'

Write-Host '== cargo audit (advisory DB scan)'
$auditOk = $true
try {
    & cargo audit --json *> $auditJson
} catch {
    $auditOk = $false
}

$advisoryCount = 'unknown'
if (Test-Path $auditJson) {
    try {
        $j = Get-Content $auditJson -Raw | ConvertFrom-Json
        if ($j.vulnerabilities.found) {
            $advisoryCount = [string]$j.vulnerabilities.found
        } elseif ($j.vulnerabilities.count) {
            $advisoryCount = [string]$j.vulnerabilities.count
        } elseif ($j.vulnerabilities) {
            $advisoryCount = [string]$j.vulnerabilities.Length
        } else {
            $advisoryCount = '0'
        }
    } catch {
        $advisoryCount = 'unknown'
    }
}

$auditStatus = switch ($advisoryCount) {
    '0'       { 'pass' }
    'unknown' { 'unknown' }
    default   { 'fail' }
}

Write-Host '== cargo geiger (unsafe-code surface)'
$geigerOk = $true
try {
    & cargo geiger --all-features --output-format Json *> $geigerJson
} catch {
    $geigerOk = $false
}

$unsafeTotal = 'unknown'
$unsafeExpr  = 'unknown'
if (Test-Path $geigerJson) {
    try {
        $j = Get-Content $geigerJson -Raw | ConvertFrom-Json
        $unsafeTotal = ($j.'packages' |
            ForEach-Object { $_.functions.unsafe_count } |
            Where-Object { $_ } |
            Measure-Object -Sum).Sum
        $unsafeExpr  = ($j.'packages' |
            ForEach-Object { $_.functions.unsafe_exprs } |
            Where-Object { $_ } |
            Measure-Object -Sum).Sum
    } catch {
        # leave 'unknown'
    }
}

Write-Host ''
Write-Host '== audit-prep summary =='
'{0,-22} {1}' -f 'cargo-audit',       "$auditStatus (advisories: $advisoryCount)"
'{0,-22} {1}' -f 'cargo-geiger-fn',   "$unsafeTotal unsafe fn calls in deps"
'{0,-22} {1}' -f 'cargo-geiger-expr', "$unsafeExpr unsafe exprs in deps"

if ($Archive) {
    $ts = Get-Date -Format 'yyyyMMdd-HHmmss'
    $dest = Join-Path (Split-Path $PSScriptRoot -Parent) "audit-evidence\$ts"
    New-Item -ItemType Directory -Path $dest -Force | Out-Null
    Copy-Item $auditJson  "$dest\cargo-audit.json"
    if (Test-Path $auditErr) { Copy-Item $auditErr  "$dest\cargo-audit.stderr" }
    Copy-Item $geigerJson "$dest\cargo-geiger.json"
    if (Test-Path $geigerErr) { Copy-Item $geigerErr "$dest\cargo-geiger.stderr" }
    Write-Host "   archived to $dest\"
}

Remove-Item $work, $auditJson, $auditErr, $geigerJson, $geigerErr -ErrorAction SilentlyContinue

switch ($auditStatus) {
    'pass' { exit 0 }
    default { exit 1 }
}
