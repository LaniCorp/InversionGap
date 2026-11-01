//! Formatting helpers and the plain-text / JSON end-of-run report.

use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::metrics::{Gap, Summary, VerdictRecord};

pub fn fmt_ms(ms: f64) -> String {
    if ms >= 10_000.0 {
        format!("{:.1} s", ms / 1e3)
    } else if ms >= 1_000.0 {
        format!("{:.2} s", ms / 1e3)
    } else {
        format!("{ms:.0} ms")
    }
}

pub fn fmt_bytes(b: u64) -> String {
    match b {
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KB", b as f64 / (1 << 10) as f64),
        b => format!("{b} B"),
    }
}

pub fn fmt_usd(v: f64) -> String {
    if v >= 100.0 { format!("${v:.0}") } else if v >= 1.0 { format!("${v:.2}") } else { format!("${v:.4}") }
}

/// The headline multiple, plus whether the edge is ahead. `None` until both sides have results.
pub fn headline(g: &Gap, sovereign_pct: f64) -> Option<(String, bool)> {
    let x = g.throughput_x?;
    Some(if x >= 1.0 {
        (format!("⚡ Edge is {x:.1}x Faster & {sovereign_pct:.0}% Sovereign"), true)
    } else {
        (format!("Cloud is {:.1}x faster — edge pool under-provisioned (raise OLLAMA_NUM_PARALLEL / add GPU endpoints)", 1.0 / x), false)
    })
}

pub fn print_summary(cloud: &Summary, edge: &Summary, gap: &Gap, sovereign_pct: f64) {
    let line = "─".repeat(78);
    println!("\n{line}");
    println!(" {:<32}{:>22}{:>22}", "", "Cloud/API-Wrapper", "Sovereign Edge");
    println!("{line}");
    let r = |k: &str, a: String, b: String| println!(" {k:<32}{a:>22}{b:>22}");
    r("Completed / failed", format!("{} / {}", cloud.completed, cloud.failed), format!("{} / {}", edge.completed, edge.failed));
    r("Wall time", format!("{:.2} s", cloud.elapsed_s), format!("{:.2} s", edge.elapsed_s));
    r("Throughput", format!("{:.2} ops/s", cloud.throughput_ops), format!("{:.2} ops/s", edge.throughput_ops));
    r("Latency avg", fmt_ms(cloud.avg_ms), fmt_ms(edge.avg_ms));
    r("Latency p50 / p95", format!("{} / {}", fmt_ms(cloud.p50_ms), fmt_ms(cloud.p95_ms)), format!("{} / {}", fmt_ms(edge.p50_ms), fmt_ms(edge.p95_ms)));
    r("Network RTT (cum.) / loopback", fmt_ms(cloud.network_ms), fmt_ms(edge.network_ms));
    r("Auth overhead (cum.)", format!("{} ({} fetch)", fmt_ms(cloud.auth_ms), cloud.token_fetches), "0 ms".into());
    r("Gateway / service overhead (cum.)", fmt_ms(cloud.gateway_ms), "0 ms".into());
    r("429 back-off / local queue (cum.)", format!("{} ({} retries)", fmt_ms(cloud.queue_ms), cloud.retries), fmt_ms(edge.queue_ms));
    r("Serde overhead (cum.)", format!("{:.0} µs", cloud.serde_us), format!("{:.0} µs", edge.serde_us));
    r("Inference (cum.)", fmt_ms(cloud.inference_ms), fmt_ms(edge.inference_ms));
    r("Payload off-host", fmt_bytes(cloud.bytes_tx), if sovereign_pct >= 100.0 { "0 B".into() } else { fmt_bytes(edge.bytes_tx) });
    r("Tokens in / out", format!("{} / {}", cloud.tokens_in, cloud.tokens_out), format!("{} / {}", edge.tokens_in, edge.tokens_out));
    r("Cost", fmt_usd(cloud.cost_usd), "$0.00".into());
    let rubric = |s: &Summary| {
        if s.rubric_graded == 0 {
            return ("—".to_string(), "—".to_string());
        }
        let pct = |n: usize| 100.0 * n as f64 / s.rubric_graded as f64;
        (
            format!("{:.0}% / {:.0}%", pct(s.rubric_severity_exact), pct(s.rubric_severity_within_one)),
            format!("{:.0}%", pct(s.rubric_action_exact)),
        )
    };
    let (cs, ca) = rubric(cloud);
    let (es, ea) = rubric(edge);
    r("Rubric severity exact / ±1", cs, es);
    r("Rubric action exact", ca, ea);
    println!("{line}");
    match headline(gap, sovereign_pct) {
        Some((h, _)) => println!(" {h}"),
        None => println!(" (no comparison — one side produced no results)"),
    }
    if let Some(x) = gap.latency_x {
        println!(" Latency multiple: {x:.1}x   Orchestration tax: {:.0}% of each cloud call ({} non-inference)", gap.tax_pct, fmt_ms(gap.tax_ms_per_call));
    }
    println!(" Token meter: {} this run → {} per 1M events", fmt_usd(gap.cloud_cost_usd), fmt_usd(gap.cloud_cost_per_million));
    println!("{line}");
}

