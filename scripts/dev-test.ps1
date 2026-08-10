# scripts/dev-test.ps1
#
# Run the full workspace test suite with the w64devkit-safe flags (T-106).
#
# `-j 2` serializes link jobs so w64devkit's ld does not run out of
# memory on parallel test-binary links. `--test-threads=1` removes the
# last bit of cross-test contention (LanceDB tempdirs are per-test but
# ONNX model init can race under high parallelism).
#
# Usage:
#   scripts\dev-test.ps1                # all crates
#   scripts\dev-test.ps1 -p memento-application   # one crate

[CmdletBinding()]
param(
    [string]$Package = ''
)

$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\dev-setup.ps1"

$args = @('test', '--workspace', '-j', '2', '--', '--test-threads=1')
if ($Package) {
    # Drop --workspace when a single crate is requested.
    $args = @('test', '-p', $Package, '-j', '2', '--', '--test-threads=1')
}

& cargo @args
exit $LASTEXITCODE
