//! Per-harness accounting. Each task reports a `TaskSample`; the dashboard reads `Summary`s.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;

use crate::events::EventKind;
use crate::triage::{Severity, Verdict, heuristic_verdict};

pub type SharedStats = Arc<Mutex<HarnessStats>>;

const FEED_LEN: usize = 64;

#[derive(Debug, Clone, Default)]
pub struct TaskSample {
    pub event_id: u32,
    /// Ground-truth scenario label (never shown to the model) for rubric grading.
    pub kind: Option<EventKind>,
    pub latency_ms: f64,
    /// Cloud: wire + WAN time (end-to-end minus server processing). Edge: loopback IPC overhead.
    pub network_ms: f64,
    /// Token acquisition (incl. waiting on another task's refresh) + gateway JWT validation.
    pub auth_ms: f64,
    /// Gateway routing/policy + content-safety filters.
    pub gateway_ms: f64,
    /// Cloud: time spent backing off after 429 throttling. Edge: time parked in Ollama's scheduler.
    pub queue_ms: f64,
    /// Model time as reported by the server (gateway Server-Timing / Ollama total_duration).
    pub inference_ms: f64,
    /// Serialization + deserialization, client and (when reported) server side.
    pub serde_us: f64,
    pub bytes_tx: u64,
    pub bytes_rx: u64,
    pub retries: u32,
    pub token_fetched: bool,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub verdict: Option<Verdict>,
    pub error: Option<String>,
    // ---- agent (compound loop) accounting
    /// Orchestration overhead: everything that is neither model compute, tool work nor local
    /// batching contention (WAN, TLS, gateway/service stages, auth, 429 back-off, loopback IPC).
    pub orch_ms: f64,
    /// Waiting for a busy local GPU (Ollama scheduler queue).
    pub contention_ms: f64,
    /// Tool execution time (in-process, or inside the cloud gateway for arm A).
    pub tool_ms: f64,
    /// Bytes that left this machine.
    pub bytes_off_host: u64,
    /// Tokens processed by local models (not metered).
    pub local_tokens: u64,
    pub model_calls: u32,
    pub tool_calls: u32,
    /// Calls that crossed the host boundary (to the cloud).
    pub boundary_calls: u32,
    /// Hybrid arm: this event was escalated to the cloud model.
    pub escalated: bool,
}

#[derive(Debug, Clone)]
pub struct FeedItem {
    pub event_id: u32,
    pub latency_ms: f64,
    pub verdict: Option<Verdict>,
    pub error: Option<String>,
}

pub struct HarnessStats {
    pub label: String,
    pub total: usize,
    price_in_per_m: f64,
    price_out_per_m: f64,
    started: Option<Instant>,
    finished: Option<Instant>,
    latencies: Vec<f64>,
    completed: usize,
    failed: usize,
    sums: TaskSample,
    token_fetches: u32,
    cost_usd: f64,
    pub feed: VecDeque<FeedItem>,
    pub last_error: Option<String>,
    escalations: usize,
    events_off_host: usize,
    graded: usize,
    severity_exact: usize,
    severity_within_one: usize,
    action_exact: usize,
    log: Vec<VerdictRecord>,
}

/// One triaged event, for the JSON report.
#[derive(Debug, Clone, Serialize)]
pub struct VerdictRecord {
    pub event_id: u32,
    pub kind: Option<EventKind>,
    pub latency_ms: f64,
    pub verdict: Option<Verdict>,
    pub expected: Option<Verdict>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Summary {
    pub label: String,
    pub total: usize,
    pub completed: usize,
    pub failed: usize,
    pub started: bool,
    pub done: bool,
    pub elapsed_s: f64,
    pub throughput_ops: f64,
    pub avg_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub network_ms: f64,
    pub auth_ms: f64,
    pub gateway_ms: f64,
    pub queue_ms: f64,
    pub inference_ms: f64,
    pub serde_us: f64,
    pub bytes_tx: u64,
    pub bytes_rx: u64,
    pub retries: u32,
    pub token_fetches: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: f64,
    /// Agreement with the reference rubric (`triage::heuristic_verdict`), successful calls only.
    pub rubric_graded: usize,
    pub rubric_severity_exact: usize,
    pub rubric_severity_within_one: usize,
    pub rubric_action_exact: usize,
    pub orch_ms: f64,
    pub contention_ms: f64,
    pub tool_ms: f64,
    pub bytes_off_host: u64,
    pub local_tokens: u64,
    pub model_calls: u32,
    pub tool_calls: u32,
    pub boundary_calls: u32,
    pub escalations: usize,
    pub events_off_host: usize,
}

fn severity_rank(s: Severity) -> i32 {
    match s {
        Severity::Low => 0,
        Severity::Medium => 1,
        Severity::High => 2,
        Severity::Critical => 3,
    }
}

impl HarnessStats {
    pub fn shared(label: &str, total: usize, price_in_per_m: f64, price_out_per_m: f64) -> SharedStats {
        Arc::new(Mutex::new(Self {
            label: label.to_string(),
            total,
            price_in_per_m,
            price_out_per_m,
            started: None,
            finished: None,
            latencies: Vec::with_capacity(total),
            completed: 0,
            failed: 0,
            sums: TaskSample::default(),
            token_fetches: 0,
            cost_usd: 0.0,
            feed: VecDeque::with_capacity(FEED_LEN),
            last_error: None,
            escalations: 0,
            events_off_host: 0,
            graded: 0,
            severity_exact: 0,
            severity_within_one: 0,
            action_exact: 0,
            log: Vec::with_capacity(total),
        }))
    }

