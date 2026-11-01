<#
.SYNOPSIS
  Real-cloud benchmark: Azure OpenAI / Azure AI Foundry deployments vs the local edge pool.

.DESCRIPTION
  Requires an edge pool (start-edge-pool.ps1) and, in -ResourceGroup:
    <OpenAiAccount>  kind OpenAI:     gpt-41-mini (Standard, regional), gpt-41-mini-global, gpt-5-mini, gpt-4o-global (GlobalStandard)
    <FoundryAccount> kind AIServices: phi-4 (GlobalStandard)
  API keys are read with `az` into an environment variable per run and never written to disk.
  Prices are Azure retail list prices for westus (prices.azure.com), USD per 1M tokens.

  -Set phi3   real-cloud matrix against the phi3 edge pool (Z*, G*)
  -Set qwen   quality-matched pairs against a qwen3.6 pool (Q*)
  -Set phi4   same-model pair: Phi-4 on Azure vs phi4 in the local pool (P*)
  -Set edge   high-quality batchable edge models vs gpt-4.1-mini global (E*); pass -Model
  -Only      run only the named scenarios
#>
param(
    [ValidateSet("phi3", "qwen", "phi4", "edge")] [string]$Set = "phi3",
    [string]$OpenAiAccount = "",
    [string]$FoundryAccount = "",
    [string]$ResourceGroup = "",
    [string]$Model = "phi3",
    [string]$Urls = "http://127.0.0.1:11435,http://127.0.0.1:11436",
    [int]$Events = 100,
    [int]$EdgeConcurrency = 32,
    [string]$Tag = "",
    [string]$OutDir = "results\azure",
    [string[]]$Only = @()
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

# Account names default to results\azure-resource.txt, which provision-azure.ps1 writes (not committed).
$resFile = Join-Path $root "results\azure-resource.txt"
if (Test-Path $resFile) {
    $res = @{}
    Get-Content $resFile | ForEach-Object { if ($_ -match '^\s*([^=]+)=(.*)$') { $res[$Matches[1].Trim()] = $Matches[2].Trim() } }
    if (-not $OpenAiAccount) { $OpenAiAccount = $res["account"] }
    if (-not $FoundryAccount) { $FoundryAccount = $res["ai_services_account"] }
    if (-not $ResourceGroup) { $ResourceGroup = $res["resource_group"] }
}
if (-not $ResourceGroup) { $ResourceGroup = "rg-inversiongap" }
if (-not $OpenAiAccount -or -not $FoundryAccount) {
    throw "Pass -OpenAiAccount and -FoundryAccount, or run provision-azure.ps1 first (it writes $resFile)."
}
$exe = Join-Path $root "target\release\inversion-gap.exe"
$out = Join-Path $root $OutDir
New-Item -ItemType Directory -Force $out | Out-Null

$keys = @{}
function KeyFor([string]$account) {
    if (-not $keys.ContainsKey($account)) {
        $keys[$account] = az cognitiveservices account keys list -n $account -g $ResourceGroup --query key1 -o tsv
    }
    $keys[$account]
}

$deps = @{
    "regional" = @{ acct = $OpenAiAccount; dep = "gpt-41-mini"; pin = "0.44"; pout = "1.76"; extra = @() }
    "global"   = @{ acct = $OpenAiAccount; dep = "gpt-41-mini-global"; pin = "0.40"; pout = "1.60"; extra = @() }
    "gpt5"     = @{ acct = $OpenAiAccount; dep = "gpt-5-mini"; pin = "0.25"; pout = "2.00"; extra = @("--cloud-reasoning-effort", "minimal", "--cloud-max-tokens", "256") }
    "gpt4o"    = @{ acct = $OpenAiAccount; dep = "gpt-4o-global"; pin = "2.50"; pout = "10.00"; extra = @() }
    "phi4"     = @{ acct = $FoundryAccount; dep = "phi-4"; pin = "0.125"; pout = "0.50"; extra = @() }
}

function Run([string]$name, [string]$which, [int]$cc, [string]$auth = "api-key", [string[]]$more = @(), [int]$n = $Events) {
    if ($Only.Count -gt 0 -and $Only -notcontains $name) { return }
    $d = $deps[$which]
    $json = Join-Path $out "$name.json"
    $log = Join-Path $out "$name.log"
    $argv = @("run", "--headless", "--model", $Model, "--ollama-url", $Urls, "--events", "$n",
              "--edge-concurrency", "$EdgeConcurrency", "--cloud", "openai",
              "--cloud-endpoint", "https://$($d.acct).openai.azure.com/openai/deployments/$($d.dep)/chat/completions?api-version=2024-10-21",
              "--cloud-model", $d.dep, "--cloud-auth", $auth, "--cloud-concurrency", "$cc",
              "--price-in", $d.pin, "--price-out", $d.pout, "--json", $json) + $d.extra + $more
    $env:INVERSION_CLOUD_API_KEY = KeyFor $d.acct
    Write-Host ("[{0:HH:mm:ss}] {1}: {2} x{3} {4}, edge {5} x{6}" -f (Get-Date), $name, $d.dep, $cc, $auth, $Model, $EdgeConcurrency)
    & $exe @argv *> $log
    $head = Select-String -Path $log -Pattern "Edge is|Cloud is" | Select-Object -Last 1
    Write-Host "           -> $($head.Line.Trim())"
}

try {
    switch ($Set) {
        "phi3" {
            Run "Z1-regional-c1-r1" "regional" 1
            Run "Z1-regional-c1-r2" "regional" 1
            Run "Z1-regional-c1-r3" "regional" 1
            Run "Z2-regional-c1-entra" "regional" 1 "entra-cli"
            Run "Z3-regional-c4" "regional" 4
            Run "Z4-regional-c16" "regional" 16
            Run "Z5-global-c1" "global" 1
            Run "Z6-global-c8" "global" 8
            Run "Z7-global-c32" "global" 32
            Run "Z7-global-c32-r2" "global" 32
            Run "Z7-global-c32-r3" "global" 32
            Run "Z7-global-c32-r4" "global" 32
            Run "Z8-gpt5mini-c1" "gpt5" 1
            Run "Z9-gpt5mini-c8" "gpt5" 8
            # 100-event runs never exceed a 200 requests/minute quota; this one sustains load past it.
            Run "Z10-regional-c16-sustained" "regional" 16 "api-key" @() 600
            Run "G1-gpt4o-c1" "gpt4o" 1
            Run "G2-gpt4o-c32" "gpt4o" 32
        }
        "qwen" {
            $EdgeConcurrency = 2
            Run "Q1-qwen-vs-regional-c1" "regional" 1
            Run "Q2-qwen-vs-global-c32" "global" 32
        }
        "phi4" {
            # Phi-4 serverless quota here is 20 requests/minute: the cloud side is quota-bound by design.
            Run "P1-phi4-same-model-c1" "phi4" 1 "api-key" @() 40
            Run "P2-phi4-same-model-c8" "phi4" 8 "api-key" @() 40
        }
        "edge" {
            Run "E0-$Tag-vs-regional-c1" "regional" 1
            Run "E1-$Tag-vs-global-c32" "global" 32
            Run "E2-$Tag-vs-gpt4o-c32" "gpt4o" 32
        }
    }
} finally {
    Remove-Item Env:INVERSION_CLOUD_API_KEY -ErrorAction SilentlyContinue
}
Write-Host "done -> $out"
