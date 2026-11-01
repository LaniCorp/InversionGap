//! Implementation B — Sovereign Edge Actor.
//!
//! Direct, highly parallel calls to local Ollama endpoints (round-robin, one endpoint per GPU).
//! No token, no gateway, no envelope, no off-host hop; verdicts land straight in native structs.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use futures::future::join_all;

use crate::events::SecurityEvent;
use crate::metrics::TaskSample;
use crate::ollama::{self, OutputFormat};
use crate::triage::{SYSTEM_PROMPT, Verdict, parse_verdict, user_prompt};
use crate::wire::OllamaChatResponse;

pub struct EdgeActor {
    http: reqwest::Client,
    pub endpoints: Vec<String>,
    pub model: String,
    think: Option<bool>,
    format: OutputFormat,
    next: AtomicUsize,
}

impl EdgeActor {
    pub fn new(endpoints: Vec<String>, model: String, think: Option<bool>, format: OutputFormat) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .pool_max_idle_per_host(64)
                .build()
                .expect("http client"),
            endpoints,
            model,
            think,
            format,
            next: AtomicUsize::new(0),
        }
    }

    /// Load the model on every endpoint (untimed). Returns seconds per endpoint.
    pub async fn warm_up(&self, events: &[SecurityEvent]) -> Vec<Result<f64>> {
        let Some(ev) = events.first() else { return vec![] };
        join_all(self.endpoints.iter().map(|url| async move {
            let t = Instant::now();
            let body = serde_json::to_vec(&ollama::triage_request(&self.model, self.think, self.format, ev))?;
            ollama::post_chat(&self.http, url, body).await?;
            Ok(t.elapsed().as_secs_f64())
        }))
        .await
    }

    pub async fn triage(&self, ev: &SecurityEvent) -> TaskSample {
        let t0 = Instant::now();
        let mut s = TaskSample { event_id: ev.id, kind: Some(ev.kind), ..Default::default() };
        match self.triage_inner(ev, &mut s).await {
            Ok(v) => s.verdict = Some(v),
            Err(e) => s.error = Some(format!("{e:#}")),
        }
        s.latency_ms = t0.elapsed().as_secs_f64() * 1e3;
        s.network_ms = (s.latency_ms - s.inference_ms - s.queue_ms).max(0.0);
        s
    }

    async fn triage_inner(&self, ev: &SecurityEvent, s: &mut TaskSample) -> Result<Verdict> {
        let content = self.complete(SYSTEM_PROMPT.to_string(), user_prompt(ev), s).await?;
        let t = Instant::now();
        let verdict = parse_verdict(&content)?;
        s.serde_us += t.elapsed().as_secs_f64() * 1e6;
        Ok(verdict)
    }

    /// One local model call (a single agent step). Accumulates into `s`; returns the raw content.
    pub async fn complete(&self, system: String, user: String, s: &mut TaskSample) -> Result<String> {
        let url = &self.endpoints[self.next.fetch_add(1, Ordering::Relaxed) % self.endpoints.len()];

        let t = Instant::now();
        let body = serde_json::to_vec(&ollama::chat_request(&self.model, self.think, self.format, system, user))?;
        s.serde_us += t.elapsed().as_secs_f64() * 1e6;
        s.bytes_tx += body.len() as u64;

        let tp = Instant::now();
        let bytes = ollama::post_chat(&self.http, url, body).await?;
        let post_ms = tp.elapsed().as_secs_f64() * 1e3;
        s.bytes_rx += bytes.len() as u64;

        let t = Instant::now();
        let resp: OllamaChatResponse = serde_json::from_slice(&bytes)?;
        s.serde_us += t.elapsed().as_secs_f64() * 1e6;

        // Ollama's total_duration includes time parked in its scheduler queue; split that out so
        // "inference" is real GPU compute (prompt eval + generation).
        let compute = (resp.prompt_eval_duration + resp.eval_duration) as f64 / 1e6;
        let total = resp.total_duration as f64 / 1e6;
        s.inference_ms += compute;
        s.queue_ms += (total - compute).max(0.0);
        s.network_ms += (post_ms - total).max(0.0);
        s.tokens_in += resp.prompt_eval_count;
        s.tokens_out += resp.eval_count;
        Ok(resp.message.content)
    }
}
