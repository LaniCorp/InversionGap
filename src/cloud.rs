//! Implementation A — Cloud/API-wrapper harness.
//!
//! Serialized (by default) OpenAI-style calls through a gateway: acquire a token, wrap the event in
//! an enterprise envelope, POST, honour 429 back-off, parse the completion, then parse the verdict
//! out of the completion text. Works against the mock gateway or any real OpenAI/Azure endpoint.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use bytes::Bytes;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::events::SecurityEvent;
use crate::gateway::{CLIENT_ID, CLIENT_SECRET, SCOPE, TENANT_ID};
use crate::metrics::TaskSample;
use crate::triage::{SYSTEM_PROMPT, Verdict, parse_verdict, user_prompt};
use crate::wire::{ChatMessage, ChatRequest, ChatResponse, EnterpriseContext};

pub enum CloudAuth {
    /// OAuth2 client-credentials against the mock gateway's Entra-like token endpoint.
    MockOAuth { token_url: String },
    /// Static key header (`api-key` for Azure OpenAI).
    ApiKey { header: String, value: String },
    /// Static `Authorization: Bearer` key (OpenAI-compatible).
    Bearer { value: String },
    /// Real Entra ID token via `az account get-access-token`.
    EntraCli { resource: String },
}

struct CachedToken {
    value: String,
    expires: Instant,
}

pub struct CloudHarness {
    http: reqwest::Client,
    endpoint: String,
    model: String,
    auth: CloudAuth,
    token: Mutex<Option<CachedToken>>,
    enterprise_envelope: bool,
    max_tokens: u32,
    max_retries: u32,
    reasoning_effort: Option<String>,
    tool_base: Option<String>,
}

/// Breakdown parsed from the gateway's `Server-Timing` (or a real provider's processing header).
#[derive(Default)]
struct ServerTiming {
    wan: f64,
    auth: f64,
    policy: f64,
    safety: f64,
    inference: f64,
    /// Agent arm A: upstream model service time beyond engine time, as forwarded by the gateway.
    upstream: f64,
    /// Agent arm A: tool execution inside the gateway.
    tool: f64,
    serde_ms: f64,
    total: f64,
}

impl ServerTiming {
    fn parse(headers: &HeaderMap) -> Self {
        let mut t = Self::default();
        if let Some(v) = headers.get("server-timing").and_then(|v| v.to_str().ok()) {
            for entry in v.split(',') {
                let mut it = entry.trim().split(';');
                let name = it.next().unwrap_or("");
                let dur = it.find_map(|p| p.trim().strip_prefix("dur=")).and_then(|d| d.parse::<f64>().ok()).unwrap_or(0.0);
                match name {
                    "wan" => t.wan = dur,
                    "auth" => t.auth = dur,
                    "policy" => t.policy = dur,
                    "safety" => t.safety = dur,
                    "inference" => t.inference = dur,
                    "upstream" => t.upstream = dur,
                    "tool" => t.tool = dur,
                    "serde" => t.serde_ms = dur,
                    "total" => t.total = dur,
                    _ => {}
                }
            }
            return t;
        }
        // Real providers: OpenAI reports `openai-processing-ms`; Azure fronts with Envoy.
        for h in ["openai-processing-ms", "x-envoy-upstream-service-time"] {
            if let Some(ms) = headers.get(h).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<f64>().ok()) {
                t.inference = ms;
                t.total = ms;
                break;
            }
        }
        t
    }

    /// Time the server spent working (everything except simulated WAN).
    fn processing(&self) -> f64 {
        (self.total - self.wan).max(0.0)
    }
}

