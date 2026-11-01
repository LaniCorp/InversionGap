//! The compound agent loop and its four architectural arms.
//!
//! Every event runs the same 4-step autonomous loop:
//!   1. model  — context extraction
//!   2. tool   — threat-intel / CMDB lookup
//!   3. model  — reasoning/planning → triage verdict
//!   4. tool   — state update / ticketing
//!
//! What differs per arm is *where* each step runs, which is exactly what the orchestration tax
//! depends on:
//!   A  Cloud-hosted agent        — runtime, tools and state behind the cloud boundary; every step is
//!                                  a gateway action (WAN, auth, policy); model steps are forwarded
//!                                  by the gateway to the cloud model
//!   B  Sovereign orchestrator    — loop and tools in this process; model steps call the cloud model
//!   C  Fully local edge          — loop, tools and model all local; nothing leaves the host
//!   D  Hybrid sovereign router   — C for every event; only low-confidence or critical verdicts are
//!                                  re-planned by the cloud model (step 3 only, minimal context)

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{Result, anyhow};
use clap::ValueEnum;
use serde_json::{Value, json};

use crate::cloud::CloudHarness;
use crate::edge::EdgeActor;
use crate::events::SecurityEvent;
use crate::metrics::TaskSample;
use crate::tools;
use crate::triage::{SYSTEM_PROMPT, Severity, Verdict, parse_verdict, user_prompt};
use crate::wire::ChatMessage;

pub const EXTRACT_PROMPT: &str = "You are the context-extraction step of a SOC agent. From the event, return compact JSON only: \
{\"host\":\"...\",\"user\":\"...\",\"src_ip\":\"...\",\"indicators\":[\"<short indicator>\"],\"summary\":\"<one line>\"}";

fn plan_prompt() -> String {
    format!("{SYSTEM_PROMPT} Use the extracted context and the intel lookup result provided with the event.")
}

fn plan_user(ev: &SecurityEvent, extraction: &str, intel: &Value) -> String {
    format!("{}\nExtracted context: {}\nIntel lookup: {}", user_prompt(ev), extraction.trim(), intel)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ValueEnum)]
pub enum Arm {
    A,
    B,
    C,
    D,
}

impl Arm {
    pub fn title(self) -> &'static str {
        match self {
            Arm::A => "A · Cloud-hosted agent",
            Arm::B => "B · Sovereign orchestrator + cloud model",
            Arm::C => "C · Fully local edge",
            Arm::D => "D · Hybrid sovereign router",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Arm::A => "A",
            Arm::B => "B",
            Arm::C => "C",
            Arm::D => "D",
        }
    }
}

pub struct Escalation {
    pub below_confidence: f32,
    pub on_critical: bool,
}

impl Escalation {
    fn needed(&self, v: &Verdict) -> bool {
        v.confidence < self.below_confidence || (self.on_critical && v.severity == Severity::Critical)
    }
}

pub struct Agent {
    /// Arm A: points at the cloud gateway (model forwarding + tool actions).
    pub hosted: Option<CloudHarness>,
    /// Arms B and D: the cloud model, called directly from this machine.
    pub direct: Option<CloudHarness>,
    pub edge: Option<EdgeActor>,
    pub escalation: Escalation,
    /// Local state store for tools run in-process.
    store: Mutex<HashMap<u32, Value>>,
}

impl Agent {
    pub fn new(hosted: Option<CloudHarness>, direct: Option<CloudHarness>, edge: Option<EdgeActor>, escalation: Escalation) -> Self {
        Self { hosted, direct, edge, escalation, store: Mutex::new(HashMap::new()) }
    }

    pub async fn run(&self, arm: Arm, ev: &SecurityEvent) -> TaskSample {
        let t0 = Instant::now();
        let mut s = TaskSample { event_id: ev.id, kind: Some(ev.kind), ..Default::default() };
        let result = match arm {
            Arm::A => self.cloud_hosted(ev, &mut s).await,
            Arm::B => self.sovereign_cloud_model(ev, &mut s).await,
            Arm::C => self.fully_local(ev, &mut s).await,
            Arm::D => self.hybrid(ev, &mut s).await,
        };
        match result {
            Ok(v) => s.verdict = Some(v),
            Err(e) => s.error = Some(format!("{e:#}")),
        }
        s.latency_ms = t0.elapsed().as_secs_f64() * 1e3;
        s
    }

    // ------------------------------------------------------------------ the four loops

    async fn cloud_hosted(&self, ev: &SecurityEvent, s: &mut TaskSample) -> Result<Verdict> {
        let hosted = self.hosted.as_ref().ok_or_else(|| anyhow!("arm A needs a cloud gateway"))?;
        let extraction = cloud_model_step(hosted, vec![ChatMessage::system(EXTRACT_PROMPT), ChatMessage::user(user_prompt(ev))], ev, s).await?;
        let intel = cloud_tool_step(hosted, "intel_lookup", &tools::intel_args(ev), ev.id, s).await?;
        let plan = cloud_model_step(hosted, vec![ChatMessage::system(plan_prompt()), ChatMessage::user(plan_user(ev, &extraction, &intel))], ev, s).await?;
        let verdict = parse_verdict(&plan)?;
        cloud_tool_step(hosted, "ticket_update", &ticket_args(ev, &verdict), ev.id, s).await?;
        Ok(verdict)
    }

