# Pack portable zip from Tauri release build.
# Requires prior: npm run tauri:build (or cargo build --release under src-tauri).
# Version is read from package.json (keep in sync with tauri.conf.json / Cargo.toml).

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root

function Get-PackageVersion {
    $pkgPath = Join-Path $Root "package.json"
    if (-not (Test-Path $pkgPath)) {
        throw "package.json not found: $pkgPath"
    }
    $pkg = Get-Content -Raw -Encoding UTF8 $pkgPath | ConvertFrom-Json
    if (-not $pkg.version) {
        throw "package.json missing version"
    }
    return [string]$pkg.version
}

$version = Get-PackageVersion
$exe = Join-Path $Root "src-tauri\target\release\anchorterm.exe"
if (-not (Test-Path $exe)) {
    throw "Release binary not found: $exe`nRun: npm run tauri:build"
}

$outDir = Join-Path $Root "dist-release"
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

$zipName = "AnchorTerm-$version-windows-x64-portable.zip"
$zipPath = Join-Path $outDir $zipName
$stage = Join-Path $outDir "_portable_stage"
if (Test-Path $stage) {
    Remove-Item -Recurse -Force $stage
}
New-Item -ItemType Directory -Force -Path $stage | Out-Null

# Portable layout: single exe (+ any companion DLLs if present later)
Copy-Item -Force $exe (Join-Path $stage "anchorterm.exe")

# Optional sidecar DLLs next to the exe (Tauri usually static; keep for future).
$releaseDir = Split-Path $exe
Get-ChildItem -Path $releaseDir -Filter "*.dll" -File -ErrorAction SilentlyContinue |
    ForEach-Object { Copy-Item -Force $_.FullName $stage }

# Brief readme inside zip (ASCII filename for PowerShell encoding safety)
$readme = @"
AnchorTerm $version - portable (Windows x64)

1. Extract to any folder
2. Run anchorterm.exe
3. Requires: WebView2 Runtime, OpenSSH Client (ssh.exe)
4. Config: %APPDATA%\AnchorTerm\
5. Logs: %APPDATA%\AnchorTerm\logs\ (override with ANCHORTERM_LOG_DIR)

See docs/INSTALL.md in the source repo for end-user install notes.
"@
$readmePath = Join-Path $stage "README-portable.txt"
[System.IO.File]::WriteAllText($readmePath, $readme, [System.Text.UTF8Encoding]::new($false))

if (Test-Path $zipPath) {
    Remove-Item -Force $zipPath
}
Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zipPath -CompressionLevel Optimal
Remove-Item -Recurse -Force $stage

$size = (Get-Item -LiteralPath $zipPath).Length
$hash = $null
try {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    $fs = [System.IO.File]::OpenRead($zipPath)
    try {
        $hash = ([BitConverter]::ToString($sha.ComputeHash($fs))).Replace("-", "")
    } finally {
        $fs.Dispose()
        $sha.Dispose()
    }
} catch {
    $hash = "(hash unavailable: $($_.Exception.Message))"
}

Write-Host "OK portable zip:"
Write-Host "  $zipPath"
Write-Host "  size=$size bytes"
Write-Host "  SHA256=$hash"
Write-Host "  version=$version"
