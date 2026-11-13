param(
    [string]$Version,
    [string]$InstallDir = $(Join-Path $HOME ".rayline\bin")
)

$ErrorActionPreference = "Stop"
$Repo = "rayline-ai/raydock"

function Fail($Message) {
    Write-Error $Message
    exit 1
}

function Remove-RetiredBackup {
    param([string]$Path)
    try {
        Remove-Item -LiteralPath $Path -Force
    } catch {
        if ($_.Exception -is [System.Management.Automation.ItemNotFoundException]) { return }
        Write-Warning "Retained update backup ${Path}: $($_.Exception.Message). A later update or installer will retry cleanup."
    }
}

function Remove-RetiredBackups {
    param([string]$Directory)
    # A dot .old name marks a committed replacement. .tmp files can still be
    # needed for recovery.
    $pattern = '^\.(raydock\.exe|rayline\.exe|rayline-core\.exe|rayline-core-LICENSE)\.old-[0-9]+-[0-9a-f]+$'
    try {
        Get-ChildItem -LiteralPath $Directory -File -Force | Where-Object { $_.Name -match $pattern } | ForEach-Object {
            Remove-RetiredBackup -Path $_.FullName
        }
    } catch {
        Write-Warning "Could not finish backup cleanup in ${Directory}: $($_.Exception.Message). Continuing installation; a later update or installer will retry."
    }
}

function Install-StagedFile {
    param([string]$Staged, [string]$Dest)
    # Windows refuses to overwrite a running executable but allows renaming
    # it, so move the current file aside before renaming the staged one in.
    $Dir = Split-Path -Parent $Dest
    $Name = Split-Path -Leaf $Dest
    $Nonce = [Guid]::NewGuid().ToString('N')
    $Backup = $null
    if (Test-Path -LiteralPath $Dest) {
        $Backup = Join-Path $Dir ".$Name.tmp-$PID-$Nonce"
        try { [System.IO.File]::Move($Dest, $Backup) }
        catch { throw "Failed to install ${Dest}: could not move the current file aside: $($_.Exception.Message)" }
    }
    try {
        # File.Move refuses an existing target rather than overwriting it.
        [System.IO.File]::Move($Staged, $Dest)
    } catch {
        $InstallError = $_.Exception.Message
        if ($Backup) {
            try { [System.IO.File]::Move($Backup, $Dest) }
            catch { throw "Failed to install ${Dest}: $InstallError. Restore also failed: $($_.Exception.Message). Original preserved at $Backup" }
        }
        throw "Failed to install ${Dest}: $InstallError"
    }
    if ($Backup) {
        $Retired = Join-Path $Dir ".$Name.old-$PID-$Nonce"
        try { [System.IO.File]::Move($Backup, $Retired) }
        catch { Write-Warning "Installed ${Dest}; retained previous file at ${Backup}: $($_.Exception.Message). Remove it manually after older sessions and installers exit." }
        # The previous image can still be running. A later installer or update
        # deletes committed .old backups; this invocation only marks them.
    }
}

function Install-File {
    param([string]$Source, [string]$Dest)
    # Stage next to the destination so the swap is a rename on one volume.
    $Staged = Join-Path (Split-Path -Parent $Dest) ".$(Split-Path -Leaf $Dest).tmp-$PID-$([Guid]::NewGuid().ToString('N'))"
    try {
        Copy-Item -LiteralPath $Source -Destination $Staged
        Install-StagedFile -Staged $Staged -Dest $Dest
    } finally {
        if (Test-Path -LiteralPath $Staged) { Remove-Item -LiteralPath $Staged -Force }
    }
}

$arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
switch ($arch) {
    "X64" { $Platform = "windows_x86_64" }
    # Only x64 binaries are published; Windows on ARM runs them under x64
    # emulation.
    "Arm64" { $Platform = "windows_x86_64" }
    default { Fail "unsupported Windows architecture: $arch" }
}

if (-not (Get-Command minisign -ErrorAction SilentlyContinue)) {
    Fail "required command not found: minisign, which verifies the release signature. Install it: https://jedisct1.github.io/minisign/"
}

if (-not $PSBoundParameters.ContainsKey('Version')) {
    $BaseUrl = "https://github.com/$Repo/releases/latest/download"
} elseif ($Version.StartsWith('v')) {
    Fail "-Version takes a bare version such as 0.2.0, not $Version"
} else {
    $BaseUrl = "https://github.com/$Repo/releases/download/v$Version"
}

