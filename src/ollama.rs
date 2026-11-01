//! Thin reqwest client for the Ollama HTTP API (no SDK — we want full control of the wire).

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use clap::ValueEnum;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::events::SecurityEvent;
use crate::triage::{SYSTEM_PROMPT, json_schema, user_prompt};
use crate::wire::{ChatMessage, OllamaChatRequest, OllamaOptions};

/// Kept identical for warm-up and the timed run: changing num_ctx forces a model reload.
pub const NUM_CTX: u32 = 2048;
pub const NUM_PREDICT: i32 = 96;
pub const KEEP_ALIVE: &str = "30m";

pub fn normalize(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

/// How the model is constrained to emit a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Full JSON schema (grammar-constrained; strictest, but grammar sampling serializes batched slots)
    Schema,
    /// Generic JSON mode
    Json,
    /// Unconstrained — rely on the prompt and the lenient parser
    None,
}

impl OutputFormat {
    fn value(self) -> Option<Value> {
        match self {
            OutputFormat::Schema => Some(json_schema()),
            OutputFormat::Json => Some(Value::String("json".into())),
            OutputFormat::None => None,
        }
    }
}

pub fn chat_request(model: &str, think: Option<bool>, format: OutputFormat, system: String, user: String) -> OllamaChatRequest {
    OllamaChatRequest {
        model: model.to_string(),
        messages: vec![ChatMessage::system(system), ChatMessage::user(user)],
        stream: false,
        think,
        format: format.value(),
        keep_alive: KEEP_ALIVE.to_string(),
        options: OllamaOptions { temperature: 0.0, num_predict: NUM_PREDICT, num_ctx: NUM_CTX },
    }
}

pub fn triage_request(model: &str, think: Option<bool>, format: OutputFormat, ev: &SecurityEvent) -> OllamaChatRequest {
    chat_request(model, think, format, SYSTEM_PROMPT.to_string(), user_prompt(ev))
}

/// POST a pre-serialized body to `/api/chat` and return the raw response bytes.
pub async fn post_chat(http: &reqwest::Client, url: &str, body: Vec<u8>) -> Result<Bytes> {
    let resp = http
        .post(format!("{url}/api/chat"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .with_context(|| format!("POST {url}/api/chat"))?;
    let status = resp.status();
    let bytes = resp.bytes().await?;
    if !status.is_success() {
        bail!("ollama {status}: {}", String::from_utf8_lossy(&bytes).chars().take(200).collect::<String>());
    }
    Ok(bytes)
}

pub async fn list_models(http: &reqwest::Client, url: &str) -> Result<Vec<String>> {
    #[derive(Deserialize)]
    struct Tags {
        models: Vec<Tag>,
    }
    #[derive(Deserialize)]
    struct Tag {
        name: String,
    }
    let tags: Tags = http
        .get(format!("{url}/api/tags"))
        .send()
        .await
        .with_context(|| format!("Ollama not reachable at {url}"))?
        .error_for_status()?
        .json()
        .await?;
    Ok(tags.models.into_iter().map(|t| t.name).collect())
}

pub async fn capabilities(http: &reqwest::Client, url: &str, model: &str) -> Result<Vec<String>> {
    #[derive(Deserialize)]
    struct Show {
        #[serde(default)]
        capabilities: Vec<String>,
    }
    let show: Show = http
        .post(format!("{url}/api/show"))
        .json(&json!({ "model": model }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(show.capabilities)
}

/// Models currently resident in VRAM: (name, size_vram bytes).
pub async fn running(http: &reqwest::Client, url: &str) -> Result<Vec<(String, u64)>> {
    #[derive(Deserialize)]
    struct Ps {
        #[serde(default)]
        models: Vec<PsModel>,
    }
    #[derive(Deserialize)]
    struct PsModel {
        name: String,
        #[serde(default)]
        size_vram: u64,
    }
    let ps: Ps = http.get(format!("{url}/api/ps")).send().await?.error_for_status()?.json().await?;
    Ok(ps.models.into_iter().map(|m| (m.name, m.size_vram)).collect())
}

/// Thinking-capable models (qwen3, deepseek-r1, ...) get `think: false` — triage wants the
/// answer, not a chain of thought. Non-thinking models get no flag at all.
pub fn think_flag(caps: &[String]) -> Option<bool> {
    caps.iter().any(|c| c == "thinking").then_some(false)
}

pub fn pick_model(available: &[String], requested: Option<&str>) -> Result<String> {
    if let Some(req) = requested {
        let hit = available.iter().find(|m| *m == req || **m == format!("{req}:latest"));
        return match hit {
            Some(m) => Ok(m.clone()),
            None => bail!("model '{req}' not found. Available: {}. Try `ollama pull {req}`", available.join(", ")),
        };
    }
    const PREFERRED: [&str; 7] = ["phi3", "phi4", "llama3", "qwen", "gemma", "mistral", "granite"];
    PREFERRED
        .iter()
        .find_map(|p| available.iter().find(|m| m.starts_with(p)))
        .or_else(|| available.first())
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no models installed. Try `ollama pull phi3`"))
}

/// Loopback or RFC1918 — hardware you own, data never crosses a trust boundary.
pub fn is_sovereign(url: &str) -> bool {
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split(['/', ':'])
        .next()
        .unwrap_or("");
    let host = host.trim_start_matches('[');
    if host == "localhost" || host.is_empty() || host.starts_with("127.") || host == "::1" || host.ends_with(".localhost") {
        return true;
    }
    if host.starts_with("10.") || host.starts_with("192.168.") {
        return true;
    }
    if let Some(rest) = host.strip_prefix("172.") {
        if let Some(Ok(second)) = rest.split('.').next().map(str::parse::<u8>) {
            return (16..=31).contains(&second);
        }
    }
    false
}
