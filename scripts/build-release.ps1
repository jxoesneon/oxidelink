# OxideLink release build script.
# Runs `npm run tauri build` and signs any output artifacts that are not already signed.
param(
    [string]$ProjectRoot = (Split-Path -Parent $PSScriptRoot)
)

$ErrorActionPreference = 'Stop'

$signScript = Join-Path $ProjectRoot 'scripts\sign-tauri.ps1'

# Code-signing PFX password must be supplied via environment variable.
if (-not $env:OXIDELINK_PFX_PASSWORD) {
    throw "Environment variable OXIDELINK_PFX_PASSWORD is not set. Set it to the PFX password and retry."
}

# Tauri updater signing.  CI should set these env vars from secrets; for local builds fall back to
# the generated keypair in src-tauri\target (which is gitignored).
if (-not $env:TAURI_SIGNING_PRIVATE_KEY) {
    $keyFile = Join-Path $ProjectRoot 'src-tauri\target\tauri-sign-pwd.key'
    $passFile = Join-Path $ProjectRoot 'src-tauri\target\tauri-sign-pwd.pass'
    if (Test-Path $keyFile) {
        $env:TAURI_SIGNING_PRIVATE_KEY = (Get-Content $keyFile -Raw).Trim()
        if (Test-Path $passFile) {
            $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = (Get-Content $passFile -Raw).Trim()
        }
    }
}

function Test-SignedFile {
    param([string]$FilePath)
    try {
        $sig = Get-AuthenticodeSignature -FilePath $FilePath
        return $sig.Status -ne 'NotSigned'
    } catch {
        return $false
    }
}

function Invoke-SignIfNeeded {
    param([string]$FilePath)
    if (Test-SignedFile $FilePath) {
        Write-Host "Already signed: $FilePath"
    } else {
        Write-Host "Signing: $FilePath"
        & $signScript -Path $FilePath
    }
}

# Run Tauri build.  Use Start-Process so PowerShell's $ErrorActionPreference does not
# treat `npm`/`tauri` stderr progress lines as terminating errors.
Push-Location $ProjectRoot
try {
    $process = Start-Process -FilePath 'cmd.exe' -ArgumentList '/c','npm run tauri build' -NoNewWindow -Wait -PassThru
    if ($process.ExitCode -ne 0) {
        throw "npm run tauri build failed with exit code $($process.ExitCode)"
    }
} finally {
    Pop-Location
}

$bundleRoot = Join-Path $ProjectRoot 'src-tauri\target\release\bundle'

# Collect main .exe and bundle artifacts.
$artifacts = @()
$mainExe = Join-Path $ProjectRoot 'src-tauri\target\release\oxidelink.exe'
if (Test-Path $mainExe) {
    $artifacts += $mainExe
}

if (Test-Path $bundleRoot) {
    $artifacts += Get-ChildItem -Path $bundleRoot -Include '*.exe','*.msi' -Recurse -ErrorAction SilentlyContinue | Select-Object -ExpandProperty FullName
}

if ($artifacts.Count -eq 0) {
    Write-Warning 'No artifacts found to sign.'
} else {
    foreach ($file in $artifacts) {
        Invoke-SignIfNeeded -FilePath $file
    }
}

Write-Host 'Release build complete.'
