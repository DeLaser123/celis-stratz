# Installer (Windows): build from source and put `stratz.exe` on a
# persistent tools directory. Usage:
#   ./scripts/install.ps1
$ErrorActionPreference = "Stop"

$binDir = if ($env:STRATZ_BIN_DIR) { $env:STRATZ_BIN_DIR } else { "$env:USERPROFILE\stratz\bin" }

Write-Host "building from source (cargo)..."
cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

New-Item -ItemType Directory -Force -Path $binDir | Out-Null
Copy-Item target/release/stratz.exe "$binDir/stratz.exe" -Force
Write-Host "installed: $binDir\stratz.exe"

# self-update: record the source repo so the binary can pick up dev builds
$stratzHome = "$env:USERPROFILE\.stratz-cli"
New-Item -ItemType Directory -Force -Path $stratzHome | Out-Null
$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Content -Path "$stratzHome\source.txt" -Value $repoRoot
Write-Host "source marker: $stratzHome\source.txt -> $repoRoot"

# persistent user PATH
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not $userPath -or -not $userPath.Contains($binDir)) {
    [Environment]::SetEnvironmentVariable('Path', "$binDir;$userPath", 'User')
    Write-Host "added $binDir to user PATH (new terminals only)"
}
Write-Host ""
Write-Host "add to PATH if needed:"
Write-Host "  [Environment]::SetEnvironmentVariable('Path', `"$binDir;`$env:Path`", 'User')"
Write-Host "next: mkdir my-strategies; cd my-strategies; stratz init"
