//! inversion-gap — benchmark the Orchestration Tax.
//!
//! Subcommands:
//!   run      single-call head-to-head: a cloud/API-wrapper path (`cloud.rs`, via the mock gateway or
//!            a real Azure OpenAI endpoint) and a local edge path (`edge.rs`, Ollama) triage the same
//!            deterministic event stream; a live dashboard shows where the time and money go.
//!   agent    compound 4-step agent loop (model → tool → model → tool) across four architectural
//!            arms A–D (`agent.rs`), with payback and data-egress accounting.
//!   check    verify the Ollama endpoints and model end to end.
//!   gateway  run the mock cloud gateway on its own (e.g. on a remote VM for real WAN hops).

mod agent;
mod cloud;
mod config;
mod edge;
mod events;
mod gateway;
mod metrics;
mod ollama;
mod tools;
mod report;
mod triage;
mod tui;
mod wire;

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use futures::StreamExt;
use serde_json::json;

use crate::cloud::{CloudAuth, CloudHarness};
use crate::agent::{Agent, Arm, Escalation};
use crate::config::{AgentArgs, Cli, CloudAuthKind, CloudTarget, Command, EdgeArgs, GatewayArgs, RunArgs, Schedule};
use crate::edge::EdgeActor;
use crate::events::SecurityEvent;
use crate::gateway::{Backend, BackendKind, GatewayState};
use crate::metrics::{HarnessStats, SharedStats, TaskSample, gap};
use crate::wire::OllamaChatResponse;

#[cfg(windows)]
#[link(name = "winmm")]
unsafe extern "system" {
    fn timeBeginPeriod(period_ms: u32) -> u32;
}

