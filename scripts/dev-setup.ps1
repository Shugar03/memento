# scripts/dev-setup.ps1
#
# Export the Memento RS toolchain environment (T-106).
#
# The dev host has no MSVC linker (C: drive nearly full, winget fails
# to install VS Build Tools); we use windows-gnu + w64devkit. Run this
# script ONCE PER SHELL before any `cargo` invocation.
#
# Usage:
#   . .\scripts\dev-setup.ps1        # dot-source (variables persist)
#   scripts\dev-setup.ps1            # invoke (variables set in child only)
#
# The same recipe is documented in docs/development.es.md and
# docs/development.en.md; the script is the single source of truth.

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$toolsRoot = Split-Path -Parent $repoRoot
$w64 = Join-Path $toolsRoot '.toolchains\w64devkit'
$protoc = Join-Path $toolsRoot '.toolchains\protoc'

$env:RUSTUP_HOME = Join-Path $toolsRoot '.toolchains\rustup'
$env:CARGO_HOME  = Join-Path $toolsRoot '.toolchains\cargo'

# w64devkit ships MinGW; LIBRARY_PATH points the GNU linker at libgcc_eh.a.
$gccLib = Join-Path $w64 'lib\gcc\x86_64-w64-mingw32\16.1.0'
if (Test-Path $gccLib) {
    $env:LIBRARY_PATH = $gccLib
}

# Prepend cargo + w64devkit + protoc to PATH so cargo finds link.exe
# (actually ld.exe from w64devkit) and protoc.
$env:Path = @(
    "$env:USERPROFILE\.cargo\bin",
    (Join-Path $w64 'bin'),
    (Join-Path $protoc 'bin'),
    $env:Path
) -join [System.IO.Path]::PathSeparator

Write-Host 'Memento RS dev environment loaded:'
Write-Host "  RUSTUP_HOME    = $env:RUSTUP_HOME"
Write-Host "  CARGO_HOME     = $env:CARGO_HOME"
Write-Host "  LIBRARY_PATH   = $env:LIBRARY_PATH"
Write-Host "  cargo          = $(Get-Command cargo -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Source)"