    pub fn start(&mut self) {
        self.started = Some(Instant::now());
    }

    pub fn finish(&mut self) {
        self.finished = Some(Instant::now());
    }

    pub fn record(&mut self, s: TaskSample) {
        self.latencies.push(s.latency_ms);
        let t = &mut self.sums;
        t.network_ms += s.network_ms;
        t.auth_ms += s.auth_ms;
        t.gateway_ms += s.gateway_ms;
        t.queue_ms += s.queue_ms;
        t.inference_ms += s.inference_ms;
        t.serde_us += s.serde_us;
        t.bytes_tx += s.bytes_tx;
        t.bytes_rx += s.bytes_rx;
        t.retries += s.retries;
        t.tokens_in += s.tokens_in;
        t.tokens_out += s.tokens_out;
        t.orch_ms += s.orch_ms;
        t.contention_ms += s.contention_ms;
        t.tool_ms += s.tool_ms;
        t.bytes_off_host += s.bytes_off_host;
        t.local_tokens += s.local_tokens;
        t.model_calls += s.model_calls;
        t.tool_calls += s.tool_calls;
        t.boundary_calls += s.boundary_calls;
        self.escalations += s.escalated as usize;
        self.events_off_host += (s.bytes_off_host > 0) as usize;
        if s.token_fetched {
            self.token_fetches += 1;
        }
        self.cost_usd += s.tokens_in as f64 * self.price_in_per_m / 1e6 + s.tokens_out as f64 * self.price_out_per_m / 1e6;
        match &s.error {
            Some(e) => {
                self.failed += 1;
                self.last_error = Some(e.clone());
            }
            None => self.completed += 1,
        }
        let expected = s.kind.map(heuristic_verdict);
        if let (Some(v), Some(e)) = (&s.verdict, &expected) {
            self.graded += 1;
            let d = (severity_rank(v.severity) - severity_rank(e.severity)).abs();
            self.severity_exact += (d == 0) as usize;
            self.severity_within_one += (d <= 1) as usize;
            self.action_exact += (v.action == e.action) as usize;
        }
        self.log.push(VerdictRecord {
            event_id: s.event_id,
            kind: s.kind,
            latency_ms: s.latency_ms,
            verdict: s.verdict.clone(),
            expected,
            error: s.error.clone(),
        });
        self.feed.push_front(FeedItem {
            event_id: s.event_id,
            latency_ms: s.latency_ms,
            verdict: s.verdict,
            error: s.error,
        });
        self.feed.truncate(FEED_LEN);
    }

    pub fn verdict_log(&self) -> Vec<VerdictRecord> {
        let mut log = self.log.clone();
        log.sort_by_key(|r| r.event_id);
        log
    }

    /// Most recent latencies, oldest first — for sparklines.
    pub fn recent_latencies(&self, n: usize) -> Vec<u64> {
        let skip = self.latencies.len().saturating_sub(n);
        self.latencies[skip..].iter().map(|v| v.round() as u64).collect()
    }