$RaylineAsset = "rayline-$Platform"
$CoreAsset = "rayline-core-$Platform"
$TempDir = Join-Path ([System.IO.Path]::GetTempPath()) "rayline-install-$PID-$([System.Guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Force -Path $TempDir | Out-Null

try {
    $SumsPath = Join-Path $TempDir "SHA256SUMS"
    $SigPath = Join-Path $TempDir "SHA256SUMS.minisig"
    $RaylinePath = Join-Path $TempDir $RaylineAsset
    $CorePath = Join-Path $TempDir $CoreAsset
    $LicensePath = Join-Path $TempDir "rayline-core-LICENSE"

    Invoke-WebRequest -Uri "$BaseUrl/SHA256SUMS" -OutFile $SumsPath
    Invoke-WebRequest -Uri "$BaseUrl/SHA256SUMS.minisig" -OutFile $SigPath

    # rayline production signing public key (matches MINISIGN_PUBLIC_KEYS in rayline-cli; see RELEASING-SIGNING.md).
    $PublicKey = "RWRKGvuHHJS76PGzxmnM/1NX8SFhTi3mPj/axsIjv/Ehnw71G4Ei9xb1"
    & minisign -Vm $SumsPath -P $PublicKey *> $null
    if ($LASTEXITCODE -ne 0) {
        Fail "SHA256SUMS signature verification failed. The release may be tampered."
    }
    Write-Host "SHA256SUMS signature verified."

    Invoke-WebRequest -Uri "$BaseUrl/$RaylineAsset" -OutFile $RaylinePath
    Invoke-WebRequest -Uri "$BaseUrl/$CoreAsset" -OutFile $CorePath
    Invoke-WebRequest -Uri "$BaseUrl/rayline-core-LICENSE" -OutFile $LicensePath

    # Each line is "<sha256>  <name>", as the release's sha256sum writes it.
    $Entries = foreach ($Line in Get-Content $SumsPath) {
        if ($Line -cnotmatch '^([0-9a-f]{64})  (\S+)$') {
            Fail "malformed line in release checksums: $Line"
        }
        [pscustomobject]@{ Name = $Matches[2]; Hash = $Matches[1] }
    }
    $Expected = @{}
    foreach ($Asset in @($RaylineAsset, $CoreAsset, "rayline-core-LICENSE")) {
        $Found = @($Entries | Where-Object { $_.Name -eq $Asset })
        if ($Found.Count -ne 1) {
            Fail "release checksums must include exactly one entry for $Asset"
        }
        $Expected[$Asset] = $Found[0].Hash
        $Path = Join-Path $TempDir $Asset
        $Actual = (Get-FileHash -Algorithm SHA256 -Path $Path).Hash.ToLowerInvariant()
        if ($Actual -ne $Expected[$Asset]) {
            Fail "sha256 mismatch for $Asset; expected $($Expected[$Asset]), got $Actual"
        }
    }

    # A full path: the .NET file calls below resolve relative paths against the
    # process directory, not the PowerShell location.
    $InstallDir = (New-Item -ItemType Directory -Force -Path $InstallDir).FullName
    Remove-RetiredBackups $InstallDir
    Install-File -Source $LicensePath -Dest (Join-Path $InstallDir "rayline-core-LICENSE")
    Install-File -Source $RaylinePath -Dest (Join-Path $InstallDir "raydock.exe")
    $alias = Join-Path $InstallDir 'rayline.exe'
    $AliasStaged = Join-Path $InstallDir ".rayline.exe.tmp-$PID-$([Guid]::NewGuid().ToString('N'))"
    try {
        # Hard links need no administrator or Developer Mode privileges. The new
        # launcher turns this initial alias into a forwarding EXE on first use.
        New-Item -ItemType HardLink -Path $AliasStaged -Target (Join-Path $InstallDir 'raydock.exe') | Out-Null
        Install-StagedFile -Staged $AliasStaged -Dest $alias
    } finally {
        if (Test-Path -LiteralPath $AliasStaged) { Remove-Item -LiteralPath $AliasStaged -Force }
    }
    Install-File -Source $CorePath -Dest (Join-Path $InstallDir "rayline-core.exe")

    Write-Host "Installed Raydock to $InstallDir"
    $PathEntries = $env:PATH -split [System.IO.Path]::PathSeparator
    if ($PathEntries -notcontains $InstallDir) {
        Write-Host ""
        Write-Host "Add Raydock to your PATH:"
        Write-Host "  `$env:PATH = `"$InstallDir;$env:PATH`""
    }
} finally {
    Remove-Item -Recurse -Force $TempDir -ErrorAction SilentlyContinue
}
