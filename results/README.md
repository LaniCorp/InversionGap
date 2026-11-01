# Results

This folder holds every run behind the findings in the main README. Each scenario has a `.json` file (meta, per-side summaries,
per-event verdicts) and a `.log` file (the console output). Scenario names start with a letter that groups
them, and `-rN` marks a repeat run.

Each JSON file has this shape (Azure account names are redacted to `…-xxxxx`):

* `meta`: arguments, model, endpoint and profile.
* `cloud` / `edge` (or `arms` for agent runs): summaries with throughput, latency percentiles, the time
  breakdown, tokens, cost and bytes.
* `*_verdicts`: every event's verdict next to the reference rubric.

## `azure/`: real Azure OpenAI / AI Foundry vs the local edge

Cloud model gpt-4.1-mini unless noted, westus. "Regional" is a Standard deployment (200 requests/minute);
"global" is GlobalStandard (2,000 requests/minute).

| Prefix | What it measures |
|---|---|
| `Z1`–`Z7` | Edge phi3 vs gpt-4.1-mini, with the cloud sending 1 to 32 requests at once, regional and global. `Z2` uses an Entra ID token instead of an API key. `Z7` was run four times. |
| `Z8`, `Z9` | gpt-5-mini (reasoning model, `reasoning_effort=minimal`) |
| `Z10` | Sustained 600 events at 16 in flight against the regional 200 requests/minute quota (429 behaviour) |
| `G1`, `G2` | GPT-4o at 1 and 32 in flight ($2.50 / $10 per 1M tokens) |
| `P1`, `P2` | Same weights: Phi-4 on Azure AI Foundry vs phi4 locally. `P1-…-coldstart` includes a cold local model load and is kept only for transparency. |
| `Q1`, `Q2` | Edge qwen3.6 35B (cannot batch in Ollama) |
| `E0`–`E2` | Edge frontier: phi4 14B and Llama 3.3 70B vs regional, global ×32 and GPT-4o ×32 |
| `power-*` | GPU board power (nvidia-smi samples) over 1,000 edge-only events |
| `snapshot-real-run.json`, `snapshots-real/` | A real-Azure `run` with off-screen dashboard renders at four moments |

**Invalid runs, kept for the record:** `E0-llama33-*` and `E2-llama33-*` ran with GPU pinning that put 34
of Llama 3.3's 81 layers on the CPU. They are excluded from all findings. `E1-llama33-*` is the corrected run
(`-Spread`, 81/81 layers on the two GPUs).

## `agent/`: the 4-step agent loop, arms A–D

| File | Setup |
|---|---|
| `R1-*`, `R1-r2-*` | All four arms, gpt-4.1-mini global vs phi4, 100 events. `R1-r2` is a repeat. |
| `R2-*` | Arms A, B and D with GPT-4o as the cloud model |
| `R3-*` | Arm D escalating on confidence < 0.85 only (no "critical" rule) |
| `R4-*` | Arms B, C and D at equal concurrency (8 in flight) |
| `screenshots/` | Real Windows Terminal captures of the dashboard (a separate 60-event run) |
| `snapshots-*/` | Off-screen dashboard renders (HTML + text) at the halfway point and at the end of each arm |

## `suite/`: v1, the mock cloud gateway

These runs use the `azure-copilot` / `azure-openai` / `zero` profiles. The cloud latencies come from those
profiles, so these results show the mechanism, not real Azure performance.

| Prefix | What it measures |
|---|---|
| `A1` | Headline: serialized cloud vs edge phi3 (three repeats) |
| `A2`, `A3` | Cloud at the quota (4 in flight) and over it (32) |
| `A4` | Lighter `azure-openai` profile |
| `A5` | Same model on both sides: the gateway forwards to local Ollama |
| `A6` | Control: `zero` profile, plumbing only |
| `A7` | Edge on one GPU |
| `B1`, `B2` | Edge qwen3.6 |
| `power*` | GPU power for phi3 |

The two files at the top of this folder (`headline-azure-copilot.json`, `fair-concurrency.json`) are the
first mock-gateway runs from development. They are superseded by `suite/`.
