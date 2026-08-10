# scripts/dev-clippy.ps1
#
# Run the workspace-wide clippy gate (T-106). The `-D warnings` flag
# turns every warning into an error; CI uses the same invocation.
#
# `--all-targets` covers bins, examples, tests, benches. `--all-features`
# activates every optional feature so dead-code from feature gates is
# flagged.

[CmdletBinding()]
param(
    [string]$Package = ''
)

$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\dev-setup.ps1"

$targets = @('--workspace', '--all-targets', '--all-features', '-j', '2', '--', '-D', 'warnings')
if ($Package) {
    $args = @('clippy', '-p', $Package) + $targets
} else {
    $args = @('clippy') + $targets
}

& cargo @args
exit $LASTEXITCODE
