<#
.SYNOPSIS
  Reproducible benchmark suite used for the InversionGap report.

.DESCRIPTION
  Assumes an edge pool is already running (start-edge-pool.ps1) on -Urls with -Model loaded.
  Every scenario writes JSON (summary + per-event verdicts) and its console output to -OutDir.

  -Set phi3 : full matrix (headline repeats, cloud concurrency sweep, profiles, controls, 1 vs 2 GPUs)
  -Set qwen : quality-oriented edge model; headline configuration only
#>
param(
    [ValidateSet("phi3", "qwen")] [string]$Set = "phi3",
    [string]$Model = "phi3",
    [string]$Urls = "http://127.0.0.1:11435,http://127.0.0.1:11436",
    [int]$Events = 100,
    [int]$EdgeConcurrency = 32,
    [string]$OutDir = "results\suite"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root "target\release\inversion-gap.exe"
$out = Join-Path $root $OutDir
New-Item -ItemType Directory -Force $out | Out-Null
$firstUrl = ($Urls -split ",")[0]

function Run([string]$name, [string[]]$extra) {
    $json = Join-Path $out "$name.json"
    $log = Join-Path $out "$name.log"
    $args = @("run", "--headless", "--model", $Model, "--ollama-url", $Urls, "--events", "$Events",
              "--edge-concurrency", "$EdgeConcurrency", "--json", $json) + $extra
    Write-Host ("[{0:HH:mm:ss}] {1}: {2}" -f (Get-Date), $name, ($extra -join " "))
    & $exe @args *> $log
    $head = Select-String -Path $log -Pattern "Edge is|Cloud is" | Select-Object -Last 1
    Write-Host "           -> $($head.Line.Trim())"
}

if ($Set -eq "phi3") {
    Run "A1-headline-r1" @("--snapshot-dir", (Join-Path $out "snapshots-headline"))
    Run "A1-headline-r2" @()
    Run "A1-headline-r3" @()
    Run "A2-cloud-c4-at-quota" @("--cloud-concurrency", "4")
    Run "A3-cloud-c32-over-quota" @("--cloud-concurrency", "32")
    Run "A4-azure-openai-c1" @("--profile", "azure-openai")
    Run "A4-azure-openai-c8-at-quota" @("--profile", "azure-openai", "--cloud-concurrency", "8")
    Run "A5-same-model-gateway" @("--gateway-backend", "ollama", "--schedule", "sequential")
    Run "A6-control-zero-c1" @("--profile", "zero")
    Run "A6-control-zero-c32" @("--profile", "zero", "--cloud-concurrency", "32")
    $saved = $Urls; $Urls = $firstUrl; $EdgeConcurrency = 16
    Run "A7-edge-one-gpu" @()
    $Urls = $saved
} else {
    $EdgeConcurrency = 2
    Run "B1-qwen-headline" @("--snapshot-dir", (Join-Path $out "snapshots-qwen"))
    Run "B2-qwen-cloud-c4-at-quota" @("--cloud-concurrency", "4")
}
Write-Host "done -> $out"