    async fn sovereign_cloud_model(&self, ev: &SecurityEvent, s: &mut TaskSample) -> Result<Verdict> {
        let direct = self.direct.as_ref().ok_or_else(|| anyhow!("arm B needs a cloud model"))?;
        let extraction = cloud_model_step(direct, vec![ChatMessage::system(EXTRACT_PROMPT), ChatMessage::user(user_prompt(ev))], ev, s).await?;
        let intel = self.local_tool(s, || tools::intel_lookup(&tools::intel_args(ev)));
        let plan = cloud_model_step(direct, vec![ChatMessage::system(plan_prompt()), ChatMessage::user(plan_user(ev, &extraction, &intel))], ev, s).await?;
        let verdict = parse_verdict(&plan)?;
        self.local_tool(s, || tools::ticket_update(&self.store, &ticket_args(ev, &verdict)));
        Ok(verdict)
    }

    async fn fully_local(&self, ev: &SecurityEvent, s: &mut TaskSample) -> Result<Verdict> {
        let edge = self.edge.as_ref().ok_or_else(|| anyhow!("arm C needs a local model"))?;
        let extraction = edge_model_step(edge, EXTRACT_PROMPT.to_string(), user_prompt(ev), s).await?;
        let intel = self.local_tool(s, || tools::intel_lookup(&tools::intel_args(ev)));
        let plan = edge_model_step(edge, plan_prompt(), plan_user(ev, &extraction, &intel), s).await?;
        let verdict = parse_verdict(&plan)?;
        self.local_tool(s, || tools::ticket_update(&self.store, &ticket_args(ev, &verdict)));
        Ok(verdict)
    }

    async fn hybrid(&self, ev: &SecurityEvent, s: &mut TaskSample) -> Result<Verdict> {
        let edge = self.edge.as_ref().ok_or_else(|| anyhow!("arm D needs a local model"))?;
        let direct = self.direct.as_ref().ok_or_else(|| anyhow!("arm D needs a cloud model"))?;
        let extraction = edge_model_step(edge, EXTRACT_PROMPT.to_string(), user_prompt(ev), s).await?;
        let intel = self.local_tool(s, || tools::intel_lookup(&tools::intel_args(ev)));
        let local_plan = edge_model_step(edge, plan_prompt(), plan_user(ev, &extraction, &intel), s).await?;
        // An unparseable local verdict counts as low confidence and escalates.
        let verdict = match parse_verdict(&local_plan) {
            Ok(v) if !self.escalation.needed(&v) => v,
            _ => {
                s.escalated = true;
                let plan = cloud_model_step(direct, vec![ChatMessage::system(plan_prompt()), ChatMessage::user(plan_user(ev, &extraction, &intel))], ev, s).await?;
                parse_verdict(&plan)?
            }
        };
        self.local_tool(s, || tools::ticket_update(&self.store, &ticket_args(ev, &verdict)));
        Ok(verdict)
    }

    fn local_tool(&self, s: &mut TaskSample, f: impl FnOnce() -> Value) -> Value {
        let t = Instant::now();
        let out = f();
        s.tool_ms += t.elapsed().as_secs_f64() * 1e3;
        s.tool_calls += 1;
        out
    }
}

fn ticket_args(ev: &SecurityEvent, v: &Verdict) -> Value {
    json!({ "event_id": ev.id, "severity": v.severity.as_str(), "action": v.action.as_str(), "category": v.category })
}

// ------------------------------------------------------------------ step accounting

/// A model step across the host boundary. Everything except engine time is orchestration.
async fn cloud_model_step(cloud: &CloudHarness, messages: Vec<ChatMessage>, ev: &SecurityEvent, s: &mut TaskSample) -> Result<String> {
    let mut st = TaskSample::default();
    let out = cloud.complete(messages, ev, &mut st).await;
    absorb_cloud(s, &st);
    s.model_calls += 1;
    out
}

/// A tool step across the host boundary (arm A only).
async fn cloud_tool_step(cloud: &CloudHarness, name: &str, args: &Value, trace: u32, s: &mut TaskSample) -> Result<Value> {
    let mut st = TaskSample::default();
    let out = cloud.call_tool(name, args, trace, &mut st).await;
    absorb_cloud(s, &st);
    s.tool_calls += 1;
    out
}

fn absorb_cloud(s: &mut TaskSample, st: &TaskSample) {
    s.network_ms += st.network_ms;
    s.auth_ms += st.auth_ms;
    s.gateway_ms += st.gateway_ms;
    s.queue_ms += st.queue_ms;
    s.inference_ms += st.inference_ms;
    s.tool_ms += st.tool_ms;
    s.serde_us += st.serde_us;
    s.orch_ms += st.network_ms + st.auth_ms + st.gateway_ms + st.queue_ms;
    s.bytes_tx += st.bytes_tx;
    s.bytes_rx += st.bytes_rx;
    s.bytes_off_host += st.bytes_tx;
    s.retries += st.retries;
    s.token_fetched |= st.token_fetched;
    s.tokens_in += st.tokens_in;
    s.tokens_out += st.tokens_out;
    s.boundary_calls += 1;
}

/// A local model step: loopback IPC is (tiny) orchestration; scheduler queue is GPU contention.
async fn edge_model_step(edge: &EdgeActor, system: String, user: String, s: &mut TaskSample) -> Result<String> {
    let mut st = TaskSample::default();
    let out = edge.complete(system, user, &mut st).await;
    s.orch_ms += st.network_ms;
    s.network_ms += st.network_ms;
    s.contention_ms += st.queue_ms;
    s.inference_ms += st.inference_ms;
    s.serde_us += st.serde_us;
    s.local_tokens += st.tokens_in + st.tokens_out;
    s.model_calls += 1;
    out
}
