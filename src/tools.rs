//! Agent tools: deterministic stand-ins for a threat-intel/CMDB lookup and a ticketing system.
//!
//! The same functions run in-process for the sovereign arms (B, C, D) and behind the mock cloud
//! gateway for the cloud-hosted arm (A), so tool *work* is identical and only the hop differs.
//! Tools see the event's observable fields only, never its hidden scenario label.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::events::SecurityEvent;

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn is_internal(ip: &str) -> bool {
    ip.starts_with("10.") || ip.starts_with("192.168.") || {
        ip.strip_prefix("172.")
            .and_then(|r| r.split('.').next())
            .and_then(|o| o.parse::<u8>().ok())
            .is_some_and(|o| (16..=31).contains(&o))
    }
}

/// Arguments for `intel_lookup`, taken from the event as an orchestrator would pass them.
pub fn intel_args(ev: &SecurityEvent) -> Value {
    json!({ "host": ev.host, "user": ev.user, "src_ip": ev.src_ip })
}

/// Step 2: reputation / asset-criticality / identity lookup.
pub fn intel_lookup(args: &Value) -> Value {
    let host = args["host"].as_str().unwrap_or("");
    let user = args["user"].as_str().unwrap_or("");
    let ip = args["src_ip"].as_str().unwrap_or("");
    let reputation = if is_internal(ip) {
        "internal"
    } else {
        match fnv(ip) % 100 {
            81.. => "malicious",
            56..=80 => "suspicious",
            _ => "unknown",
        }
    };
    let criticality = if ["prod", "dc", "vault", "fw"].iter().any(|k| host.contains(k)) {
        "high"
    } else if ["build", "k8s", "web"].iter().any(|k| host.contains(k)) {
        "medium"
    } else {
        "low"
    };
    let privileged = matches!(user, "root" | "admin" | "SYSTEM") || user.starts_with("svc_");
    json!({
        "src_ip_reputation": reputation,
        "asset_criticality": criticality,
        "user_privileged": privileged,
        "prior_incidents_30d": fnv(&format!("{host}/{user}")) % 4,
    })
}

/// Step 4: open (or auto-close) a ticket for the verdict and persist it in `store`.
pub fn ticket_update(store: &Mutex<HashMap<u32, Value>>, args: &Value) -> Value {
    let event_id = args["event_id"].as_u64().unwrap_or(0) as u32;
    let severity = args["severity"].as_str().unwrap_or("medium");
    let action = args["action"].as_str().unwrap_or("monitor");
    let status = if action == "ignore" { "auto-closed" } else { "open" };
    let queue = match severity {
        "critical" => "incident-response",
        "high" => "soc-tier2",
        _ => "soc-tier1",
    };
    let ticket = json!({ "ticket_id": format!("SOC-{event_id:06}"), "status": status, "queue": queue, "severity": severity, "action": action });
    store.lock().unwrap().insert(event_id, ticket.clone());
    ticket
}

pub fn execute(name: &str, args: &Value, store: &Mutex<HashMap<u32, Value>>) -> Result<Value> {
    match name {
        "intel_lookup" => Ok(intel_lookup(args)),
        "ticket_update" => Ok(ticket_update(store, args)),
        other => bail!("unknown tool '{other}'"),
    }
}
