<#
.SYNOPSIS
  Measures GPU board power (nvidia-smi, 100 ms sampling) during a long edge-only run and writes
  results\suite\power.json. Board power only: CPU, RAM, PSU losses and cooling are not included.
#>
param(
    [string]$Model = "phi3",
    [string]$Urls = "http://127.0.0.1:11435,http://127.0.0.1:11436",
    [int]$Events = 1000,
    [int]$EdgeConcurrency = 32,
    [string]$OutDir = "results\suite",
    [string]$Name = "power"
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$out = Join-Path $root $OutDir
$samples = Join-Path $out "$Name-samples.csv"
$exe = Join-Path $root "target\release\inversion-gap.exe"

$smi = Start-Process nvidia-smi -ArgumentList "--query-gpu=timestamp,index,power.draw", "--format=csv,noheader,nounits", "-lms", "100" `
    -RedirectStandardOutput $samples -WindowStyle Hidden -PassThru
Start-Sleep -Seconds 5                       # idle baseline
$t0 = Get-Date
& $exe run --headless --edge-only --no-warmup --model $Model --ollama-url $Urls --events $Events `
    --edge-concurrency $EdgeConcurrency --json (Join-Path $out "$Name-run.json") *> (Join-Path $out "$Name-run.log")
$t1 = Get-Date
Start-Sleep -Milliseconds 500
Stop-Process -Id $smi.Id

$rows = Get-Content $samples | ForEach-Object {
    $p = $_ -split ",\s*"
    if ($p.Count -ge 3 -and $p[2] -match '^[\d.]+$') { [pscustomobject]@{ t = [datetime]$p[0]; gpu = [int]$p[1]; w = [double]$p[2] } }
}
function TotalWatts($set) {
    # Mean per GPU over the window, summed across GPUs.
    ($set | Group-Object gpu | ForEach-Object { ($_.Group | Measure-Object w -Average).Average } | Measure-Object -Sum).Sum
}
$idle = TotalWatts ($rows | Where-Object { $_.t -lt $t0 })
$busy = TotalWatts ($rows | Where-Object { $_.t -ge $t0 -and $_.t -le $t1 })
$run = Get-Content (Join-Path $out "$Name-run.json") -Raw | ConvertFrom-Json
$result = [ordered]@{
    model = $Model
    events = $run.edge.completed
    wall_s = $run.edge.elapsed_s
    throughput_ops = $run.edge.throughput_ops
    avg_gpu_w_total = [math]::Round($busy, 1)
    idle_gpu_w_total = [math]::Round($idle, 1)
    samples = @($rows).Count
    window = "$($t0.ToString('o')) .. $($t1.ToString('o'))"
}
$result | ConvertTo-Json | Set-Content (Join-Path $out "$Name.json") -Encoding utf8
$result
