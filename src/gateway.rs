//! Mock centralized gateway: the "Orchestration Tax" made explicit.
//!
//! Every chat request pays, in order: WAN ingress → APIM-style quota admission (429 when over
//! quota) → Entra-like JWT validation → envelope deserialization → routing/policy → content
//! safety → upstream inference → response serialization → WAN egress. Each stage is reported
//! back in a `Server-Timing` header so the client can attribute where the time went.
//!
//! The upstream "model" is either synthetic (rule-based verdict + a 150–500 ms sleep standing in
//! for a hosted model) or a real Ollama endpoint, which turns the comparison into a pure
//! orchestration-overhead measurement: same model, same GPUs, plus the cloud plumbing.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::Utc;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::time::sleep;

use crate::events::Rng;
use crate::triage::heuristic_verdict;
use crate::wire::{ChatRequest, ChatResponse, Choice, OllamaChatResponse, ResponseMessage, Usage};
use crate::ollama;
use crate::tools;

pub const CLIENT_ID: &str = "inversion-gap-harness";
pub const CLIENT_SECRET: &str = "mock-secret-not-a-real-credential";
pub const TENANT_ID: &str = "00000000-1a2b-4c3d-9e8f-000000000042";
pub const AUDIENCE: &str = "api://inversion-gap-gateway";
pub const SCOPE: &str = "api://inversion-gap-gateway/.default";
const SIGNING_KEY: &[u8] = b"inversion-gap-mock-signing-key";
const REGIONS: [&str; 3] = ["westeurope", "swedencentral", "eastus2"];

// ---------------------------------------------------------------- latency profiles

/// Inclusive millisecond range, parsed from `"150-500"` or `"200"`.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct MsRange {
    pub lo: u64,
    pub hi: u64,
}

impl MsRange {
    const fn new(lo: u64, hi: u64) -> Self {
        Self { lo, hi }
    }
}

impl FromStr for MsRange {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parse = |v: &str| v.trim().parse::<u64>().map_err(|e| format!("'{v}': {e}"));
        match s.split_once('-') {
            Some((lo, hi)) => {
                let (lo, hi) = (parse(lo)?, parse(hi)?);
                if hi < lo { Err(format!("range {s} is reversed")) } else { Ok(Self { lo, hi }) }
            }
            None => parse(s).map(|v| Self { lo: v, hi: v }),
        }
    }
}

