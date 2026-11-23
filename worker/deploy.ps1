# Refresh RuneQL OpenRS2 snapshot, compile WASM query engine, and deploy to Cloudflare Workers (runeql.rs3.dev).
param(
    [switch]$SkipExport,
    [switch]$SkipWasm,
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"
$env:Path = "C:\nvm4w\nodejs;$env:USERPROFILE\.cargo\bin;$env:Path"

$WorkerDir = $PSScriptRoot
$RepoRoot = Split-Path -Parent $WorkerDir
$ServerDir = Join-Path $RepoRoot "server"
$WasmDir = Join-Path $WorkerDir "wasm"

if (-not $SkipExport) {
    Write-Host "==> Building server & exporting latest OpenRS2 RQL1 snapshot..." -ForegroundColor Cyan
    Push-Location $ServerDir
    try {
        cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "server cargo build failed" }
        .\target\release\runeql-server.exe export-snapshot --out (Join-Path $WorkerDir "src\catalog.rql.gz")
        if ($LASTEXITCODE -ne 0) { throw "export-snapshot failed" }
    } finally {
        Pop-Location
    }
}

if (-not $SkipWasm) {
    Write-Host "==> Compiling RuneQL engine to wasm32-unknown-unknown..." -ForegroundColor Cyan
    Push-Location $WasmDir
    try {
        cargo build --target wasm32-unknown-unknown --release
        if ($LASTEXITCODE -ne 0) { throw "wasm cargo build failed" }
        Copy-Item -Force ".\target\wasm32-unknown-unknown\release\runeql_worker_wasm.wasm" (Join-Path $WorkerDir "src\runeql_worker.wasm")
    } finally {
        Pop-Location
    }
}

Write-Host "==> Bundle assets:" -ForegroundColor Cyan
Get-Item (Join-Path $WorkerDir "src\runeql_worker.wasm"), (Join-Path $WorkerDir "src\catalog.rql.gz") |
    Select-Object Name, @{N='SizeKB';E={[math]::Round($_.Length/1KB, 1)}} |
    Format-Table -AutoSize

Push-Location $WorkerDir
try {
    if ($DryRun) {
        Write-Host "==> Running wrangler deploy --dry-run..." -ForegroundColor Cyan
        npx wrangler deploy --dry-run
    } else {
        Write-Host "==> Deploying to Cloudflare Workers (https://runeql.rs3.dev)..." -ForegroundColor Cyan
        npx wrangler deploy
    }
    if ($LASTEXITCODE -ne 0) { throw "wrangler deploy failed" }
} finally {
    Pop-Location
}
