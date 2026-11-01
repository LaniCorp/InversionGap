//! Command-line arguments (clap). Every flag also appears in `inversion-gap <subcommand> --help`.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::gateway::{BackendKind, LatencyProfile, MsRange, ProfileName};
use crate::ollama::OutputFormat;

#[derive(Parser, Debug)]
#[command(
    name = "inversion-gap",
    version,
    about = "Measures the Orchestration Tax: centralized cloud/API-wrapper triage vs sovereign local-edge actors"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the head-to-head benchmark with the live dashboard
    Run(RunArgs),
    /// Verify Ollama endpoints are reachable and can triage an event end-to-end
    Check(EdgeArgs),
    /// Run the mock cloud gateway standalone (e.g. on an Azure VM to get real WAN hops)
    Gateway(GatewayArgs),
    /// Compound agent loop (model → tool → model → tool) across architectural arms A–D
    Agent(AgentArgs),
}

#[derive(Args, Debug, Clone)]
pub struct AgentArgs {
    #[command(flatten)]
    pub edge: EdgeArgs,
    #[command(flatten)]
    pub profile: ProfileArgs,

    /// Arms to run: a (cloud-hosted agent), b (sovereign orchestrator + cloud model), c (fully local), d (hybrid router)
    #[arg(long, value_enum, value_delimiter = ',', default_value = "a,b,c,d")]
    pub arms: Vec<crate::agent::Arm>,
    /// Events per arm
    #[arg(long, default_value_t = 50)]
    pub events: usize,
    /// Seed for the deterministic event generator (same seed = same events for every arm)
    #[arg(long, default_value_t = 42)]
    pub seed: u64,
    /// Events in flight for the cloud arms (A, B)
    #[arg(long, default_value_t = 8)]
    pub cloud_concurrency: usize,
    /// Events in flight for the local arms (C, D)
    #[arg(long, default_value_t = 32)]
    pub edge_concurrency: usize,
    /// Run arms one after another (default; avoids GPU and quota contention) or all at once
    #[arg(long, value_enum, default_value_t = Schedule::Sequential)]
    pub schedule: Schedule,

    /// Cloud model: the in-process mock (synthetic) or a real OpenAI-compatible / Azure OpenAI endpoint
    #[arg(long, value_enum, default_value_t = CloudTarget::Mock)]
    pub cloud: CloudTarget,
    /// Full chat-completions URL for --cloud openai, e.g.
    /// https://<res>.openai.azure.com/openai/deployments/<dep>/chat/completions?api-version=2024-10-21
    #[arg(long, env = "INVERSION_CLOUD_ENDPOINT")]
    pub cloud_endpoint: Option<String>,
    /// Model/deployment name sent in the request body
    #[arg(long, env = "INVERSION_CLOUD_MODEL", default_value = "gpt-4o-mini")]
    pub cloud_model: String,
    /// API key for the real endpoint (`api-key` header); also used by arm A's cloud runtime to reach the model
    #[arg(long, env = "INVERSION_CLOUD_API_KEY", hide_env_values = true)]
    pub cloud_api_key: Option<String>,
    /// Max completion tokens per cloud model call
    #[arg(long, default_value_t = 256)]
    pub cloud_max_tokens: u32,
    /// `reasoning_effort` for reasoning cloud models (e.g. gpt-5-mini): minimal | low | medium | high
    #[arg(long)]
    pub cloud_reasoning_effort: Option<String>,
    /// USD per 1M input tokens for the cloud model
    #[arg(long, default_value_t = 0.40)]
    pub price_in: f64,
    /// USD per 1M output tokens for the cloud model
    #[arg(long, default_value_t = 1.60)]
    pub price_out: f64,

    /// Hybrid arm D: escalate when the local verdict's confidence is below this
    #[arg(long, default_value_t = 0.85)]
    pub escalate_below: f32,
    /// Hybrid arm D: do not escalate critical verdicts on severity alone
    #[arg(long)]
    pub no_escalate_critical: bool,