impl CloudHarness {
    pub fn new(
        endpoint: String,
        model: String,
        auth: CloudAuth,
        enterprise_envelope: bool,
        max_tokens: u32,
        reasoning_effort: Option<String>,
    ) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .pool_max_idle_per_host(64)
                .build()
                .expect("http client"),
            endpoint,
            model,
            auth,
            token: Mutex::new(None),
            enterprise_envelope,
            max_tokens,
            max_retries: 8,
            reasoning_effort,
            tool_base: None,
        }
    }

    pub async fn triage(&self, ev: &SecurityEvent) -> TaskSample {
        let t0 = Instant::now();
        let mut s = TaskSample { event_id: ev.id, kind: Some(ev.kind), ..Default::default() };
        match self.triage_inner(ev, &mut s).await {
            Ok(v) => s.verdict = Some(v),
            Err(e) => s.error = Some(format!("{e:#}")),
        }
        s.latency_ms = t0.elapsed().as_secs_f64() * 1e3;
        s
    }

    async fn triage_inner(&self, ev: &SecurityEvent, s: &mut TaskSample) -> Result<Verdict> {
        let messages = vec![ChatMessage::system(SYSTEM_PROMPT), ChatMessage::user(user_prompt(ev))];
        let content = self.complete(messages, ev, s).await?;
        let t = Instant::now();
        let verdict = parse_verdict(&content)?;
        s.serde_us += t.elapsed().as_secs_f64() * 1e6;
        Ok(verdict)
    }

    /// Tool calls (agent arm A) go to `{tool_base}/v1/agent/tools/{name}` with the same auth.
    pub fn with_tool_base(mut self, base: String) -> Self {
        self.tool_base = Some(base);
        self
    }

    /// One cloud model call (a single agent step). Accumulates into `s`; returns the raw content.
    pub async fn complete(&self, messages: Vec<ChatMessage>, ev: &SecurityEvent, s: &mut TaskSample) -> Result<String> {
        let t = Instant::now();
        let req = ChatRequest {
            model: self.model.clone(),
            messages,
            max_completion_tokens: Some(self.max_tokens),
            response_format: Some(json!({ "type": "json_object" })),
            reasoning_effort: self.reasoning_effort.clone(),
            x_enterprise_context: self.enterprise_envelope.then(|| enterprise_context(ev)),
        };
        let body = serde_json::to_vec(&req)?;
        s.serde_us += t.elapsed().as_secs_f64() * 1e6;

        let bytes = self.post(&self.endpoint, body, ev.id, s).await?;

        let t = Instant::now();
        let parsed: ChatResponse = serde_json::from_slice(&bytes)?;
        let content = parsed
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .ok_or_else(|| anyhow!("completion had no content"))?;
        s.serde_us += t.elapsed().as_secs_f64() * 1e6;
        if let Some(u) = parsed.usage {
            s.tokens_in += u.prompt_tokens;
            s.tokens_out += u.completion_tokens;
            // Real Azure OpenAI reports its own service/engine split in the body; use it to
            // separate service overhead from model time, leaving network = outside the service.
            if let Some(lc) = u.latency_checkpoint.filter(|lc| lc.service_ttlt_ms > 0.0) {
                s.network_ms = (s.network_ms - lc.service_ttlt_ms).max(0.0);
                s.gateway_ms += (lc.service_ttlt_ms - lc.engine_ttlt_ms).max(0.0);
                s.inference_ms += lc.engine_ttlt_ms;
            }
        }
        Ok(content)
    }

    /// One remote tool call through the cloud boundary (agent arm A).
    pub async fn call_tool(&self, name: &str, args: &Value, trace: u32, s: &mut TaskSample) -> Result<Value> {
        let base = self.tool_base.as_deref().ok_or_else(|| anyhow!("no tool endpoint configured"))?;
        let body = serde_json::to_vec(args)?;
        let bytes = self.post(&format!("{base}/v1/agent/tools/{name}"), body, trace, s).await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// POST with auth, 429 back-off and per-attempt timing attribution. Returns the success body.
    async fn post(&self, url: &str, body: Vec<u8>, trace: u32, s: &mut TaskSample) -> Result<Bytes> {
        let t = Instant::now();
        let (auth_header, fetched) = self.auth_header().await?;
        s.auth_ms += t.elapsed().as_secs_f64() * 1e3;
        s.token_fetched |= fetched;

        let mut attempt = 0;
        loop {
            let ta = Instant::now();
            let resp = self
                .http
                .post(url)
                .header(CONTENT_TYPE, "application/json")
                .header(auth_header.0.as_str(), auth_header.1.as_str())
                .header("x-ms-client-request-id", format!("ig-{trace}-{}-{attempt}", Utc::now().timestamp_micros()))
                .header("traceparent", format!("00-{:032x}-{:016x}-01", trace as u128 * 0x9E3779B97F4A7C15, attempt as u64 + 1))
                .body(body.clone())
                .send()
                .await
                .with_context(|| format!("POST {url}"))?;
            let status = resp.status();
            let headers = resp.headers().clone();
            let bytes = resp.bytes().await?;
            let elapsed = ta.elapsed().as_secs_f64() * 1e3;
            s.bytes_tx += body.len() as u64;
            s.bytes_rx += bytes.len() as u64;

            let st = ServerTiming::parse(&headers);
            s.network_ms += (elapsed - st.processing()).max(0.0);
            s.auth_ms += st.auth;
            s.gateway_ms += st.policy + st.safety + st.upstream;
            s.inference_ms += st.inference;
            s.tool_ms += st.tool;
            s.serde_us += st.serde_ms * 1e3;

            if status.as_u16() == 429 && attempt < self.max_retries {
                let wait = headers
                    .get("retry-after-ms")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(250 << attempt.min(5));
                s.retries += 1;
                s.queue_ms += wait as f64;
                tokio::time::sleep(Duration::from_millis(wait)).await;
                attempt += 1;
                continue;
            }
            if !status.is_success() {
                bail!("HTTP {status}: {}", String::from_utf8_lossy(&bytes).chars().take(200).collect::<String>());
            }
            return Ok(bytes);
        }
    }

    /// Returns the header to attach and whether a fresh token had to be fetched.
    async fn auth_header(&self) -> Result<((String, String), bool)> {
        match &self.auth {
            CloudAuth::ApiKey { header, value } => Ok(((header.clone(), value.clone()), false)),
            CloudAuth::Bearer { value } => Ok(((AUTHORIZATION.to_string(), format!("Bearer {value}")), false)),
            CloudAuth::MockOAuth { .. } | CloudAuth::EntraCli { .. } => {
                // Concurrent callers queue on this lock during a refresh — that wait is real auth tax.
                let mut guard = self.token.lock().await;
                if let Some(tok) = guard.as_ref() {
                    if tok.expires > Instant::now() + Duration::from_secs(30) {
                        return Ok(((AUTHORIZATION.to_string(), format!("Bearer {}", tok.value)), false));
                    }
                }
                let fresh = self.fetch_token().await?;
                let header = (AUTHORIZATION.to_string(), format!("Bearer {}", fresh.value));
                *guard = Some(fresh);
                Ok((header, true))
            }
        }
    }

    async fn fetch_token(&self) -> Result<CachedToken> {
        match &self.auth {
            CloudAuth::MockOAuth { token_url } => {
                #[derive(Deserialize)]
                struct Tok {
                    access_token: String,
                    expires_in: u64,
                }
                let tok: Tok = self
                    .http
                    .post(token_url)
                    .form(&[
                        ("grant_type", "client_credentials"),
                        ("client_id", CLIENT_ID),
                        ("client_secret", CLIENT_SECRET),
                        ("scope", SCOPE),
                    ])
                    .send()
                    .await
                    .context("token endpoint")?
                    .error_for_status()?
                    .json()
                    .await?;
                Ok(CachedToken { value: tok.access_token, expires: Instant::now() + Duration::from_secs(tok.expires_in) })
            }
            CloudAuth::EntraCli { resource } => {
                // `az` is a .cmd shim on Windows, so go through the shell.
                let mut cmd = if cfg!(windows) {
                    let mut c = tokio::process::Command::new("cmd");
                    c.args(["/C", "az"]);
                    c
                } else {
                    tokio::process::Command::new("az")
                };
                let out = cmd
                    .args(["account", "get-access-token", "--resource", resource, "-o", "json"])
                    .output()
                    .await
                    .context("running `az account get-access-token` (is the Azure CLI installed and logged in?)")?;
                if !out.status.success() {
                    bail!("az get-access-token failed: {}", String::from_utf8_lossy(&out.stderr).trim());
                }
                #[derive(Deserialize)]
                struct AzTok {
                    #[serde(rename = "accessToken")]
                    access_token: String,
                    #[serde(default)]
                    expires_on: Option<i64>,
                }
                let tok: AzTok = serde_json::from_slice(&out.stdout)?;
                let ttl = tok.expires_on.map(|e| (e - Utc::now().timestamp()).max(60) as u64).unwrap_or(3000);
                Ok(CachedToken { value: tok.access_token, expires: Instant::now() + Duration::from_secs(ttl) })
            }
            _ => unreachable!("static credentials are never fetched"),
        }
    }
}

