<#
.SYNOPSIS
  Creates the Azure resources the real-cloud runs use (run-azure-suite.ps1 and the `agent` examples).

.DESCRIPTION
  One resource group with two accounts:
    <OpenAiAccount>  kind OpenAI      gpt-41-mini         gpt-4.1-mini 2025-04-14  Standard (regional)  capacity 200
                                      gpt-41-mini-global  gpt-4.1-mini 2025-04-14  GlobalStandard       capacity 2000
                                      gpt-5-mini          gpt-5-mini   2025-08-07  GlobalStandard       capacity 200
                                      gpt-4o-global       gpt-4o       2024-11-20  GlobalStandard       capacity 400
    <FoundryAccount> kind AIServices  phi-4               Phi-4 7                  GlobalStandard       capacity 20
  These are the deployments behind the published results. Model versions, regions and the capacity
  your subscription may use change over time; if a create fails on quota, lower -Capacity* or pick
  another region, and note the change next to your results.

  Everything is pay-per-token: there is no idle cost. Remove it all with
    az group delete -n <ResourceGroup>

  -DryRun prints the az commands without running them.
#>
param(
    [string]$ResourceGroup = "rg-inversiongap",
    [string]$Location = "westus",
    [string]$Suffix = (-join ((97..122) | Get-Random -Count 5 | ForEach-Object { [char]$_ })),
    [string]$OpenAiAccount = "",
    [string]$FoundryAccount = "",
    [int]$CapacityRegional = 200,
    [int]$CapacityGlobal = 2000,
    [int]$CapacityGpt5Mini = 200,
    [int]$CapacityGpt4o = 400,
    [int]$CapacityPhi4 = 20,
    [switch]$DryRun
)
$ErrorActionPreference = "Stop"
if (-not $OpenAiAccount) { $OpenAiAccount = "aoai-inversiongap-$Suffix" }
if (-not $FoundryAccount) { $FoundryAccount = "ais-inversiongap-$Suffix" }

function Az([string[]]$a) {
    Write-Host "az $($a -join ' ')" -ForegroundColor DarkGray
    if ($DryRun) { return }
    & az @a -o none
    if ($LASTEXITCODE -ne 0) { throw "az $($a[0..2] -join ' ') failed" }
}

function Deploy([string]$account, [string]$name, [string]$model, [string]$version, [string]$format, [string]$sku, [int]$cap) {
    Az @("cognitiveservices", "account", "deployment", "create", "-g", $ResourceGroup, "-n", $account,
        "--deployment-name", $name, "--model-name", $model, "--model-version", $version,
        "--model-format", $format, "--sku-name", $sku, "--sku-capacity", "$cap")
}

Az @("group", "create", "-n", $ResourceGroup, "-l", $Location)
Az @("cognitiveservices", "account", "create", "-g", $ResourceGroup, "-n", $OpenAiAccount, "-l", $Location,
    "--kind", "OpenAI", "--sku", "S0", "--custom-domain", $OpenAiAccount, "--yes")
Az @("cognitiveservices", "account", "create", "-g", $ResourceGroup, "-n", $FoundryAccount, "-l", $Location,
    "--kind", "AIServices", "--sku", "S0", "--custom-domain", $FoundryAccount, "--yes")

Deploy $OpenAiAccount "gpt-41-mini" "gpt-4.1-mini" "2025-04-14" "OpenAI" "Standard" $CapacityRegional
Deploy $OpenAiAccount "gpt-41-mini-global" "gpt-4.1-mini" "2025-04-14" "OpenAI" "GlobalStandard" $CapacityGlobal
Deploy $OpenAiAccount "gpt-5-mini" "gpt-5-mini" "2025-08-07" "OpenAI" "GlobalStandard" $CapacityGpt5Mini
Deploy $OpenAiAccount "gpt-4o-global" "gpt-4o" "2024-11-20" "OpenAI" "GlobalStandard" $CapacityGpt4o
Deploy $FoundryAccount "phi-4" "Phi-4" "7" "Microsoft" "GlobalStandard" $CapacityPhi4

if (-not $DryRun) {
    # Read by run-azure-suite.ps1; ignored by git so account names stay out of the repository.
    $resFile = Join-Path (Split-Path -Parent $PSScriptRoot) "results\azure-resource.txt"
    @("resource_group=$ResourceGroup", "account=$OpenAiAccount", "region=$Location", "ai_services_account=$FoundryAccount") |
        Set-Content -Encoding utf8 $resFile
}

Write-Host ""
Write-Host "Done. run-azure-suite.ps1 reads these from results\azure-resource.txt:" -ForegroundColor Green
Write-Host "  -ResourceGroup $ResourceGroup -OpenAiAccount $OpenAiAccount -FoundryAccount $FoundryAccount"
Write-Host "Endpoint for a single deployment:"
Write-Host "  https://$OpenAiAccount.openai.azure.com/openai/deployments/gpt-41-mini-global/chat/completions?api-version=2024-10-21"
