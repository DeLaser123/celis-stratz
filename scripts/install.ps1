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
Write-Host ""
Write-Host "add to PATH if needed:"
Write-Host "  [Environment]::SetEnvironmentVariable('Path', `"$binDir;`$env:Path`", 'User')"
Write-Host "next: mkdir my-strategies; cd my-strategies; stratz init"