    /// Owned hardware cost for the break-even calculation (2× RTX 5090 at list)
    #[arg(long, default_value_t = 4000.0)]
    pub hardware_usd: f64,
    /// Daily event volume for the break-even calculation
    #[arg(long, default_value_t = 100_000.0)]
    pub events_per_day: f64,
    /// Local energy cost per 1M events, subtracted from savings (measured ≈ $0.5–0.7 for phi3/phi4 at $0.15/kWh)
    #[arg(long, default_value_t = 0.6)]
    pub edge_energy_usd_per_m: f64,

    /// Plain-text progress and summary instead of the TUI
    #[arg(long)]
    pub headless: bool,
    /// Write per-arm summaries, verdicts and paybacks to a JSON file
    #[arg(long)]
    pub json: Option<PathBuf>,
    /// Skip the (untimed) model warm-up on edge endpoints
    #[arg(long)]
    pub no_warmup: bool,
    /// Write off-screen renders of the dashboard (HTML + text) at key moments into this directory
    #[arg(long)]
    pub snapshot_dir: Option<PathBuf>,
    /// Snapshot size in terminal cells, WIDTHxHEIGHT
    #[arg(long, default_value = "150x56")]
    pub snapshot_size: String,
}

#[derive(Args, Debug, Clone)]
pub struct EdgeArgs {
    /// Ollama endpoints, comma-separated or repeated. Use one per GPU to fan out across GPUs.
    #[arg(long = "ollama-url", env = "INVERSION_OLLAMA_URLS", value_delimiter = ',', default_value = "http://localhost:11434")]
    pub ollama_urls: Vec<String>,
    /// Local model. Auto-detected when omitted (prefers phi3, phi4, llama3, qwen, gemma, ...)
    #[arg(long, env = "INVERSION_MODEL")]
    pub model: Option<String>,
    /// Output constraint for edge calls
    #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
    pub edge_format: OutputFormat,
}

#[derive(Args, Debug, Clone)]
pub struct ProfileArgs {
    /// Latency/orchestration profile for the mock gateway
    #[arg(long, value_enum, default_value_t = ProfileName::AzureCopilot)]
    pub profile: ProfileName,
    /// Override simulated WAN round-trip, e.g. "60-110" (ms). Use "0" when the gateway is truly remote.
    #[arg(long)]
    pub wan_rtt_ms: Option<MsRange>,
    /// Override synthetic upstream model time, e.g. "150-500" (ms)
    #[arg(long)]
    pub inference_ms: Option<MsRange>,
    /// Override the gateway's per-client in-flight quota (excess requests get HTTP 429)
    #[arg(long)]
    pub quota: Option<usize>,
}