impl std::fmt::Display for MsRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.lo == self.hi { write!(f, "{}ms", self.lo) } else { write!(f, "{}-{}ms", self.lo, self.hi) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ProfileName {
    /// Copilot-style: Entra auth, APIM, tenant grounding meta-prompt, prompt shields, cross-region RTT
    AzureCopilot,
    /// Direct Azure OpenAI deployment: lighter policy stack, in-region RTT, no meta-prompt
    AzureOpenai,
    /// No injected delays at all — measures only real HTTP/JSON/auth plumbing
    Zero,
}

#[derive(Debug, Clone, Serialize)]
pub struct LatencyProfile {
    pub name: &'static str,
    /// Full round-trip; half is charged on ingress, half on egress.
    pub wan_rtt: MsRange,
    pub token_issue: MsRange,
    pub token_ttl_s: u64,
    pub jwt_validate: MsRange,
    pub policy: MsRange,
    pub safety: MsRange,
    /// Synthetic upstream model time (ignored with the Ollama backend).
    pub inference: MsRange,
    /// Server-side system/meta-prompt + grounding tokens billed on every call.
    pub meta_prompt_tokens: u64,
    /// Concurrent in-flight requests allowed per client before 429s.
    pub max_inflight: usize,
    pub retry_after_ms: u64,
}

impl ProfileName {
    pub fn profile(self) -> LatencyProfile {
        match self {
            ProfileName::AzureCopilot => LatencyProfile {
                name: "azure-copilot",
                wan_rtt: MsRange::new(60, 110),
                token_issue: MsRange::new(120, 220),
                token_ttl_s: 3600,
                jwt_validate: MsRange::new(8, 20),
                policy: MsRange::new(15, 45),
                safety: MsRange::new(40, 90),
                inference: MsRange::new(150, 500),
                meta_prompt_tokens: 900,
                max_inflight: 4,
                retry_after_ms: 300,
            },
            ProfileName::AzureOpenai => LatencyProfile {
                name: "azure-openai",
                wan_rtt: MsRange::new(25, 60),
                token_issue: MsRange::new(80, 150),
                token_ttl_s: 3600,
                jwt_validate: MsRange::new(3, 8),
                policy: MsRange::new(5, 15),
                safety: MsRange::new(20, 50),
                inference: MsRange::new(150, 400),
                meta_prompt_tokens: 0,
                max_inflight: 8,
                retry_after_ms: 200,
            },
            ProfileName::Zero => LatencyProfile {
                name: "zero",
                wan_rtt: MsRange::new(0, 0),
                token_issue: MsRange::new(0, 0),
                token_ttl_s: 3600,
                jwt_validate: MsRange::new(0, 0),
                policy: MsRange::new(0, 0),
                safety: MsRange::new(0, 0),
                inference: MsRange::new(0, 0),
                meta_prompt_tokens: 0,
                max_inflight: 1024,
                retry_after_ms: 50,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BackendKind {
    /// Rule-based verdict after sleeping the profile's inference range
    Synthetic,
    /// Forward to Ollama — isolates pure orchestration overhead (same model as the edge)
    Ollama,
}

pub enum Backend {
    Synthetic,
    Ollama { url: String, model: String, think: Option<bool> },
    /// Forward to a real OpenAI-compatible endpoint (agent arm A: the cloud-hosted runtime calls the cloud model).
    Azure { endpoint: String, api_key: String, model: String, max_tokens: u32, reasoning_effort: Option<String> },
}

impl Backend {
    pub fn describe(&self) -> String {
        match self {
            Backend::Synthetic => "synthetic upstream".into(),
            Backend::Ollama { model, .. } => format!("ollama upstream ({model})"),
            Backend::Azure { model, .. } => format!("real cloud upstream ({model})"),
        }
    }
}

// ---------------------------------------------------------------- server

pub struct GatewayState {
    profile: LatencyProfile,
    backend: Backend,
    rng: Mutex<Rng>,
    inflight: Arc<Semaphore>,
    http: reqwest::Client,
    seq: AtomicU64,
    /// Cloud-side agent state (tickets written by arm A's tool calls).
    store: Mutex<HashMap<u32, serde_json::Value>>,
}

impl GatewayState {
    pub fn new(profile: LatencyProfile, backend: Backend) -> Self {
        let inflight = Arc::new(Semaphore::new(profile.max_inflight.max(1)));
        Self {
            profile,
            backend,
            rng: Mutex::new(Rng::new(0xC10D)),
            inflight,
            http: reqwest::Client::builder().timeout(Duration::from_secs(120)).build().expect("http client"),
            seq: AtomicU64::new(1),
            store: Mutex::new(HashMap::new()),
        }
    }

    fn draw(&self, r: MsRange) -> u64 {
        self.rng.lock().unwrap().range(r.lo, r.hi)
    }

    async fn stall(&self, r: MsRange) -> f64 {
        let ms = self.draw(r);
        pause(ms).await;
        ms as f64
    }

}

/// Simulated delay. Zero is skipped entirely: on Windows even `sleep(0)` waits for the next
/// timer tick, which would silently inflate the cloud side.
async fn pause(ms: u64) {
    if ms > 0 {
        sleep(Duration::from_millis(ms)).await;
    }
}

impl GatewayState {
    fn request_id(&self) -> String {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        format!("{:08x}-{:04x}-4{:03x}-a{:03x}-{:012x}", Utc::now().timestamp() as u32, n & 0xffff, n % 0xfff, (n * 7) % 0xfff, n * 2654435761)
    }
}

pub fn router(state: Arc<GatewayState>) -> Router {
    Router::new()
        .route("/oauth2/v2.0/token", post(token))
        .route("/v1/chat/completions", post(chat))
        .route("/v1/agent/tools/{name}", post(tool))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state)
}

/// Bind and serve in the background; returns the bound address (use port 0 for ephemeral).
pub async fn spawn(addr: SocketAddr, state: GatewayState) -> Result<SocketAddr> {
    let listener = TcpListener::bind(addr).await.with_context(|| format!("bind gateway on {addr}"))?;
    let local = listener.local_addr()?;
    let app = router(Arc::new(state));
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("gateway stopped: {e}");
        }
    });
    Ok(local)
}

pub async fn serve_forever(addr: SocketAddr, state: GatewayState) -> Result<()> {
    let listener = TcpListener::bind(addr).await.with_context(|| format!("bind gateway on {addr}"))?;
    println!("mock cloud gateway listening on http://{}", listener.local_addr()?);
    println!("  token:  POST /oauth2/v2.0/token   (client_credentials, client_id={CLIENT_ID})");
    println!("  chat:   POST /v1/chat/completions (Bearer)");
    axum::serve(listener, router(Arc::new(state))).await?;
    Ok(())
}

// ---------------------------------------------------------------- OAuth2 / Entra-like tokens

#[derive(Deserialize)]
struct TokenRequest {
    grant_type: String,
    client_id: String,
    client_secret: String,
    #[serde(default)]
    scope: String,
}

#[derive(Serialize, Deserialize)]
struct Claims {
    aud: String,
    iss: String,
    sub: String,
    appid: String,
    tid: String,
    roles: Vec<String>,
    scp: String,
    iat: i64,
    nbf: i64,
    exp: i64,
}

async fn token(State(st): State<Arc<GatewayState>>, Form(req): Form<TokenRequest>) -> Response {
    // The token endpoint lives across the WAN too.
    st.stall(st.profile.wan_rtt).await;
    st.stall(st.profile.token_issue).await;

    if req.grant_type != "client_credentials" {
        return oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type");
    }
    if req.client_id != CLIENT_ID || req.client_secret != CLIENT_SECRET {
        return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client");
    }
    if !req.scope.is_empty() && req.scope != SCOPE {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_scope");
    }
    let now = Utc::now().timestamp();
    let claims = Claims {
        aud: AUDIENCE.into(),
        iss: format!("https://login.mock-entra.local/{TENANT_ID}/v2.0"),
        sub: req.client_id.clone(),
        appid: req.client_id,
        tid: TENANT_ID.into(),
        roles: vec!["Triage.Invoke".into()],
        scp: SCOPE.into(),
        iat: now,
        nbf: now,
        exp: now + st.profile.token_ttl_s as i64,
    };
    Json(json!({
        "token_type": "Bearer",
        "expires_in": st.profile.token_ttl_s,
        "ext_expires_in": st.profile.token_ttl_s,
        "access_token": sign(&claims),
    }))
    .into_response()
}

fn oauth_error(status: StatusCode, code: &str) -> Response {
    (status, Json(json!({ "error": code }))).into_response()
}

fn mac(input: &str) -> u64 {
    // FNV-1a over key || input. Not cryptographic — the point is the validation round-trip.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in SIGNING_KEY.iter().chain(input.as_bytes()) {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn sign(claims: &Claims) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT","kid":"mock-entra-1"}"#);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("claims serialize"));
    let sig = mac(&format!("{header}.{payload}"));
    format!("{header}.{payload}.{sig:016x}")
}

fn validate(headers: &HeaderMap) -> std::result::Result<Claims, &'static str> {
    let raw = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or("missing bearer token")?;
    let mut parts = raw.splitn(3, '.');
    let (Some(h), Some(p), Some(s)) = (parts.next(), parts.next(), parts.next()) else {
        return Err("malformed token");
    };
    if format!("{:016x}", mac(&format!("{h}.{p}"))) != s {
        return Err("invalid signature");
    }
    let bytes = URL_SAFE_NO_PAD.decode(p).map_err(|_| "malformed token payload")?;
    let claims: Claims = serde_json::from_slice(&bytes).map_err(|_| "malformed claims")?;
    if claims.aud != AUDIENCE {
        return Err("invalid audience");
    }
    if claims.exp < Utc::now().timestamp() {
        return Err("token expired");
    }
    if claims.tid != TENANT_ID || !claims.roles.iter().any(|r| r == "Triage.Invoke") {
        return Err("conditional access: missing Triage.Invoke role");
    }
    Ok(claims)
}