    pub fn summary(&self) -> Summary {
        let elapsed_s = match self.started {
            Some(s) => self.finished.unwrap_or_else(Instant::now).duration_since(s).as_secs_f64(),
            None => 0.0,
        };
        let n = self.latencies.len();
        let mut sorted = self.latencies.clone();
        sorted.sort_by(f64::total_cmp);
        let pct = |p: f64| if n == 0 { 0.0 } else { sorted[((n - 1) as f64 * p).round() as usize] };
        let t = &self.sums;
        Summary {
            label: self.label.clone(),
            total: self.total,
            completed: self.completed,
            failed: self.failed,
            started: self.started.is_some(),
            done: self.finished.is_some(),
            elapsed_s,
            throughput_ops: if elapsed_s > 0.0 { self.completed as f64 / elapsed_s } else { 0.0 },
            avg_ms: if n == 0 { 0.0 } else { sorted.iter().sum::<f64>() / n as f64 },
            p50_ms: pct(0.50),
            p95_ms: pct(0.95),
            network_ms: t.network_ms,
            auth_ms: t.auth_ms,
            gateway_ms: t.gateway_ms,
            queue_ms: t.queue_ms,
            inference_ms: t.inference_ms,
            serde_us: t.serde_us,
            bytes_tx: t.bytes_tx,
            bytes_rx: t.bytes_rx,
            retries: t.retries,
            token_fetches: self.token_fetches,
            tokens_in: t.tokens_in,
            tokens_out: t.tokens_out,
            cost_usd: self.cost_usd,
            rubric_graded: self.graded,
            rubric_severity_exact: self.severity_exact,
            rubric_severity_within_one: self.severity_within_one,
            rubric_action_exact: self.action_exact,
            orch_ms: t.orch_ms,
            contention_ms: t.contention_ms,
            tool_ms: t.tool_ms,
            bytes_off_host: t.bytes_off_host,
            local_tokens: t.local_tokens,
            model_calls: t.model_calls,
            tool_calls: t.tool_calls,
            boundary_calls: t.boundary_calls,
            escalations: self.escalations,
            events_off_host: self.events_off_host,
        }
    }
}

impl Summary {
    pub fn calls(&self) -> usize {
        self.completed + self.failed
    }

    pub fn per_call(&self, total: f64) -> f64 {
        if self.calls() == 0 { 0.0 } else { total / self.calls() as f64 }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Gap {
    /// Edge throughput ÷ cloud throughput.
    pub throughput_x: Option<f64>,
    /// Cloud mean latency ÷ edge mean latency.
    pub latency_x: Option<f64>,
    /// Share of mean cloud latency that is not model inference.
    pub tax_pct: f64,
    pub tax_ms_per_call: f64,
    pub cloud_cost_usd: f64,
    pub cloud_cost_per_million: f64,
    pub cloud_egress_bytes: u64,
}

pub fn gap(cloud: &Summary, edge: &Summary) -> Gap {
    let both = cloud.completed > 0 && edge.completed > 0;
    let cloud_inference = cloud.per_call(cloud.inference_ms);
    let tax_ms = (cloud.avg_ms - cloud_inference).max(0.0);
    Gap {
        throughput_x: (both && cloud.throughput_ops > 0.0).then(|| edge.throughput_ops / cloud.throughput_ops),
        latency_x: (both && edge.avg_ms > 0.0).then(|| cloud.avg_ms / edge.avg_ms),
        tax_pct: if cloud.avg_ms > 0.0 { 100.0 * tax_ms / cloud.avg_ms } else { 0.0 },
        tax_ms_per_call: tax_ms,
        cloud_cost_usd: cloud.cost_usd,
        cloud_cost_per_million: if cloud.completed > 0 { cloud.cost_usd / cloud.completed as f64 * 1e6 } else { 0.0 },
        cloud_egress_bytes: cloud.bytes_tx,
    }
}

/// Days for owned hardware to pay for itself from per-event savings at a daily volume.
pub fn payback_days(hardware_usd: f64, events_per_day: f64, saving_per_event_usd: f64) -> Option<f64> {
    (saving_per_event_usd > 0.0 && events_per_day > 0.0).then(|| hardware_usd / (saving_per_event_usd * events_per_day))
}

impl Summary {
    /// Metered cloud spend per successfully triaged event.
    pub fn cost_per_event(&self) -> Option<f64> {
        (self.completed > 0).then(|| self.cost_usd / self.completed as f64)
    }

    pub fn egress_pct(&self) -> f64 {
        if self.calls() == 0 { 0.0 } else { 100.0 * self.events_off_host as f64 / self.calls() as f64 }
    }
}