/// Windows' default ~15.6 ms timer tick would quantize every simulated gateway delay (and the
/// cloud client's back-off). Ask for 1 ms resolution for the life of the process.
fn high_resolution_timers() {
    #[cfg(windows)]
    unsafe {
        timeBeginPeriod(1);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    high_resolution_timers();
    match Cli::parse().command {
        Command::Run(args) => run(args).await,
        Command::Check(args) => check(args).await,
        Command::Gateway(args) => serve_gateway(args).await,
        Command::Agent(args) => run_agent(args).await,
    }
}

/// Resolved edge configuration: reachable endpoints, model, thinking flag.
struct EdgeSetup {
    endpoints: Vec<String>,
    model: String,
    think: Option<bool>,
    format: ollama::OutputFormat,
}

async fn resolve_edge(args: &EdgeArgs) -> Result<EdgeSetup> {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build()?;
    let urls: Vec<String> = args.ollama_urls.iter().map(|u| ollama::normalize(u)).filter(|u| !u.is_empty()).collect();
    let mut reachable = Vec::new();
    let mut models = Vec::new();
    for url in &urls {
        match ollama::list_models(&http, url).await {
            Ok(m) => {
                if models.is_empty() {
                    models = m;
                }
                reachable.push(url.clone());
            }
            Err(e) => eprintln!("  ! {url}: {e:#}"),
        }
    }
    if reachable.is_empty() {
        bail!("no Ollama endpoint reachable ({}). Is `ollama serve` running?", urls.join(", "));
    }
    let model = ollama::pick_model(&models, args.model.as_deref())?;
    let caps = ollama::capabilities(&http, &reachable[0], &model).await.unwrap_or_default();
    Ok(EdgeSetup { endpoints: reachable, model, think: ollama::think_flag(&caps), format: args.edge_format })
}

async fn drive<F, Fut>(events: Arc<Vec<SecurityEvent>>, concurrency: usize, stats: SharedStats, task: F)
where
    F: Fn(SecurityEvent) -> Fut,
    Fut: Future<Output = TaskSample>,
{
    stats.lock().unwrap().start();
    futures::stream::iter(events.iter().cloned())
        .map(task)
        .buffer_unordered(concurrency.max(1))
        .for_each(|sample| {
            stats.lock().unwrap().record(sample);
            futures::future::ready(())
        })
        .await;
    stats.lock().unwrap().finish();
}

async fn run(args: RunArgs) -> Result<()> {
    println!("inversion-gap · resolving edge endpoints…");
    let edge_setup = resolve_edge(&args.edge).await?;
    let sovereign = edge_setup.endpoints.iter().filter(|u| ollama::is_sovereign(u)).count();
    let sovereign_pct = 100.0 * sovereign as f64 / edge_setup.endpoints.len() as f64;
    println!(
        "  model {} (format {:?}{}) on {}",
        edge_setup.model,
        edge_setup.format,
        if edge_setup.think.is_some() { ", think=false" } else { "" },
        edge_setup.endpoints.join(", ")
    );

    let events = Arc::new(events::generate(args.events, args.seed));
    let edge = Arc::new(EdgeActor::new(edge_setup.endpoints.clone(), edge_setup.model.clone(), edge_setup.think, edge_setup.format));

    if !args.no_warmup {
        println!("  warming up model on {} endpoint(s) (untimed)…", edge.endpoints.len());
        for (url, r) in edge.endpoints.iter().zip(edge.warm_up(&events).await) {
            match r {
                Ok(s) => println!("    {url}: ready in {s:.2}s"),
                Err(e) => println!("    {url}: warm-up failed: {e:#}"),
            }
        }
    }

    // ---- cloud side
    let profile = args.profile.resolve();
    let (cloud, cloud_desc) = match args.cloud {
        CloudTarget::Mock => {
            let (base, where_) = match &args.gateway_url {
                Some(url) => (ollama::normalize(url), "remote gateway".to_string()),
                None => {
                    let backend = match args.gateway_backend {
                        BackendKind::Synthetic => Backend::Synthetic,
                        BackendKind::Ollama => Backend::Ollama {
                            url: edge_setup.endpoints[0].clone(),
                            model: edge_setup.model.clone(),
                            think: edge_setup.think,
                        },
                    };
                    let desc = backend.describe();
                    let addr = gateway::spawn(SocketAddr::from(([127, 0, 0, 1], 0)), GatewayState::new(profile.clone(), backend)).await?;
                    (format!("http://{addr}"), format!("in-process mock gateway, {desc}"))
                }
            };
            let harness = CloudHarness::new(
                format!("{base}/v1/chat/completions"),
                "gpt-4o-copilot-sim".into(),
                CloudAuth::MockOAuth { token_url: format!("{base}/oauth2/v2.0/token") },
                true,
                args.cloud_max_tokens,
                None,
            );
            let desc = if args.gateway_url.is_some() {
                format!("{where_} {base} (profile is whatever that gateway was started with)")
            } else {
                format!(
                    "{where_} · profile {} (WAN {}, model {}, quota {}, +{} meta-prompt tok)",
                    profile.name, profile.wan_rtt, profile.inference, profile.max_inflight, profile.meta_prompt_tokens
                )
            };
            (harness, desc)
        }
        CloudTarget::Openai => {
            let endpoint = args.cloud_endpoint.clone().context("--cloud openai needs --cloud-endpoint (or INVERSION_CLOUD_ENDPOINT)")?;
            let auth = match args.cloud_auth {
                CloudAuthKind::ApiKey => CloudAuth::ApiKey {
                    header: "api-key".into(),
                    value: args.cloud_api_key.clone().context("--cloud-auth api-key needs --cloud-api-key / INVERSION_CLOUD_API_KEY")?,
                },
                CloudAuthKind::Bearer => CloudAuth::Bearer {
                    value: args.cloud_api_key.clone().context("--cloud-auth bearer needs --cloud-api-key / INVERSION_CLOUD_API_KEY")?,
                },
                CloudAuthKind::EntraCli => CloudAuth::EntraCli { resource: args.entra_resource.clone() },
            };
            let desc = format!(
                "real endpoint {} (model {}, auth {:?}{})",
                endpoint,
                args.cloud_model,
                args.cloud_auth,
                args.cloud_reasoning_effort.as_deref().map(|r| format!(", reasoning {r}")).unwrap_or_default()
            );
            (CloudHarness::new(endpoint, args.cloud_model.clone(), auth, false, args.cloud_max_tokens, args.cloud_reasoning_effort.clone()), desc)
        }
    };
    let cloud = Arc::new(cloud);

    let cloud_stats = HarnessStats::shared("cloud", events.len(), args.price_in, args.price_out);
    let edge_stats = HarnessStats::shared("edge", events.len(), 0.0, 0.0);

    // ---- race
    let done = Arc::new(AtomicBool::new(false));
    let bench = {
        let (events, cloud, edge) = (events.clone(), cloud.clone(), edge.clone());
        let (cs, es, done) = (cloud_stats.clone(), edge_stats.clone(), done.clone());
        let (cc, ec, schedule, edge_only) = (args.cloud_concurrency, args.edge_concurrency, args.schedule, args.edge_only);
        tokio::spawn(async move {
            let cloud_run = drive(events.clone(), cc, cs, move |e| {
                let h = cloud.clone();
                async move { h.triage(&e).await }
            });
            let edge_run = drive(events, ec, es, move |e| {
                let h = edge.clone();
                async move { h.triage(&e).await }
            });
            match schedule {
                _ if edge_only => {
                    drop(cloud_run);
                    edge_run.await;
                }
                Schedule::Parallel => {
                    tokio::join!(cloud_run, edge_run);
                }
                Schedule::Sequential => {
                    cloud_run.await;
                    edge_run.await;
                }
            }
            done.store(true, Ordering::Relaxed);
        })
    };

    let meta = tui::DashboardMeta {
        model: edge_setup.model.clone(),
        events: events.len(),
        edge_endpoints: edge_setup.endpoints.clone(),
        edge_concurrency: args.edge_concurrency,
        cloud_concurrency: args.cloud_concurrency,
        cloud_desc: cloud_desc.clone(),
        sovereign_pct,
    };

    // Off-screen dashboard renders at fixed milestones, for reports.
    let snapshots = match &args.snapshot_dir {
        Some(dir) => {
            std::fs::create_dir_all(dir)?;
            let (w, h) = args
                .snapshot_size
                .split_once('x')
                .and_then(|(w, h)| Some((w.parse::<u16>().ok()?, h.parse::<u16>().ok()?)))
                .context("--snapshot-size must look like 150x50")?;
            let (cs, es, d, m, dir) = (cloud_stats.clone(), edge_stats.clone(), done.clone(), meta.clone(), dir.clone());
            Some(tokio::spawn(async move {
                let mut taken = [false; 3];
                while !d.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let (c, e) = (cs.lock().unwrap().summary(), es.lock().unwrap().summary());
                    let due = [
                        e.calls() * 3 >= e.total && !e.done,      // both harnesses racing
                        e.done && c.calls() * 4 >= c.total,     // edge finished, cloud ~25%
                        c.calls() * 4 >= c.total * 3 && !c.done, // cloud ~75%
                    ];
                    let names = ["1-racing", "2-edge-finished", "3-cloud-75pct"];
                    for i in 0..3 {
                        if due[i] && !taken[i] {
                            taken[i] = true;
                            write_snapshot(&dir, names[i], &cs, &es, &m, false, w, h);
                        }
                    }
                }
                write_snapshot(&dir, "4-final", &cs, &es, &m, true, w, h);
            }))
        }
        None => None,
    };

    if args.headless {
        println!("  cloud: {cloud_desc}");
        let started = Instant::now();
        while !done.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(1000)).await;
            let (c, e) = (cloud_stats.lock().unwrap().summary(), edge_stats.lock().unwrap().summary());
            println!(
                "  [{:>5.1}s] cloud {:>3}/{} ({:.2} ops/s)   edge {:>3}/{} ({:.2} ops/s)",
                started.elapsed().as_secs_f64(),
                c.calls(),
                c.total,
                c.throughput_ops,
                e.calls(),
                e.total,
                e.throughput_ops
            );
        }
    } else {
        let (cs, es, d) = (cloud_stats.clone(), edge_stats.clone(), done.clone());
        tokio::task::spawn_blocking(move || tui::run(cs, es, meta, d)).await??;
        if !done.load(Ordering::Relaxed) {
            bench.abort();
            println!("aborted before completion — partial results:");
        }
    }

    if let Some(task) = snapshots {
        done.store(true, Ordering::Relaxed);
        task.await?;
        println!(" dashboard snapshots in {}", args.snapshot_dir.as_ref().unwrap().display());
    }

    let c = cloud_stats.lock().unwrap().summary();
    let e = edge_stats.lock().unwrap().summary();
    let g = gap(&c, &e);
    report::print_summary(&c, &e, &g, sovereign_pct);
    for (label, stats) in [("cloud", &cloud_stats), ("edge", &edge_stats)] {
        if let Some(err) = &stats.lock().unwrap().last_error {
            println!(" last {label} error: {err}");
        }
    }
    if let Some(path) = &args.json {
        let meta = json!({
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "events": events.len(),
            "seed": args.seed,
            "edge_model": edge_setup.model,
            "edge_endpoints": edge_setup.endpoints,
            "edge_concurrency": args.edge_concurrency,
            "edge_format": format!("{:?}", edge_setup.format),
            "cloud_concurrency": args.cloud_concurrency,
            "cloud": cloud_desc,
            "profile": (args.cloud == CloudTarget::Mock && args.gateway_url.is_none()).then_some(&profile),
            "price_in_per_m": args.price_in,
            "price_out_per_m": args.price_out,
            "sovereign_pct": sovereign_pct,
        });
        let (cl, el) = (cloud_stats.lock().unwrap().verdict_log(), edge_stats.lock().unwrap().verdict_log());
        report::write_json(path, meta, &c, &e, &g, report::Logs { cloud: &cl, edge: &el })?;
        println!(" wrote {}", path.display());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_snapshot(
    dir: &std::path::Path,
    name: &str,
    cloud: &SharedStats,
    edge: &SharedStats,
    meta: &tui::DashboardMeta,
    done: bool,
    w: u16,
    h: u16,
) {
    match tui::snapshot(cloud, edge, meta, done, w, h) {
        Ok((html, text)) => {
            let _ = std::fs::write(dir.join(format!("{name}.html")), html);
            let _ = std::fs::write(dir.join(format!("{name}.txt")), text);
        }
        Err(e) => eprintln!("snapshot {name} failed: {e:#}"),
    }
}

/// `inversion-gap check` — the "does `ollama run` work" gate, done over the API we benchmark.
async fn check(args: EdgeArgs) -> Result<()> {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(300)).build()?;
    println!("Ollama endpoints:");
    let setup = resolve_edge(&args).await?;
    for url in &setup.endpoints {
        let models = ollama::list_models(&http, url).await?;
        println!("  ✓ {url}  ({} models)", models.len());
    }
    let caps = ollama::capabilities(&http, &setup.endpoints[0], &setup.model).await.unwrap_or_default();
    println!("Model: {}  capabilities: [{}]  think flag: {:?}", setup.model, caps.join(", "), setup.think);

    let ev = events::generate(1, 7).remove(0);
    println!("Probe event: {}", triage::user_prompt(&ev));
    for url in &setup.endpoints {
        for pass in ["cold/warm-up", "warm"] {
            let t = Instant::now();
            let body = serde_json::to_vec(&ollama::triage_request(&setup.model, setup.think, setup.format, &ev))?;
            let bytes = ollama::post_chat(&http, url, body).await?;
            let wall = t.elapsed().as_secs_f64() * 1e3;
            let resp: OllamaChatResponse = serde_json::from_slice(&bytes)?;
            let verdict = triage::parse_verdict(&resp.message.content)?;
            let tps = if resp.eval_duration > 0 { resp.eval_count as f64 / (resp.eval_duration as f64 / 1e9) } else { 0.0 };
            println!(
                "  {url} [{pass:<12}] {:>7.0} ms wall · load {:.0} ms · {} tok @ {:.0} tok/s → {} / {} / {} ({:.2})",
                wall,
                resp.load_duration as f64 / 1e6,
                resp.eval_count,
                tps,
                verdict.severity.as_str(),
                verdict.action.as_str(),
                verdict.category,
                verdict.confidence
            );
        }
        if let Ok(ps) = ollama::running(&http, url).await {
            for (name, vram) in ps {
                println!("    resident: {name} ({:.1} GB VRAM)", vram as f64 / 1e9);
            }
        }
    }
    if let Ok(out) = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=index,name,memory.used,memory.total,utilization.gpu", "--format=csv,noheader"])
        .output()
    {
        println!("GPUs:");
        for l in String::from_utf8_lossy(&out.stdout).lines() {
            println!("  {l}");
        }
    }
    println!("OK — edge actor is ready.");
    Ok(())
}

