param(
    [string]$Version = "latest",
    [string]$InstallDir = $(Join-Path $HOME ".rayline\bin")
)

$ErrorActionPreference = "Stop"
$Repo = "rayline-ai/rayline"

function Fail($Message) {
    Write-Error $Message
    exit 1
}

function Remove-RetiredBackup($Path) {
    try {
        Remove-Item -LiteralPath $Path -Force
    } catch {
        if ($_.Exception -is [System.Management.Automation.ItemNotFoundException]) { return }
        $Code = $_.Exception.HResult -band 0xffff
        if ($Code -in @(5, 32, 33)) {
            Write-Warning "Retained old executable ${Path}: $($_.Exception.Message). Close older Raydock sessions; a later run will retry cleanup."
        } else { throw }
    }
}

function Remove-RetiredBackups($Directory) {
    # These names are assigned only after a replacement succeeds. Never sweep
    # .tmp files: another installer/updater may still need them for rollback.
    $Pattern = '^\.(raydock\.exe|rayline\.exe|rld\.exe|rayline-core\.exe|rayline-core-LICENSE)\.old-[0-9]+-[0-9a-f]+$'
    Get-ChildItem -LiteralPath $Directory -File -Force | Where-Object { $_.Name -match $Pattern } | ForEach-Object {
        Remove-RetiredBackup $_.FullName
    }
}

$arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
switch ($arch) {
    "X64" { $Platform = "windows_x86_64" }
    default { Fail "unsupported Windows architecture: $arch" }
}

if ($env:RAYLINE_RELEASE_BASE_URL) {
    $BaseUrl = $env:RAYLINE_RELEASE_BASE_URL.TrimEnd("/")
} elseif ($Version -eq "latest") {
    $BaseUrl = "https://github.com/$Repo/releases/latest/download"
} else {
    $Tag = "v$($Version.TrimStart('v'))"
    $BaseUrl = "https://github.com/$Repo/releases/download/$Tag"
}

$RaylineAsset = "rayline-$Platform"
$DaemonAsset = "rld-$Platform"
$CoreAsset = "rayline-core-$Platform"
$TempDir = Join-Path ([System.IO.Path]::GetTempPath()) "rayline-install-$PID-$([System.Guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Force -Path $TempDir | Out-Null

try {
    $SumsPath = Join-Path $TempDir "SHA256SUMS"
    $RaylinePath = Join-Path $TempDir $RaylineAsset
    $DaemonPath = Join-Path $TempDir $DaemonAsset
    $CorePath = Join-Path $TempDir $CoreAsset
    $LicensePath = Join-Path $TempDir "rayline-core-LICENSE"

    Invoke-WebRequest -Uri "$BaseUrl/SHA256SUMS" -OutFile $SumsPath
    Invoke-WebRequest -Uri "$BaseUrl/$RaylineAsset" -OutFile $RaylinePath
    Invoke-WebRequest -Uri "$BaseUrl/$DaemonAsset" -OutFile $DaemonPath
    Invoke-WebRequest -Uri "$BaseUrl/$CoreAsset" -OutFile $CorePath
    Invoke-WebRequest -Uri "$BaseUrl/rayline-core-LICENSE" -OutFile $LicensePath

    $Expected = @{}
    foreach ($Line in Get-Content $SumsPath) {
        $Parts = $Line.Trim() -split "\s+"
        if ($Parts.Length -ge 2) {
            $Expected[$Parts[1].TrimStart("*")] = $Parts[0].ToLowerInvariant()
        }
    }

    foreach ($Asset in @($RaylineAsset, $DaemonAsset, $CoreAsset, "rayline-core-LICENSE")) {
        if (-not $Expected.ContainsKey($Asset)) {
            Fail "release checksums do not include $Asset"
        }
        $Path = Join-Path $TempDir $Asset
        $Actual = (Get-FileHash -Algorithm SHA256 -Path $Path).Hash.ToLowerInvariant()
        if ($Actual -ne $Expected[$Asset]) {
            Fail "sha256 mismatch for $Asset; expected $($Expected[$Asset]), got $Actual"
        }
    }

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Remove-RetiredBackups $InstallDir
    Copy-Item -Force $LicensePath (Join-Path $InstallDir "rayline-core-LICENSE")
    Copy-Item -Force $RaylinePath (Join-Path $InstallDir "raydock.exe")
    $alias = Join-Path $InstallDir 'rayline.exe'
    $AliasBackup = $null
    $AliasNonce = [Guid]::NewGuid().ToString('N')
    if (Test-Path -LiteralPath $alias) {
        $AliasBackup = Join-Path $InstallDir ".rayline.exe.tmp-$PID-$AliasNonce"
        Move-Item -LiteralPath $alias -Destination $AliasBackup
    }
    # Hard links need no administrator or Developer Mode privileges. The new
    # launcher turns this initial alias into a forwarding EXE on first use.
    try {
        New-Item -ItemType HardLink -Path $alias -Target (Join-Path $InstallDir 'raydock.exe') | Out-Null
    } catch {
        $InstallError = $_
        if ($AliasBackup) {
            try { Move-Item -LiteralPath $AliasBackup -Destination $alias }
            catch { throw "Failed to install alias: $InstallError. Restore also failed: $_. Original preserved at $AliasBackup" }
        }
        throw $InstallError
    }
    if ($AliasBackup) {
        $Retired = Join-Path $InstallDir ".rayline.exe.old-$PID-$AliasNonce"
        Move-Item -LiteralPath $AliasBackup -Destination $Retired
        Remove-RetiredBackup $Retired
    }
    Copy-Item -Force $DaemonPath (Join-Path $InstallDir "rld.exe")
    Copy-Item -Force $CorePath (Join-Path $InstallDir "rayline-core.exe")

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