// ---------------------------------------------------------------- chat completions

#[derive(Default)]
struct Timing {
    wan: f64,
    auth: f64,
    policy: f64,
    safety: f64,
    inference: f64,
    /// Real upstream model service time beyond engine time (Azure backend).
    upstream: f64,
    tool: f64,
    serde_ms: f64,
}

impl Timing {
    fn header(&self, total: f64) -> String {
        format!(
            "wan;dur={:.2}, auth;dur={:.2}, policy;dur={:.2}, safety;dur={:.2}, inference;dur={:.2}, upstream;dur={:.2}, tool;dur={:.3}, serde;dur={:.3}, total;dur={:.2}",
            self.wan, self.auth, self.policy, self.safety, self.inference, self.upstream, self.tool, self.serde_ms, total
        )
    }
}

fn ms_since(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

async fn chat(State(st): State<Arc<GatewayState>>, headers: HeaderMap, body: Bytes) -> Response {
    let t_total = Instant::now();
    let mut timing = Timing::default();
    let request_id = st.request_id();
    let rtt = st.draw(st.profile.wan_rtt);
    let ingress = rtt / 2;
    let egress = rtt - ingress;
    timing.wan = rtt as f64;
    pause(ingress).await;

    let finish = |status: StatusCode, body: Vec<u8>, timing: &Timing, extra: &[(&'static str, String)]| {
        let mut resp = (status, [(header::CONTENT_TYPE, "application/json")], body).into_response();
        let h = resp.headers_mut();
        h.insert("server-timing", HeaderValue::from_str(&timing.header(ms_since(t_total))).unwrap());
        h.insert("apim-request-id", HeaderValue::from_str(&request_id).unwrap());
        for (k, v) in extra {
            if let Ok(v) = HeaderValue::from_str(v) {
                h.insert(*k, v);
            }
        }
        resp
    };

    // 1. Quota admission (APIM / TPM limiter): over-quota callers get a 429 and must back off.
    let Ok(_permit) = st.inflight.clone().try_acquire_owned() else {
        pause(egress).await;
        let body = json!({ "error": { "code": "429", "message": format!("Rate limit is exceeded. Try again in {} milliseconds.", st.profile.retry_after_ms) } });
        return finish(
            StatusCode::TOO_MANY_REQUESTS,
            serde_json::to_vec(&body).unwrap(),
            &timing,
            &[("retry-after-ms", st.profile.retry_after_ms.to_string()), ("retry-after", "1".into())],
        );
    };

    // 2. Entra-like bearer validation + conditional access.
    let t = Instant::now();
    let claims = validate(&headers);
    st.stall(st.profile.jwt_validate).await;
    timing.auth = ms_since(t);
    if let Err(reason) = claims {
        pause(egress).await;
        let body = json!({ "error": { "code": "401", "message": reason } });
        return finish(StatusCode::UNAUTHORIZED, serde_json::to_vec(&body).unwrap(), &timing, &[]);
    }

    // 3. Envelope deserialization.
    let t = Instant::now();
    let req: ChatRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            let body = json!({ "error": { "code": "400", "message": e.to_string() } });
            return finish(StatusCode::BAD_REQUEST, serde_json::to_vec(&body).unwrap(), &timing, &[]);
        }
    };
    timing.serde_ms += ms_since(t);

    // 4. Routing / policy (tenant routing, DLP labels, deployment selection).
    let t = Instant::now();
    st.stall(st.profile.policy).await;
    let region = REGIONS[(st.draw(MsRange::new(0, REGIONS.len() as u64 - 1))) as usize];
    timing.policy = ms_since(t);

    // 5. Content safety (prompt shields in, output filter out — charged together).
    let t = Instant::now();
    st.stall(st.profile.safety).await;
    timing.safety = ms_since(t);

    // 6. Upstream inference.
    let t = Instant::now();
    let prompt_chars: usize = req.messages.iter().map(|m| m.content.len()).sum();
    let upstream = match &st.backend {
        Backend::Synthetic => {
            st.stall(st.profile.inference).await;
            let verdict = match &req.x_enterprise_context {
                Some(ctx) => heuristic_verdict(ctx.event.kind),
                None => heuristic_verdict(crate::events::EventKind::PortScan),
            };
            let content = serde_json::to_string(&verdict).unwrap();
            let completion = (content.len() / 4) as u64;
            Ok((content, (prompt_chars / 4) as u64, completion))
        }
        Backend::Ollama { url, model, think } => forward_to_ollama(&st.http, url, model, *think, &req).await,
        Backend::Azure { endpoint, api_key, model, max_tokens, reasoning_effort } => {
            match forward_to_azure(&st.http, endpoint, api_key, model, *max_tokens, reasoning_effort.clone(), &req).await {
                Ok((content, pin, pout, engine)) => {
                    // Real model time if the provider reports it; the rest of the upstream call is
                    // service overhead + network from this runtime to the model.
                    if let Some(engine) = engine {
                        timing.upstream = (ms_since(t) - engine).max(0.0);
                    }
                    Ok((content, pin, pout))
                }
                Err(e) => Err(e),
            }
        }
    };
    timing.inference = ms_since(t) - timing.upstream;
    let (content, prompt_tokens, completion_tokens) = match upstream {
        Ok(v) => v,
        Err(e) => {
            pause(egress).await;
            let body = json!({ "error": { "code": "502", "message": format!("upstream: {e}") } });
            return finish(StatusCode::BAD_GATEWAY, serde_json::to_vec(&body).unwrap(), &timing, &[]);
        }
    };
    // Real upstreams report real token counts; only simulated upstreams get the profile's meta-prompt.
    let prompt_tokens = match st.backend {
        Backend::Azure { .. } => prompt_tokens,
        _ => prompt_tokens + st.profile.meta_prompt_tokens,
    };

    // 7. Response serialization (Azure-style payload incl. content-filter annotations).
    let t = Instant::now();
    let resp = ChatResponse {
        id: format!("chatcmpl-{request_id}"),
        object: "chat.completion".into(),
        created: Utc::now().timestamp(),
        model: req.model,
        choices: vec![Choice {
            index: 0,
            message: ResponseMessage { role: "assistant".into(), content: Some(content) },
            finish_reason: Some("stop".into()),
        }],
        usage: Some(Usage { prompt_tokens, completion_tokens, total_tokens: prompt_tokens + completion_tokens, latency_checkpoint: None }),
        prompt_filter_results: Some(json!([{
            "prompt_index": 0,
            "content_filter_results": {
                "hate": { "filtered": false, "severity": "safe" },
                "self_harm": { "filtered": false, "severity": "safe" },
                "sexual": { "filtered": false, "severity": "safe" },
                "violence": { "filtered": false, "severity": "safe" },
                "jailbreak": { "filtered": false, "detected": false }
            }
        }])),
    };
    let bytes = serde_json::to_vec(&resp).unwrap();
    timing.serde_ms += ms_since(t);

    pause(egress).await;
    finish(StatusCode::OK, bytes, &timing, &[("x-ms-region", region.to_string())])
}