async fn serve_gateway(args: GatewayArgs) -> Result<()> {
    let backend = match args.backend {
        BackendKind::Synthetic => Backend::Synthetic,
        BackendKind::Ollama => {
            let setup = resolve_edge(&args.edge).await?;
            Backend::Ollama { url: setup.endpoints[0].clone(), model: setup.model, think: setup.think }
        }
    };
    let profile = args.profile.resolve();
    println!("profile: {}", serde_json::to_string(&profile)?);
    println!("backend: {}", backend.describe());
    gateway::serve_forever(args.bind, GatewayState::new(profile, backend)).await
}

/// `inversion-gap agent` — the compound loop across architectural arms A–D.
async fn run_agent(args: AgentArgs) -> Result<()> {
    let arms: Vec<Arm> = {
        let mut a = args.arms.clone();
        a.sort_by_key(|x| x.short());
        a.dedup();
        a
    };
    let needs_edge = arms.iter().any(|a| matches!(a, Arm::C | Arm::D));
    let needs_direct = arms.iter().any(|a| matches!(a, Arm::B | Arm::D));
    let needs_gateway = arms.contains(&Arm::A) || (needs_direct && args.cloud == CloudTarget::Mock);
    let events = Arc::new(events::generate(args.events, args.seed));
    println!("inversion-gap agent · arms {}", arms.iter().map(|a| a.short()).collect::<Vec<_>>().join(", "));

    // ---- edge
    let (edge, edge_model) = if needs_edge {
        let setup = resolve_edge(&args.edge).await?;
        println!("  edge model {} on {}", setup.model, setup.endpoints.join(", "));
        let actor = EdgeActor::new(setup.endpoints.clone(), setup.model.clone(), setup.think, setup.format);
        if !args.no_warmup {
            for (url, r) in actor.endpoints.iter().zip(actor.warm_up(&events).await) {
                match r {
                    Ok(s) => println!("    {url}: ready in {s:.2}s"),
                    Err(e) => println!("    {url}: warm-up failed: {e:#}"),
                }
            }
        }
        (Some(actor), setup.model)
    } else {
        (None, "—".to_string())
    };

    // ---- cloud model target
    let real = match args.cloud {
        CloudTarget::Openai => {
            let endpoint = args.cloud_endpoint.clone().context("--cloud openai needs --cloud-endpoint")?;
            let key = args.cloud_api_key.clone().context("--cloud openai needs --cloud-api-key / INVERSION_CLOUD_API_KEY")?;
            Some((endpoint, key))
        }
        CloudTarget::Mock => None,
    };

    // ---- cloud gateway: arm A's runtime (and the mock model when no real endpoint is used)
    let mut profile = args.profile.resolve();
    if args.profile.quota.is_none() {
        // No artificial throttling by default: arm A pays the runtime hops, not an invented quota.
        profile.max_inflight = args.cloud_concurrency.max(1) * 4;
    }
    let gateway_base = if needs_gateway {
        let backend = match &real {
            Some((endpoint, key)) => Backend::Azure {
                endpoint: endpoint.clone(),
                api_key: key.clone(),
                model: args.cloud_model.clone(),
                max_tokens: args.cloud_max_tokens,
                reasoning_effort: args.cloud_reasoning_effort.clone(),
            },
            None => Backend::Synthetic,
        };
        let addr = gateway::spawn(SocketAddr::from(([127, 0, 0, 1], 0)), GatewayState::new(profile.clone(), backend)).await?;
        Some(format!("http://{addr}"))
    } else {
        None
    };
    let hosted = match (&gateway_base, arms.contains(&Arm::A)) {
        (Some(base), true) => Some(
            CloudHarness::new(
                format!("{base}/v1/chat/completions"),
                "cloud-hosted-agent".into(),
                CloudAuth::MockOAuth { token_url: format!("{base}/oauth2/v2.0/token") },
                true,
                args.cloud_max_tokens,
                None,
            )
            .with_tool_base(base.clone()),
        ),
        _ => None,
    };
    let direct = if needs_direct {
        Some(match (&real, &gateway_base) {
            (Some((endpoint, key)), _) => CloudHarness::new(
                endpoint.clone(),
                args.cloud_model.clone(),
                CloudAuth::ApiKey { header: "api-key".into(), value: key.clone() },
                false,
                args.cloud_max_tokens,
                args.cloud_reasoning_effort.clone(),
            ),
            (None, Some(base)) => CloudHarness::new(
                format!("{base}/v1/chat/completions"),
                "mock-model".into(),
                CloudAuth::MockOAuth { token_url: format!("{base}/oauth2/v2.0/token") },
                // The synthetic model reads the envelope; give B the same context as A in mock mode.
                true,
                args.cloud_max_tokens,
                None,
            ),
            (None, None) => unreachable!("mock cloud always starts a gateway"),
        })
    } else {
        None
    };
    let cloud_desc = match &real {
        Some((endpoint, _)) => format!("{} via {}", args.cloud_model, endpoint.split('/').nth(2).unwrap_or(endpoint)),
        None => "mock synthetic model".into(),
    };
    let arm_a_desc = format!(
        "simulated runtime hops ({}: WAN {}, JWT {}, policy {}, safety {} on model steps)",
        profile.name, profile.wan_rtt, profile.jwt_validate, profile.policy, profile.safety
    );
    println!("  cloud model: {cloud_desc}");
    if arms.contains(&Arm::A) {
        println!("  arm A: {arm_a_desc}; model steps forwarded to the cloud model");
    }

    let agent = Arc::new(Agent::new(
        hosted,
        direct,
        edge,
        Escalation { below_confidence: args.escalate_below, on_critical: !args.no_escalate_critical },
    ));
    let arm_stats: tui::ArmStats = arms
        .iter()
        .map(|a| (*a, HarnessStats::shared(a.short(), events.len(), args.price_in, args.price_out)))
        .collect();

    let meta = tui::AgentMeta {
        events: events.len(),
        edge_model: edge_model.clone(),
        cloud_desc: cloud_desc.clone(),
        arm_a_desc: arm_a_desc.clone(),
        cloud_concurrency: args.cloud_concurrency,
        edge_concurrency: args.edge_concurrency,
        hardware_usd: args.hardware_usd,
        events_per_day: args.events_per_day,
        edge_energy_usd_per_m: args.edge_energy_usd_per_m,
        escalate_below: args.escalate_below,
        escalate_critical: !args.no_escalate_critical,
        schedule: format!("{:?}", args.schedule).to_lowercase(),
    };

    // ---- run
    let done = Arc::new(AtomicBool::new(false));
    let bench = {
        let (events, agent, arm_stats, done) = (events.clone(), agent.clone(), arm_stats.clone(), done.clone());
        let (cc, ec, schedule) = (args.cloud_concurrency, args.edge_concurrency, args.schedule);
        tokio::spawn(async move {
            let runs = arm_stats.into_iter().map(|(arm, stats)| {
                let (events, agent) = (events.clone(), agent.clone());
                let conc = if matches!(arm, Arm::A | Arm::B) { cc } else { ec };
                async move {
                    drive(events, conc, stats, move |e| {
                        let agent = agent.clone();
                        async move { agent.run(arm, &e).await }
                    })
                    .await
                }
            });
            match schedule {
                Schedule::Parallel => {
                    futures::future::join_all(runs).await;
                }
                Schedule::Sequential => {
                    for r in runs {
                        r.await;
                    }
                }
            }
            done.store(true, Ordering::Relaxed);
        })
    };

    let snapshots = match &args.snapshot_dir {
        Some(dir) => {
            std::fs::create_dir_all(dir)?;
            let (w, h) = args
                .snapshot_size
                .split_once('x')
                .and_then(|(w, h)| Some((w.parse::<u16>().ok()?, h.parse::<u16>().ok()?)))
                .context("--snapshot-size must look like 150x56")?;
            let (arm_stats, meta, dir, d) = (arm_stats.clone(), meta.clone(), dir.clone(), done.clone());
            Some(tokio::spawn(async move {
                let mut taken = std::collections::HashSet::new();
                while !d.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    for (arm, stats) in &arm_stats {
                        let s = stats.lock().unwrap().summary();
                        let key = format!("arm-{}-half", arm.short());
                        if s.started && !s.done && s.calls() * 2 >= s.total && taken.insert(key.clone()) {
                            write_agent_snapshot(&dir, &key, &arm_stats, &meta, false, w, h);
                        }
                    }
                }
                write_agent_snapshot(&dir, "final", &arm_stats, &meta, true, w, h);
            }))
        }
        None => None,
    };

    if args.headless {
        let started = Instant::now();
        while !done.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(1000)).await;
            let line: Vec<String> = arm_stats
                .iter()
                .map(|(a, s)| {
                    let s = s.lock().unwrap().summary();
                    format!("{} {:>3}/{} ({:.2}/s)", a.short(), s.calls(), s.total, s.throughput_ops)
                })
                .collect();
            println!("  [{:>6.1}s] {}", started.elapsed().as_secs_f64(), line.join("   "));
        }
    } else {
        let (a, m, d) = (arm_stats.clone(), meta.clone(), done.clone());
        tokio::task::spawn_blocking(move || tui::run_agent(a, m, d)).await??;
        if !done.load(Ordering::Relaxed) {
            bench.abort();
            println!("aborted before completion — partial results:");
        }
    }
    if let Some(task) = snapshots {
        done.store(true, Ordering::Relaxed);
        task.await?;
    }

    let sums: Vec<(Arm, metrics::Summary)> = arm_stats.iter().map(|(a, s)| (*a, s.lock().unwrap().summary())).collect();
    let paybacks = tui::paybacks(&sums, &meta);
    report::print_agent_summary(&sums, &paybacks, &meta);
    for (arm, stats) in &arm_stats {
        if let Some(err) = &stats.lock().unwrap().last_error {
            println!(" last {} error: {err}", arm.short());
        }
    }
    if let Some(path) = &args.json {
        let arms_json: Vec<serde_json::Value> = arm_stats
            .iter()
            .map(|(a, s)| {
                let st = s.lock().unwrap();
                json!({ "arm": a.short(), "title": a.title(), "summary": st.summary(), "verdicts": st.verdict_log() })
            })
            .collect();
        let doc = json!({
            "meta": {
                "timestamp": chrono::Utc::now().to_rfc3339(),
                "events": events.len(),
                "seed": args.seed,
                "edge_model": edge_model,
                "cloud": cloud_desc,
                "arm_a": arm_a_desc,
                "cloud_concurrency": args.cloud_concurrency,
                "edge_concurrency": args.edge_concurrency,
                "schedule": meta.schedule,
                "price_in_per_m": args.price_in,
                "price_out_per_m": args.price_out,
                "escalate_below": args.escalate_below,
                "escalate_critical": !args.no_escalate_critical,
                "hardware_usd": args.hardware_usd,
                "events_per_day": args.events_per_day,
                "edge_energy_usd_per_m": args.edge_energy_usd_per_m,
                "profile": profile,
            },
            "arms": arms_json,
            "paybacks": paybacks.iter().map(|(l, d)| json!({ "comparison": l, "days": d })).collect::<Vec<_>>(),
        });
        std::fs::write(path, serde_json::to_vec_pretty(&doc)?)?;
        println!(" wrote {}", path.display());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_agent_snapshot(dir: &std::path::Path, name: &str, arms: &tui::ArmStats, meta: &tui::AgentMeta, done: bool, w: u16, h: u16) {
    match tui::snapshot_agent(arms, meta, done, w, h) {
        Ok((html, text)) => {
            let _ = std::fs::write(dir.join(format!("{name}.html")), html);
            let _ = std::fs::write(dir.join(format!("{name}.txt")), text);
        }
        Err(e) => eprintln!("snapshot {name} failed: {e:#}"),
    }
}
