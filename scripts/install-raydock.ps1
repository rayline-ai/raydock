# Canonical repository entrypoint; release hosts publish the shared script bytes.
param(
    [string]$Version,
    [string]$InstallDir
)
& (Join-Path $PSScriptRoot 'install-rayline.ps1') @PSBoundParameters
