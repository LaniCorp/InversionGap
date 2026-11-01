//! Triage contract shared by both harnesses: prompt, output schema and verdict parsing.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::events::{EventKind, SecurityEvent};

pub const SYSTEM_PROMPT: &str = "You are a high-frequency SOC triage engine. Classify the security/system event. \
Respond ONLY with compact JSON: {\"severity\":\"low|medium|high|critical\",\"action\":\"ignore|monitor|escalate|block\",\
\"category\":\"<short_snake_case>\",\"confidence\":<0.0-1.0>}";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Ignore,
    Monitor,
    Escalate,
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    pub severity: Severity,
    pub action: Action,
    pub category: String,
    pub confidence: f32,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Low => "low",
            Severity::Medium => "medium",
            Severity::High => "high",
            Severity::Critical => "critical",
        }
    }
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Ignore => "ignore",
            Action::Monitor => "monitor",
            Action::Escalate => "escalate",
            Action::Block => "block",
        }
    }
}

pub fn user_prompt(ev: &SecurityEvent) -> String {
    format!("Event #{}: {}", ev.id, ev.summary())
}

/// JSON schema handed to Ollama's structured-output `format` field.
pub fn json_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "severity": { "type": "string", "enum": ["low", "medium", "high", "critical"] },
            "action": { "type": "string", "enum": ["ignore", "monitor", "escalate", "block"] },
            "category": { "type": "string" },
            "confidence": { "type": "number" }
        },
        "required": ["severity", "action", "category", "confidence"]
    })
}

/// Lenient parse: tolerates code fences, prose around the object and odd casing.
pub fn parse_verdict(text: &str) -> Result<Verdict> {
    #[derive(Deserialize)]
    struct Raw {
        severity: String,
        action: String,
        #[serde(default)]
        category: String,
        #[serde(default)]
        confidence: f32,
    }

    let start = text.find('{').ok_or_else(|| anyhow!("no JSON object in model output: {}", clip(text)))?;
    let end = text.rfind('}').ok_or_else(|| anyhow!("unterminated JSON in model output: {}", clip(text)))?;
    let raw: Raw = serde_json::from_str(&text[start..=end])?;
    let severity = match raw.severity.to_ascii_lowercase().as_str() {
        "low" | "info" | "informational" => Severity::Low,
        "high" => Severity::High,
        "critical" => Severity::Critical,
        _ => Severity::Medium,
    };
    let action = match raw.action.to_ascii_lowercase().as_str() {
        "ignore" | "none" | "dismiss" => Action::Ignore,
        "escalate" | "investigate" | "alert" => Action::Escalate,
        "block" | "contain" | "isolate" => Action::Block,
        _ => Action::Monitor,
    };
    Ok(Verdict { severity, action, category: raw.category, confidence: raw.confidence.clamp(0.0, 1.0) })
}

/// Rule-based verdict used by the mock gateway's synthetic upstream model.
pub fn heuristic_verdict(kind: EventKind) -> Verdict {
    let (severity, action, category, confidence) = match kind {
        EventKind::BruteForce => (Severity::High, Action::Block, "brute_force", 0.93),
        EventKind::PortScan => (Severity::Medium, Action::Monitor, "reconnaissance", 0.81),
        EventKind::PrivilegeEscalation => (Severity::Critical, Action::Escalate, "privilege_escalation", 0.95),
        EventKind::MalwareHash => (Severity::Critical, Action::Block, "malware", 0.97),
        EventKind::DnsExfiltration => (Severity::High, Action::Block, "data_exfiltration", 0.88),
        EventKind::ImpossibleTravel => (Severity::High, Action::Escalate, "account_compromise", 0.84),
        EventKind::SuspiciousPowershell => (Severity::Critical, Action::Block, "malicious_script", 0.92),
        EventKind::LateralMovement => (Severity::Critical, Action::Escalate, "lateral_movement", 0.90),
        EventKind::DiskPressure => (Severity::Medium, Action::Monitor, "capacity", 0.86),
        EventKind::ServiceCrashLoop => (Severity::High, Action::Escalate, "availability", 0.89),
        EventKind::CertExpiry => (Severity::Medium, Action::Monitor, "certificate_hygiene", 0.91),
        EventKind::BenignLogin => (Severity::Low, Action::Ignore, "benign_auth", 0.96),
    };
    Verdict { severity, action, category: category.to_string(), confidence }
}

fn clip(s: &str) -> String {
    s.chars().take(120).collect()
}