async fn forward_to_ollama(
    http: &reqwest::Client,
    url: &str,
    model: &str,
    think: Option<bool>,
    req: &ChatRequest,
) -> Result<(String, u64, u64)> {
    let system = req.messages.iter().find(|m| m.role == "system").map(|m| m.content.clone()).unwrap_or_default();
    let Some(user) = req.messages.iter().rev().find(|m| m.role == "user").map(|m| m.content.clone()) else {
        bail!("no user message");
    };
    let body = serde_json::to_vec(&ollama::chat_request(model, think, ollama::OutputFormat::Schema, system, user))?;
    let bytes = ollama::post_chat(http, url, body).await?;
    let resp: OllamaChatResponse = serde_json::from_slice(&bytes)?;
    Ok((resp.message.content, resp.prompt_eval_count, resp.eval_count))
}

async fn forward_to_azure(
    http: &reqwest::Client,
    endpoint: &str,
    api_key: &str,
    model: &str,
    max_tokens: u32,
    reasoning_effort: Option<String>,
    req: &ChatRequest,
) -> Result<(String, u64, u64, Option<f64>)> {
    let upstream = ChatRequest {
        model: model.to_string(),
        messages: req.messages.clone(),
        max_completion_tokens: Some(max_tokens),
        response_format: Some(json!({ "type": "json_object" })),
        reasoning_effort,
        x_enterprise_context: None,
    };
    let resp = http.post(endpoint).header("api-key", api_key).json(&upstream).send().await?;
    let status = resp.status();
    let bytes = resp.bytes().await?;
    if !status.is_success() {
        bail!("upstream {status}: {}", String::from_utf8_lossy(&bytes).chars().take(200).collect::<String>());
    }
    let parsed: ChatResponse = serde_json::from_slice(&bytes)?;
    let content = parsed.choices.first().and_then(|c| c.message.content.clone()).unwrap_or_default();
    let usage = parsed.usage.unwrap_or_default();
    let engine = usage.latency_checkpoint.as_ref().map(|lc| lc.engine_ttlt_ms).filter(|v| *v > 0.0);
    Ok((content, usage.prompt_tokens, usage.completion_tokens, engine))
}

