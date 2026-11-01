# Stops the per-GPU Ollama servers started by start-edge-pool.ps1, including their
# llama-server.exe runner children (killing only the parent would orphan them and leak VRAM).
$root = Split-Path -Parent $PSScriptRoot
$pidFile = Join-Path $root ".edge-pool.pids"
if (-not (Test-Path $pidFile)) { Write-Host "No edge pool running (no $pidFile)."; exit 0 }
foreach ($id in Get-Content $pidFile) {
    $p = Get-Process -Id $id -ErrorAction SilentlyContinue
    if ($p -and $p.ProcessName -like "ollama*") {
        & taskkill.exe /PID $id /T /F | Out-Null
        Write-Host "Stopped ollama pid $id (and runners)"
    }
}
Remove-Item $pidFile