impl ProfileArgs {
    pub fn resolve(&self) -> LatencyProfile {
        let mut p = self.profile.profile();
        if let Some(r) = self.wan_rtt_ms {
            p.wan_rtt = r;
        }
        if let Some(r) = self.inference_ms {
            p.inference = r;
        }
        if let Some(q) = self.quota {
            p.max_inflight = q;
        }
        p
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CloudTarget {
    /// In-process (or --gateway-url remote) mock gateway with OAuth, policy, safety and quota stages
    Mock,
    /// A real OpenAI-compatible / Azure OpenAI chat-completions endpoint
    Openai,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CloudAuthKind {
    /// `api-key: <key>` header (Azure OpenAI)
    ApiKey,
    /// `Authorization: Bearer <key>` (OpenAI-compatible)
    Bearer,
    /// Real Entra ID token from `az account get-access-token`
    EntraCli,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Schedule {
    /// Run both harnesses at the same time (live side-by-side race)
    Parallel,
    /// Cloud first, then edge (use with --gateway-backend ollama to avoid GPU contention)
    Sequential,
}

#[derive(Args, Debug, Clone)]
pub struct RunArgs {
    #[command(flatten)]
    pub edge: EdgeArgs,
    #[command(flatten)]
    pub profile: ProfileArgs,

    /// Number of events to triage per harness
    #[arg(long, default_value_t = 50)]
    pub events: usize,
    /// Seed for the deterministic event generator
    #[arg(long, default_value_t = 42)]
    pub seed: u64,
    /// Concurrent in-flight requests for the edge actor (spread across --ollama-url endpoints)
    #[arg(long, default_value_t = 32)]
    pub edge_concurrency: usize,
    /// Concurrent in-flight requests for the cloud harness (1 = serialized API calls)
    #[arg(long, default_value_t = 1)]
    pub cloud_concurrency: usize,
    #[arg(long, value_enum, default_value_t = Schedule::Parallel)]
    pub schedule: Schedule,

    /// Cloud implementation to benchmark against
    #[arg(long, value_enum, default_value_t = CloudTarget::Mock)]
    pub cloud: CloudTarget,
    /// Upstream model behind the mock gateway
    #[arg(long, value_enum, default_value_t = BackendKind::Synthetic)]
    pub gateway_backend: BackendKind,
    /// Use an already-running mock gateway (e.g. `inversion-gap gateway` on an Azure VM) instead of spawning one
    #[arg(long, env = "INVERSION_GATEWAY_URL")]
    pub gateway_url: Option<String>,
    /// Full chat-completions URL for --cloud openai, e.g.
    /// https://<res>.openai.azure.com/openai/deployments/<dep>/chat/completions?api-version=2024-10-21
    #[arg(long, env = "INVERSION_CLOUD_ENDPOINT")]
    pub cloud_endpoint: Option<String>,
    /// Model/deployment name sent in the request body
    #[arg(long, env = "INVERSION_CLOUD_MODEL", default_value = "gpt-4o-mini")]
    pub cloud_model: String,
    #[arg(long, value_enum, default_value_t = CloudAuthKind::ApiKey)]
    pub cloud_auth: CloudAuthKind,
    /// API key for --cloud-auth api-key|bearer
    #[arg(long, env = "INVERSION_CLOUD_API_KEY", hide_env_values = true)]
    pub cloud_api_key: Option<String>,
    /// Resource for --cloud-auth entra-cli
    #[arg(long, default_value = "https://cognitiveservices.azure.com")]
    pub entra_resource: String,
    /// Max completion tokens for cloud calls
    #[arg(long, default_value_t = 128)]
    pub cloud_max_tokens: u32,
    /// `reasoning_effort` for reasoning cloud models (e.g. gpt-5-mini): minimal | low | medium | high
    #[arg(long)]
    pub cloud_reasoning_effort: Option<String>,

    /// Token meter: USD per 1M input tokens (default ≈ GPT-4o list price)
    #[arg(long, default_value_t = 2.50)]
    pub price_in: f64,
    /// Token meter: USD per 1M output tokens
    #[arg(long, default_value_t = 10.00)]
    pub price_out: f64,

    /// Plain-text progress and summary instead of the TUI
    #[arg(long)]
    pub headless: bool,
    /// Write the final summaries to a JSON file
    #[arg(long)]
    pub json: Option<PathBuf>,
    /// Skip the (untimed) model warm-up on edge endpoints
    #[arg(long)]
    pub no_warmup: bool,
    /// Run only the edge actor (e.g. for steady-state GPU power measurement)
    #[arg(long)]
    pub edge_only: bool,
    /// Write off-screen renders of the dashboard (HTML + text) at key moments into this directory
    #[arg(long)]
    pub snapshot_dir: Option<PathBuf>,
    /// Snapshot size in terminal cells, WIDTHxHEIGHT
    #[arg(long, default_value = "150x50")]
    pub snapshot_size: String,
}

#[derive(Args, Debug, Clone)]
pub struct GatewayArgs {
    /// Address to listen on
    #[arg(long, default_value = "0.0.0.0:8787")]
    pub bind: SocketAddr,
    #[command(flatten)]
    pub profile: ProfileArgs,
    /// Upstream model behind the gateway
    #[arg(long, value_enum, default_value_t = BackendKind::Synthetic)]
    pub backend: BackendKind,
    #[command(flatten)]
    pub edge: EdgeArgs,
}