fn respond(t_total: Instant, request_id: &str, status: StatusCode, body: Vec<u8>, timing: &Timing, extra: &[(&'static str, String)]) -> Response {
    let mut resp = (status, [(header::CONTENT_TYPE, "application/json")], body).into_response();
    let h = resp.headers_mut();
    h.insert("server-timing", HeaderValue::from_str(&timing.header(ms_since(t_total))).unwrap());
    h.insert("apim-request-id", HeaderValue::from_str(request_id).unwrap());
    for (k, v) in extra {
        if let Ok(v) = HeaderValue::from_str(v) {
            h.insert(*k, v);
        }
    }
    resp
}

/// Agent tool action behind the cloud boundary (arm A): WAN → quota → auth → policy → tool → WAN.
async fn tool(State(st): State<Arc<GatewayState>>, Path(name): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    let t_total = Instant::now();
    let mut timing = Timing::default();
    let request_id = st.request_id();
    let rtt = st.draw(st.profile.wan_rtt);
    let (ingress, egress) = (rtt / 2, rtt - rtt / 2);
    timing.wan = rtt as f64;
    pause(ingress).await;

    let Ok(_permit) = st.inflight.clone().try_acquire_owned() else {
        pause(egress).await;
        let body = json!({ "error": { "code": "429", "message": "Rate limit is exceeded." } });
        return respond(
            t_total,
            &request_id,
            StatusCode::TOO_MANY_REQUESTS,
            serde_json::to_vec(&body).unwrap(),
            &timing,
            &[("retry-after-ms", st.profile.retry_after_ms.to_string())],
        );
    };

    let t = Instant::now();
    let claims = validate(&headers);
    st.stall(st.profile.jwt_validate).await;
    timing.auth = ms_since(t);
    if let Err(reason) = claims {
        pause(egress).await;
        let body = json!({ "error": { "code": "401", "message": reason } });
        return respond(t_total, &request_id, StatusCode::UNAUTHORIZED, serde_json::to_vec(&body).unwrap(), &timing, &[]);
    }

    let t = Instant::now();
    let args: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            let body = json!({ "error": { "code": "400", "message": e.to_string() } });
            return respond(t_total, &request_id, StatusCode::BAD_REQUEST, serde_json::to_vec(&body).unwrap(), &timing, &[]);
        }
    };
    timing.serde_ms += ms_since(t);

    let t = Instant::now();
    st.stall(st.profile.policy).await;
    timing.policy = ms_since(t);

    let t = Instant::now();
    let result = tools::execute(&name, &args, &st.store);
    timing.tool = ms_since(t);

    let (status, out) = match result {
        Ok(v) => (StatusCode::OK, v),
        Err(e) => (StatusCode::NOT_FOUND, json!({ "error": { "code": "404", "message": e.to_string() } })),
    };
    let t = Instant::now();
    let bytes = serde_json::to_vec(&out).unwrap();
    timing.serde_ms += ms_since(t);
    pause(egress).await;
    respond(t_total, &request_id, status, bytes, &timing, &[])
}
