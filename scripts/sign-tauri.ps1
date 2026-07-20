# Signs a single Windows binary using the OxideLink self-signed PFX.
# Called by Tauri (bundle.windows.signCommand) and by build-release.ps1.
param(
    [Parameter(Mandatory = $true)]
    [string]$Path,

    [string]$TimestampServer = 'http://timestamp.digicert.com',
    [string]$Digest = 'sha256'
)

$ErrorActionPreference = 'Stop'

$projectRoot = Split-Path -Parent $PSScriptRoot
$pfx         = Join-Path $projectRoot 'src-tauri\certs\oxidelink.pfx'

if (-not (Test-Path $pfx)) {
    throw "PFX not found: $pfx"
}
if (-not $env:OXIDELINK_PFX_PASSWORD) {
    throw "Environment variable OXIDELINK_PFX_PASSWORD is not set. Set it to the PFX password and retry."
}

$password = $env:OXIDELINK_PFX_PASSWORD

# Skip signing placeholder/empty resources that Tauri may copy into the bundle.
$size = (Get-Item $Path -ErrorAction SilentlyContinue).Length
if ($size -eq 0) {
    Write-Host "Skipping zero-byte file: $Path"
    exit 0
}

# Locate signtool.exe
$signTool = $null
$kitPaths = @(
    "${env:ProgramFiles(x86)}\Windows Kits\10\bin\10.0.26100.0\x64\signtool.exe",
    "${env:ProgramFiles(x86)}\Windows Kits\10\bin\10.0.22621.0\x64\signtool.exe",
    "${env:ProgramFiles(x86)}\Windows Kits\10\bin\10.0.22000.0\x64\signtool.exe",
    "${env:ProgramFiles(x86)}\Windows Kits\10\bin\10.0.19041.0\x64\signtool.exe",
    "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe"
)
foreach ($p in $kitPaths) {
    $candidates = Get-Item $p -ErrorAction SilentlyContinue | Sort-Object FullName -Descending
    if ($candidates) {
        $signTool = $candidates[0].FullName
        break
    }
}
if (-not $signTool) {
    $cmd = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($cmd) { $signTool = $cmd.Source }
}
if (-not $signTool) {
    throw "signtool.exe not found. Install the Windows SDK or add it to PATH."
}

Write-Host "Signing $Path with $signTool"
& $signTool sign /f $pfx /p $password /tr $TimestampServer /td $Digest /fd $Digest "$Path"
if ($LASTEXITCODE -ne 0) {
    throw "signtool failed with exit code $LASTEXITCODE"
}
Write-Host "Signed: $Path"