/// The wrapper envelope a typical enterprise integration ships with every call.
fn enterprise_context(ev: &SecurityEvent) -> EnterpriseContext {
    EnterpriseContext {
        tenant_id: TENANT_ID.into(),
        correlation_id: format!("corr-{:08}-{}", ev.id, Utc::now().timestamp_micros()),
        data_classification: "Confidential".into(),
        sensitivity_label: "Confidential\\Security Operations".into(),
        user_claims: json!({
            "oid": "7f3c2a10-0000-4000-8000-00000000soc1",
            "upn": "soc-automation@lanicorp.example",
            "groups": ["SOC-Tier1", "SOC-Automation", "SecurityReaders", "All-Employees"],
            "amr": ["pwd", "mfa"],
            "device": { "compliant": true, "managed": true, "trustType": "AzureAD" }
        }),
        routing: json!({
            "region_preference": ["westeurope", "swedencentral", "eastus2"],
            "deployment": "triage-gpt-prod",
            "priority": "standard",
            "pii_redaction": true
        }),
        compliance: json!({
            "retention_days": 30,
            "dlp_policies": ["PCI-DSS", "GDPR-Art32", "ISO27001-A.12"],
            "audit": true,
            "residency": "EU"
        }),
        grounding: vec![
            "Policy SOC-001: escalate any privilege change on production hosts within 5 minutes.".into(),
            "Policy SOC-014: block source IPs with >20 failed authentications per minute.".into(),
            "Runbook RB-207: DNS tunnelling indicators include long base32 labels and TXT bursts.".into(),
        ],
        event: ev.clone(),
    }
}
