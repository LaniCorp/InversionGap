<#
.SYNOPSIS
  Starts one dedicated Ollama server per GPU so the edge actor can fan out across all of them.

.DESCRIPTION
  A single Ollama server loads a model once, on one GPU, and (by default) serves one request at a
  time. For the edge benchmark we want one model replica per GPU, each accepting several
  concurrent requests. This script launches `ollama serve` per GPU on consecutive ports with:
    CUDA_VISIBLE_DEVICES = <gpu index>   (pin the replica to one card)
    OLLAMA_NUM_PARALLEL  = -NumParallel  (concurrent sequences per replica)
  It first unloads models from the default instance (port 11434) so VRAM is free.
  Your main Ollama config is not modified; stop the pool with stop-edge-pool.ps1.

.EXAMPLE
  .\scripts\start-edge-pool.ps1 -NumParallel 8 -Model qwen3.6:35b-a3b
  $env:INVERSION_OLLAMA_URLS = "http://127.0.0.1:11435,http://127.0.0.1:11436"
  cargo run --release -- run --edge-concurrency 16
#>
param(
    [int]$BasePort = 11435,
    [int]$NumParallel = 8,
    [int]$GpuCount = 0,
    [string]$Model = "",
    [string]$DefaultHost = "http://127.0.0.1:11434",
    # One server spanning all GPUs (for models too large for a single card, e.g. 70B).
    [switch]$Spread
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$logDir = Join-Path $root "logs"
$pidFile = Join-Path $root ".edge-pool.pids"
New-Item -ItemType Directory -Force $logDir | Out-Null

$ollama = (Get-Command ollama -ErrorAction Stop).Source
if ($GpuCount -le 0) {
    $GpuCount = @(& nvidia-smi -L | Where-Object { $_ -match '^GPU \d+' }).Count
    if ($GpuCount -le 0) { throw "No NVIDIA GPUs detected (nvidia-smi -L)." }
}

# Free VRAM held by the default instance.
try {
    $ps = Invoke-RestMethod "$DefaultHost/api/ps" -TimeoutSec 5
    foreach ($m in $ps.models) {
        Write-Host "Unloading $($m.name) from default instance $DefaultHost"
        Invoke-RestMethod "$DefaultHost/api/generate" -Method Post -ContentType "application/json" `
            -Body (@{ model = $m.name; keep_alive = 0 } | ConvertTo-Json) -TimeoutSec 60 | Out-Null
    }
} catch {
    Write-Host "Default instance not reachable at $DefaultHost (fine)."
}

$saved = @{}
foreach ($k in "OLLAMA_HOST", "CUDA_VISIBLE_DEVICES", "GGML_VK_VISIBLE_DEVICES", "OLLAMA_NUM_PARALLEL", "OLLAMA_MAX_LOADED_MODELS", "OLLAMA_KEEP_ALIVE", "OLLAMA_SCHED_SPREAD") {
    $saved[$k] = [Environment]::GetEnvironmentVariable($k, "Process")
}

$urls = @()
$pids = @()
try {
    $replicas = if ($Spread) { @((0..($GpuCount - 1)) -join ",") } else { 0..($GpuCount - 1) | ForEach-Object { "$_" } }
    for ($i = 0; $i -lt $replicas.Count; $i++) {
        $port = $BasePort + $i
        $env:OLLAMA_HOST = "127.0.0.1:$port"
        if ($Spread) {
            # Pinning a device list made Ollama use only the first GPU and spill the rest to CPU;
            # expose every GPU and ask the scheduler to spread the model across them instead.
            Remove-Item Env:CUDA_VISIBLE_DEVICES, Env:GGML_VK_VISIBLE_DEVICES -ErrorAction SilentlyContinue
            $env:OLLAMA_SCHED_SPREAD = "1"
        } else {
            $env:CUDA_VISIBLE_DEVICES = $replicas[$i]
            $env:GGML_VK_VISIBLE_DEVICES = $replicas[$i]   # Ollama may pick the Vulkan backend; pin that too
        }
        $env:OLLAMA_NUM_PARALLEL = "$NumParallel"
        $env:OLLAMA_MAX_LOADED_MODELS = "1"
        $env:OLLAMA_KEEP_ALIVE = "30m"
        $p = Start-Process -FilePath $ollama -ArgumentList "serve" -WindowStyle Hidden -PassThru `
            -RedirectStandardOutput (Join-Path $logDir "ollama-gpu$i.out.log") `
            -RedirectStandardError (Join-Path $logDir "ollama-gpu$i.err.log")
        $pids += $p.Id
        $urls += "http://127.0.0.1:$port"
        Write-Host "GPU $($replicas[$i]) -> http://127.0.0.1:$port (pid $($p.Id), OLLAMA_NUM_PARALLEL=$NumParallel)"
    }
} finally {
    foreach ($k in $saved.Keys) { [Environment]::SetEnvironmentVariable($k, $saved[$k], "Process") }
}
$pids | Set-Content $pidFile

foreach ($u in $urls) {
    $ok = $false
    for ($t = 0; $t -lt 60 -and -not $ok; $t++) {
        try { Invoke-RestMethod "$u/api/version" -TimeoutSec 2 | Out-Null; $ok = $true } catch { Start-Sleep -Milliseconds 500 }
    }
    if (-not $ok) { throw "Ollama at $u did not come up; see $logDir" }
}

if ($Model) {
    Write-Host "Pre-loading $Model on each replica..."
    $jobs = foreach ($u in $urls) {
        Start-Job -ArgumentList $u, $Model -ScriptBlock {
            param($u, $m)
            $body = @{ model = $m; prompt = "ok"; stream = $false; keep_alive = "30m"; options = @{ num_ctx = 2048; num_predict = 1 } } | ConvertTo-Json
            Invoke-RestMethod "$u/api/generate" -Method Post -ContentType "application/json" -Body $body -TimeoutSec 600 | Out-Null
            "$u loaded"
        }
    }
    $jobs | Wait-Job | Receive-Job
    $jobs | Remove-Job
}

$joined = $urls -join ","
Write-Host ""
Write-Host "Edge pool ready. Use:"
Write-Host "  `$env:INVERSION_OLLAMA_URLS = `"$joined`""
Write-Host "  cargo run --release -- run --edge-concurrency $($NumParallel * $replicas.Count)"