#[derive(Serialize)]
struct JsonReport<'a, M: Serialize> {
    meta: M,
    cloud: &'a Summary,
    edge: &'a Summary,
    gap: &'a Gap,
    cloud_verdicts: &'a [VerdictRecord],
    edge_verdicts: &'a [VerdictRecord],
}

pub struct Logs<'a> {
    pub cloud: &'a [VerdictRecord],
    pub edge: &'a [VerdictRecord],
}

pub fn write_json(path: &Path, meta: impl Serialize, cloud: &Summary, edge: &Summary, gap: &Gap, logs: Logs) -> Result<()> {
    let report = JsonReport { meta, cloud, edge, gap, cloud_verdicts: logs.cloud, edge_verdicts: logs.edge };
    std::fs::write(path, serde_json::to_vec_pretty(&report)?)?;
    Ok(())
}

pub fn print_agent_summary(sums: &[(crate::agent::Arm, Summary)], paybacks: &[(String, Option<f64>)], meta: &crate::tui::AgentMeta) {
    let line = "─".repeat(24 + 18 * sums.len());
    println!("\n{line}");
    print!(" {:<23}", "Compound loop (4 steps)");
    for (a, _) in sums {
        print!("{:>18}", format!("Arm {}", a.short()));
    }
    println!("\n{line}");
    let row = |k: &str, f: &dyn Fn(&Summary) -> String| {
        print!(" {k:<23}");
        for (_, s) in sums {
            print!("{:>18}", f(s));
        }
        println!();
    };
    let n = |s: &Summary| s.calls().max(1) as f64;
    row("Completed / failed", &|s| format!("{} / {}", s.completed, s.failed));
    row("Throughput", &|s| format!("{:.2} ev/s", s.throughput_ops));
    row("Event latency avg", &|s| fmt_ms(s.avg_ms));
    row("Event latency p95", &|s| fmt_ms(s.p95_ms));
    row("Orchestration / event", &|s| fmt_ms(s.orch_ms / n(s)));
    row("  network / event", &|s| fmt_ms(s.network_ms / n(s)));
    row("  service+gw / event", &|s| fmt_ms(s.gateway_ms / n(s)));
    row("  auth / event", &|s| fmt_ms(s.auth_ms / n(s)));
    row("  429 back-off / event", &|s| fmt_ms(s.queue_ms / n(s)));
    row("Model compute / event", &|s| fmt_ms(s.inference_ms / n(s)));
    row("Tool exec / event", &|s| format!("{:.3} ms", s.tool_ms / n(s)));
    row("GPU contention / event", &|s| fmt_ms(s.contention_ms / n(s)));
    row("Cross-boundary calls", &|s| format!("{:.1} / event", s.boundary_calls as f64 / n(s)));
    row("Metered cost", &|s| fmt_usd(s.cost_usd));
    row("$ per 1M events", &|s| s.cost_per_event().map(|c| fmt_usd(c * 1e6)).unwrap_or_else(|| "—".into()));
    row("Bytes off-host", &|s| fmt_bytes(s.bytes_off_host));
    row("Events off-host", &|s| format!("{:.0}%", s.egress_pct()));
    row("Escalated (D)", &|s| if s.escalations > 0 { format!("{} ({:.0}%)", s.escalations, 100.0 * s.escalations as f64 / n(s)) } else { "—".into() });
    row("Rubric sev exact / ±1", &|s| {
        let g = s.rubric_graded.max(1) as f64;
        format!("{:.0}% / {:.0}%", 100.0 * s.rubric_severity_exact as f64 / g, 100.0 * s.rubric_severity_within_one as f64 / g)
    });
    row("Rubric action exact", &|s| format!("{:.0}%", 100.0 * s.rubric_action_exact as f64 / s.rubric_graded.max(1) as f64));
    println!("{line}");
    println!(
        " Payback of owned GPUs (${:.0}) at {:.0} events/day, net of ${:.2}/1M events local energy:",
        meta.hardware_usd, meta.events_per_day, meta.edge_energy_usd_per_m
    );
    for (l, d) in paybacks {
        match d {
            Some(d) => println!("   {l}: {d:.1} days"),
            None => println!("   {l}: no saving"),
        }
    }
    println!(" A: {}", meta.arm_a_desc);
    println!(" Cloud model for A/B/D: {}", meta.cloud_desc);
    println!("{line}");
}
