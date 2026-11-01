//! Live ratatui dashboards, plus off-screen renders of them (HTML + text) for reports.
//!
//! `run`:   two harness panels (cloud, edge) racing side by side, plus the gap panel.
//! `agent`: a 2×2 grid of arm panels (A–D), plus the Compound Loop Tax panel (orchestration overhead
//!          per event, payback days, $ per 1M events, data egress, agreement).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Gauge, Paragraph, Sparkline};

use crate::metrics::{FeedItem, Gap, SharedStats, Summary, gap};
use crate::report::{fmt_bytes, fmt_ms, fmt_usd, headline};
use crate::triage::Severity;

const CLOUD: Color = Color::Rgb(255, 138, 76);
const EDGE: Color = Color::Rgb(80, 250, 160);
const DIM: Color = Color::Rgb(130, 130, 145);
const GOLD: Color = Color::Rgb(255, 214, 90);

#[derive(Clone)]
pub struct DashboardMeta {
    pub model: String,
    pub events: usize,
    pub edge_endpoints: Vec<String>,
    pub edge_concurrency: usize,
    pub cloud_concurrency: usize,
    pub cloud_desc: String,
    pub sovereign_pct: f64,
}

struct View {
    s: Summary,
    spark: Vec<u64>,
    feed: Vec<FeedItem>,
    last_error: Option<String>,
}

fn view(stats: &SharedStats, spark_len: usize) -> View {
    let st = stats.lock().unwrap();
    View {
        s: st.summary(),
        spark: st.recent_latencies(spark_len),
        feed: st.feed.iter().take(12).cloned().collect(),
        last_error: st.last_error.clone(),
    }
}

/// Blocking render loop — run it on a blocking thread. Returns when the user quits.
pub fn run(cloud: SharedStats, edge: SharedStats, meta: DashboardMeta, done: Arc<AtomicBool>) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = (|| -> Result<()> {
        loop {
            terminal.draw(|f| draw(f, &cloud, &edge, &meta, done.load(Ordering::Relaxed)))?;
            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind != KeyEventKind::Press {
                        continue;
                    }
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                        _ => {}
                    }
                }
            }
        }
    })();
    ratatui::restore();
    result
}

fn draw(f: &mut Frame, cloud: &SharedStats, edge: &SharedStats, meta: &DashboardMeta, done: bool) {
    // The gap panel is the point of the whole exercise: it keeps its rows, harness panels flex.
    let [header, body, gap_area, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Fill(1),
        Constraint::Length(10),
        Constraint::Length(1),
    ])
    .areas(f.area());
    let [left, right] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(body);

    let spark_len = (left.width as usize).saturating_sub(2);
    let c = view(cloud, spark_len);
    let e = view(edge, spark_len);
    let g = gap(&c.s, &e.s);

    draw_header(f, header, meta);
    draw_harness(f, left, "☁  Cloud/API-Wrapper Harness", CLOUD, &c, cloud_rows(&c.s, meta));
    draw_harness(f, right, "⚡ Sovereign Edge Actor (Lani-style)", EDGE, &e, edge_rows(&e.s, meta));
    draw_gap(f, gap_area, &c.s, &e.s, &g, meta);

    let status = if done { "complete — press q to exit" } else { "running — q to abort" };
    let footer_line = Line::from(vec![
        Span::styled(format!(" {status} "), Style::new().fg(Color::Black).bg(if done { EDGE } else { GOLD })),
        Span::styled(format!("  cloud: {}", meta.cloud_desc), Style::new().fg(DIM)),
    ]);
    f.render_widget(Paragraph::new(footer_line), footer);
}

fn draw_header(f: &mut Frame, area: Rect, meta: &DashboardMeta) {
    let title = Line::from(vec![
        Span::styled("THE ORCHESTRATION TAX ", Style::new().fg(GOLD).add_modifier(Modifier::BOLD)),
        Span::styled("· Complexity Inversion Gap benchmark", Style::new().add_modifier(Modifier::BOLD)),
    ]);
    let sub = Line::from(Span::styled(
        format!(
            "{} events · edge model {} on {} endpoint(s) ×{} in flight · cloud ×{} in flight",
            meta.events,
            meta.model,
            meta.edge_endpoints.len(),
            meta.edge_concurrency,
            meta.cloud_concurrency
        ),
        Style::new().fg(DIM),
    ));
    let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM));
    f.render_widget(Paragraph::new(vec![title, sub]).alignment(Alignment::Center).block(block), area);
}

fn row(label: &str, value: String, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {label:<30}"), Style::new().fg(DIM)),
        Span::styled(value, Style::new().fg(color).add_modifier(Modifier::BOLD)),
    ])
}

fn cloud_rows(s: &Summary, _meta: &DashboardMeta) -> Vec<Line<'static>> {
    let w = Color::White;
    vec![
        row("Throughput", format!("{:.2} ops/sec", s.throughput_ops), CLOUD),
        row("Latency avg / p50 / p95", format!("{} / {} / {}", fmt_ms(s.avg_ms), fmt_ms(s.p50_ms), fmt_ms(s.p95_ms)), w),
        row("Cumulative network RTT", fmt_ms(s.network_ms), w),
        row("Auth overhead (OAuth + JWT)", format!("{}  ({} token fetch)", fmt_ms(s.auth_ms), s.token_fetches), w),
        row("Gateway / service overhead", fmt_ms(s.gateway_ms), w),
        row("Throttle back-off (429s)", format!("{}  ({} retries)", fmt_ms(s.queue_ms), s.retries), w),
        row("Serialisation/deser overhead", format!("{:.0} µs", s.serde_us), w),
        row("Upstream inference", fmt_ms(s.inference_ms), w),
        row("Payload off-host ↑ / ↓", format!("{} / {}", fmt_bytes(s.bytes_tx), fmt_bytes(s.bytes_rx)), w),
        row("Tokens metered in / out", format!("{} / {}", s.tokens_in, s.tokens_out), w),
        row("Token Meter cost", fmt_usd(s.cost_usd), GOLD),
    ]
}

fn edge_rows(s: &Summary, meta: &DashboardMeta) -> Vec<Line<'static>> {
    let w = Color::White;
    vec![
        row("Throughput", format!("{:.2} ops/sec", s.throughput_ops), EDGE),
        row("Latency avg / p50 / p95", format!("{} / {} / {}", fmt_ms(s.avg_ms), fmt_ms(s.p50_ms), fmt_ms(s.p95_ms)), w),
        row("Local loopback/IPC latency", fmt_ms(s.network_ms), w),
        row("Auth overhead", "0 ms — no token, no gateway".into(), EDGE),
        row("Gateway / routing hops", "0".into(), EDGE),
        row("Local queue (GPU saturation)", format!("{}  ({} GPU actors)", fmt_ms(s.queue_ms), meta.edge_endpoints.len()), w),
        row("Serialisation/deser overhead", format!("{:.0} µs (in-memory)", s.serde_us), w),
        row("Local GPU inference", fmt_ms(s.inference_ms), w),
        row("Payload off-host", if meta.sovereign_pct >= 100.0 { "0 B".into() } else { fmt_bytes(s.bytes_tx) }, EDGE),
        row("Tokens processed in / out", format!("{} / {}", s.tokens_in, s.tokens_out), w),
        row("Local resource cost", "$0.00 (flat)".into(), EDGE),
    ]
}

fn draw_harness(f: &mut Frame, area: Rect, title: &str, color: Color, v: &View, rows: Vec<Line<'static>>) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color))
        .title(Span::styled(format!(" {title} "), Style::new().fg(color).add_modifier(Modifier::BOLD)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // On short terminals drop the sparkline first, then the feed; metrics always win.
    let metrics_h = rows.len() as u16 + 1;
    let spare = inner.height.saturating_sub(1 + metrics_h);
    let spark_h = if spare >= 7 { 3 } else { 0 };
    let [gauge, metrics, spark_label, spark, feed] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(metrics_h),
        Constraint::Length(spark_h.min(1)),
        Constraint::Length(spark_h),
        Constraint::Fill(1),
    ])
    .areas(inner);

    let s = &v.s;
    let ratio = if s.total == 0 { 0.0 } else { (s.calls() as f64 / s.total as f64).min(1.0) };
    let state = if s.done { "done" } else if s.started { "running" } else { "queued" };
    f.render_widget(
        Gauge::default()
            .gauge_style(Style::new().fg(color).bg(Color::Rgb(40, 40, 48)))
            .ratio(ratio)
            .label(format!("{}/{}  {}  {:.1}s{}", s.calls(), s.total, state, s.elapsed_s, if s.failed > 0 { format!("  {} failed", s.failed) } else { String::new() })),
        gauge,
    );
    f.render_widget(Paragraph::new(rows), metrics);
    f.render_widget(Paragraph::new(Span::styled(" per-task latency →", Style::new().fg(DIM))), spark_label);
    f.render_widget(Sparkline::default().data(&v.spark).style(Style::new().fg(color)), spark);

    let mut lines: Vec<Line> = v
        .feed
        .iter()
        .take(feed.height as usize)
        .map(|it| match (&it.verdict, &it.error) {
            (Some(vd), _) => {
                let sev = match vd.severity {
                    Severity::Critical => Color::Rgb(255, 85, 85),
                    Severity::High => Color::Rgb(255, 170, 70),
                    Severity::Medium => GOLD,
                    Severity::Low => Color::Rgb(120, 200, 255),
                };
                Line::from(vec![
                    Span::styled(format!(" #{:<4}", it.event_id), Style::new().fg(DIM)),
                    Span::styled(format!("{:<9}", vd.severity.as_str()), Style::new().fg(sev).add_modifier(Modifier::BOLD)),
                    Span::styled(format!("{:<9}", vd.action.as_str()), Style::new().fg(Color::White)),
                    Span::styled(format!("{:<24}", truncate(&vd.category, 23)), Style::new().fg(DIM)),
                    Span::styled(fmt_ms(it.latency_ms), Style::new().fg(color)),
                ])
            }
            (None, err) => Line::from(vec![
                Span::styled(format!(" #{:<4}", it.event_id), Style::new().fg(DIM)),
                Span::styled(format!("error: {}", truncate(err.as_deref().unwrap_or("?"), 60)), Style::new().fg(Color::Red)),
            ]),
        })
        .collect();
    if lines.is_empty() {
        if let Some(e) = &v.last_error {
            lines.push(Line::from(Span::styled(format!(" {}", truncate(e, 80)), Style::new().fg(Color::Red))));
        }
    }
    f.render_widget(Paragraph::new(lines), feed);
}

fn draw_gap(f: &mut Frame, area: Rect, c: &Summary, e: &Summary, g: &Gap, meta: &DashboardMeta) {
    let block = Block::bordered()
        .border_type(BorderType::Double)
        .border_style(Style::new().fg(GOLD))
        .title(Span::styled(" ◆ The Complexity Inversion Gap ", Style::new().fg(GOLD).add_modifier(Modifier::BOLD)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let bar_w = (inner.width as usize).saturating_sub(34).max(10);
    let max = c.throughput_ops.max(e.throughput_ops).max(1e-9);
    let bar = |v: f64| "█".repeat(((v / max) * bar_w as f64).round() as usize);

    let (head, head_color) = match headline(g, meta.sovereign_pct) {
        Some((text, good)) => (text, if good { EDGE } else { CLOUD }),
        None => ("measuring…".to_string(), DIM),
    };
    let lat = match g.latency_x {
        Some(x) => format!("cloud avg {} vs edge avg {}  →  {:.1}x", fmt_ms(c.avg_ms), fmt_ms(e.avg_ms), x),
        None => "—".into(),
    };
    let per_call = |v: f64| fmt_ms(c.per_call(v));
    let lines = vec![
        Line::from(Span::styled(head, Style::new().fg(head_color).add_modifier(Modifier::BOLD))).alignment(Alignment::Center),
        Line::from(""),
        Line::from(vec![
            Span::styled(format!(" {:<12}", "Cloud ops/s"), Style::new().fg(DIM)),
            Span::styled(format!("{:>8.2} ", c.throughput_ops), Style::new().fg(CLOUD)),
            Span::styled(bar(c.throughput_ops), Style::new().fg(CLOUD)),
        ]),
        Line::from(vec![
            Span::styled(format!(" {:<12}", "Edge ops/s"), Style::new().fg(DIM)),
            Span::styled(format!("{:>8.2} ", e.throughput_ops), Style::new().fg(EDGE)),
            Span::styled(bar(e.throughput_ops), Style::new().fg(EDGE)),
        ]),
        Line::from(vec![Span::styled(" Latency multiple   ", Style::new().fg(DIM)), Span::raw(lat)]),
        Line::from(vec![
            Span::styled(" Orchestration tax  ", Style::new().fg(DIM)),
            Span::styled(format!("{:.0}% of every cloud call is not inference ", g.tax_pct), Style::new().fg(GOLD)),
            Span::styled(
                format!(
                    "(net {} · auth {} · policy+safety {} · back-off {} per call)",
                    per_call(c.network_ms),
                    per_call(c.auth_ms),
                    per_call(c.gateway_ms),
                    per_call(c.queue_ms)
                ),
                Style::new().fg(DIM),
            ),
        ]),
        Line::from(vec![
            Span::styled(" Token meter        ", Style::new().fg(DIM)),
            Span::styled(
                format!("{} this run → {} per 1M events", fmt_usd(g.cloud_cost_usd), fmt_usd(g.cloud_cost_per_million)),
                Style::new().fg(GOLD),
            ),
            Span::styled("   edge: $0.00", Style::new().fg(EDGE)),
        ]),
        Line::from(vec![
            Span::styled(" Data egress        ", Style::new().fg(DIM)),
            Span::raw(format!("{} left the trust boundary (cloud)", fmt_bytes(g.cloud_egress_bytes))),
            Span::styled(format!("   edge: {:.0}% sovereign", meta.sovereign_pct), Style::new().fg(EDGE)),
        ]),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

/// Render the exact dashboard frame off-screen and return (html, plain text). Used by
/// `--snapshot-dir` so reports can embed the real UI rather than a terminal capture.
pub fn snapshot(cloud: &SharedStats, edge: &SharedStats, meta: &DashboardMeta, done: bool, width: u16, height: u16) -> Result<(String, String)> {
    render_offscreen(width, height, |f| draw(f, cloud, edge, meta, done))
}

/// Draw one frame into an off-screen buffer and export it as (html, plain text).
fn render_offscreen(width: u16, height: u16, paint: impl FnOnce(&mut Frame)) -> Result<(String, String)> {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut terminal = Terminal::new(TestBackend::new(width, height))?;
    terminal.draw(paint)?;
    let buf = terminal.backend().buffer();

    let css = |c: Color| -> Option<String> {
        Some(match c {
            Color::Reset => return None,
            Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
            Color::Black => "#000000".into(),
            Color::Red => "#e05561".into(),
            Color::Green => "#5bd16e".into(),
            Color::Yellow => "#e5c07b".into(),
            Color::Blue => "#61afef".into(),
            Color::Magenta => "#c678dd".into(),
            Color::Cyan => "#56b6c2".into(),
            Color::Gray => "#abb2bf".into(),
            Color::DarkGray => "#5c6370".into(),
            Color::LightRed => "#ff7b86".into(),
            Color::LightGreen => "#8ff0a4".into(),
            Color::LightYellow => "#ffe08a".into(),
            Color::LightBlue => "#8cc8ff".into(),
            Color::LightMagenta => "#e0a0ff".into(),
            Color::LightCyan => "#8be9fd".into(),
            Color::White => "#ffffff".into(),
            Color::Indexed(i) => format!("var(--ansi-{i}, #cccccc)"),
        })
    };
    let escape = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");

    let mut html = String::from(
        "<!doctype html><html><head><meta charset=\"utf-8\"><style>\
         body{margin:0;background:#0d1117}\
         pre{margin:0;padding:14px;font:13px/1.25 'Cascadia Mono',Consolas,monospace;color:#d8dee9;background:#0d1117}\
         .c1{display:inline-block;width:1ch;overflow:visible}\
         </style></head><body><pre>",
    );
    let mut text = String::new();
    for y in 0..buf.area.height {
        let mut run_style: Option<(Option<String>, Option<String>, bool)> = None;
        let mut run = String::new();
        let flush = |html: &mut String, style: &Option<(Option<String>, Option<String>, bool)>, run: &mut String| {
            if run.is_empty() {
                return;
            }
            let (fg, bg, bold) = style.clone().unwrap_or_default();
            let mut st = String::new();
            if let Some(fg) = fg {
                st.push_str(&format!("color:{fg};"));
            }
            if let Some(bg) = bg {
                st.push_str(&format!("background:{bg};"));
            }
            if bold {
                st.push_str("font-weight:700;");
            }
            if st.is_empty() {
                html.push_str(&escape(run));
            } else {
                html.push_str(&format!("<span style=\"{st}\">{}</span>", escape(run)));
            }
            run.clear();
        };
        for x in 0..buf.area.width {
            let cell = &buf[(x, y)];
            let sym = cell.symbol();
            if sym.is_empty() {
                continue; // trailing half of a wide glyph
            }
            text.push_str(sym);
            let style = (css(cell.fg), css(cell.bg), cell.modifier.contains(Modifier::BOLD));
            // Emoji/symbols fall back to proportional fonts in browsers; pin each to one cell
            // (ratatui already pads wide glyphs with a trailing space cell) so borders stay aligned.
            let box_drawing = sym.chars().next().is_some_and(|ch| ('\u{2500}'..='\u{259F}').contains(&ch));
            if !sym.is_ascii() && !box_drawing {
                flush(&mut html, &run_style, &mut run);
                run_style = Some(style.clone());
                run.push_str(sym);
                let mut tmp = String::new();
                flush(&mut tmp, &run_style, &mut run);
                if tmp.starts_with("<span") {
                    html.push_str(&tmp.replacen("<span ", "<span class=\"c1\" ", 1));
                } else {
                    html.push_str(&format!("<span class=\"c1\">{}</span>", escape(sym)));
                }
                run_style = None;
                continue;
            }
            if run_style.as_ref() != Some(&style) {
                flush(&mut html, &run_style, &mut run);
                run_style = Some(style);
            }
            run.push_str(sym);
        }
        flush(&mut html, &run_style, &mut run);
        html.push('\n');
        text.push('\n');
    }
    html.push_str("</pre></body></html>\n");
    Ok((html, text))
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n.saturating_sub(1)).chain(['…']).collect() }
}

// =========================================================================================
// Agent (compound loop) dashboard: four arms in a 2×2 grid + the Compound Loop Tax panel.
// =========================================================================================

use crate::agent::Arm;

const ARM_B: Color = Color::Rgb(110, 165, 255);
const ARM_D: Color = Color::Rgb(205, 145, 255);

fn arm_color(arm: Arm) -> Color {
    match arm {
        Arm::A => CLOUD,
        Arm::B => ARM_B,
        Arm::C => EDGE,
        Arm::D => ARM_D,
    }
}

#[derive(Clone)]
pub struct AgentMeta {
    pub events: usize,
    pub edge_model: String,
    pub cloud_desc: String,
    pub arm_a_desc: String,
    pub cloud_concurrency: usize,
    pub edge_concurrency: usize,
    pub hardware_usd: f64,
    pub events_per_day: f64,
    pub edge_energy_usd_per_m: f64,
    pub escalate_below: f32,
    pub escalate_critical: bool,
    pub schedule: String,
}

pub type ArmStats = Vec<(Arm, SharedStats)>;

/// Payback lines shared by the dashboard and the text report.
pub fn paybacks(sums: &[(Arm, Summary)], meta: &AgentMeta) -> Vec<(String, Option<f64>)> {
    let cost = |arm: Arm| sums.iter().find(|(a, _)| *a == arm).and_then(|(_, s)| s.cost_per_event());
    let energy = meta.edge_energy_usd_per_m / 1e6;
    let pb = |saving: Option<f64>| saving.and_then(|v| crate::metrics::payback_days(meta.hardware_usd, meta.events_per_day, v));
    let mut out = Vec::new();
    if cost(Arm::A).is_some() {
        out.push(("own GPUs (C) instead of A".to_string(), pb(cost(Arm::A).map(|a| a - energy))));
    }
    if cost(Arm::B).is_some() {
        out.push(("own GPUs (C) instead of B".to_string(), pb(cost(Arm::B).map(|b| b - energy))));
    }
    if let (Some(b), Some(d)) = (cost(Arm::B), cost(Arm::D)) {
        out.push(("hybrid D instead of B".to_string(), pb(Some(b - d - energy))));
    }
    out
}

pub fn run_agent(arms: ArmStats, meta: AgentMeta, done: Arc<AtomicBool>) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = (|| -> Result<()> {
        loop {
            terminal.draw(|f| draw_agent(f, &arms, &meta, done.load(Ordering::Relaxed)))?;
            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind != KeyEventKind::Press {
                        continue;
                    }
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                        _ => {}
                    }
                }
            }
        }
    })();
    ratatui::restore();
    result
}

pub fn snapshot_agent(arms: &ArmStats, meta: &AgentMeta, done: bool, width: u16, height: u16) -> Result<(String, String)> {
    render_offscreen(width, height, |f| draw_agent(f, arms, meta, done))
}

fn draw_agent(f: &mut Frame, arms: &ArmStats, meta: &AgentMeta, done: bool) {
    let [header, grid, econ, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Fill(1),
        Constraint::Length(12),
        Constraint::Length(1),
    ])
    .areas(f.area());
    let [top, bottom] = Layout::vertical([Constraint::Fill(1), Constraint::Fill(1)]).areas(grid);
    let [tl, tr] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(top);
    let [bl, br] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(bottom);

    let sums: Vec<(Arm, Summary)> = arms.iter().map(|(a, s)| (*a, s.lock().unwrap().summary())).collect();

    let title = Line::from(vec![
        Span::styled("THE COMPOUND LOOP TAX ", Style::new().fg(GOLD).add_modifier(Modifier::BOLD)),
        Span::styled("· 4-step agent per event: model → tool → model → tool", Style::new().add_modifier(Modifier::BOLD)),
    ]);
    let sub = Line::from(Span::styled(
        format!(
            "{} events · edge model {} · cloud ×{} / edge ×{} events in flight · arms run {}",
            meta.events, meta.edge_model, meta.cloud_concurrency, meta.edge_concurrency, meta.schedule
        ),
        Style::new().fg(DIM),
    ));
    let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM));
    f.render_widget(Paragraph::new(vec![title, sub]).alignment(Alignment::Center).block(block), header);

    for (area, arm) in [(tl, Arm::A), (tr, Arm::B), (bl, Arm::C), (br, Arm::D)] {
        match sums.iter().find(|(a, _)| *a == arm) {
            Some((_, s)) => draw_arm(f, area, arm, s),
            None => {
                let b = Block::bordered().border_style(Style::new().fg(DIM)).title(format!(" {} — not selected ", arm.title()));
                f.render_widget(b, area);
            }
        }
    }
    draw_econ(f, econ, &sums, meta);

    let status = if done { "complete — press q to exit" } else { "running — q to abort" };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!(" {status} "), Style::new().fg(Color::Black).bg(if done { EDGE } else { GOLD })),
            Span::styled(format!("  A: {} · B/D cloud: {}", meta.arm_a_desc, meta.cloud_desc), Style::new().fg(DIM)),
        ])),
        footer,
    );
}

fn draw_arm(f: &mut Frame, area: Rect, arm: Arm, s: &Summary) {
    let color = arm_color(arm);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color))
        .title(Span::styled(format!(" {} ", arm.title()), Style::new().fg(color).add_modifier(Modifier::BOLD)));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [gauge, body] = Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(inner);

    let ratio = if s.total == 0 { 0.0 } else { (s.calls() as f64 / s.total as f64).min(1.0) };
    let state = if s.done { "done" } else if s.started { "running" } else { "queued" };
    f.render_widget(
        Gauge::default()
            .gauge_style(Style::new().fg(color).bg(Color::Rgb(40, 40, 48)))
            .ratio(ratio)
            .label(format!("{}/{}  {}  {:.1}s{}", s.calls(), s.total, state, s.elapsed_s, if s.failed > 0 { format!("  {} failed", s.failed) } else { String::new() })),
        gauge,
    );

    let n = s.calls().max(1) as f64;
    let per = |v: f64| fmt_ms(v / n);
    let w = Color::White;
    let share = if s.avg_ms > 0.0 { 100.0 * (s.orch_ms / n) / s.avg_ms } else { 0.0 };
    let g = s.rubric_graded.max(1) as f64;
    // Wider label column than the single-shot dashboard: these labels run to 32 characters.
    let row = |label: &str, value: String, c: Color| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!(" {label:<33}"), Style::new().fg(DIM)),
            Span::styled(value, Style::new().fg(c).add_modifier(Modifier::BOLD)),
        ])
    };
    let mut rows = vec![
        row("Throughput", format!("{:.2} events/s", s.throughput_ops), color),
        row("Event latency avg / p95", format!("{} / {}", fmt_ms(s.avg_ms), fmt_ms(s.p95_ms)), w),
        row("Orchestration overhead / event", format!("{}  ({share:.0}% of latency)", per(s.orch_ms)), GOLD),
        row("  net · service · auth · 429", format!("{} · {} · {} · {}", per(s.network_ms), per(s.gateway_ms), per(s.auth_ms), per(s.queue_ms)), w),
        row("Model compute / event", per(s.inference_ms), w),
        row("Tool execution / event", if s.tool_ms / n < 1.0 { format!("{:.0} µs", 1000.0 * s.tool_ms / n) } else { per(s.tool_ms) }, w),
        row("Local GPU contention / event", per(s.contention_ms), w),
        row(
            "Calls / event: model·tool·cross",
            format!("{:.1} · {:.1} · {:.1}", s.model_calls as f64 / n, s.tool_calls as f64 / n, s.boundary_calls as f64 / n),
            w,
        ),
        row(
            "Metered cost",
            match s.cost_per_event() {
                Some(c) if s.cost_usd > 0.0 => format!("{}  ({}/1M events)", fmt_usd(s.cost_usd), fmt_usd(c * 1e6)),
                _ => "$0.00".into(),
            },
            if s.cost_usd > 0.0 { GOLD } else { EDGE },
        ),
        row(
            "Data off-host",
            format!("{} · {:.0}% of events", fmt_bytes(s.bytes_off_host), s.egress_pct()),
            if s.bytes_off_host == 0 { EDGE } else { w },
        ),
        row(
            "Agreement sev / action",
            if s.rubric_graded == 0 { "—".into() } else { format!("{:.0}% / {:.0}%", 100.0 * s.rubric_severity_exact as f64 / g, 100.0 * s.rubric_action_exact as f64 / g) },
            w,
        ),
    ];
    if arm == Arm::D {
        rows.push(row("Escalated to cloud", format!("{} ({:.0}%)", s.escalations, 100.0 * s.escalations as f64 / n), ARM_D));
    }
    f.render_widget(Paragraph::new(rows), body);
}

fn draw_econ(f: &mut Frame, area: Rect, sums: &[(Arm, Summary)], meta: &AgentMeta) {
    let block = Block::bordered()
        .border_type(BorderType::Double)
        .border_style(Style::new().fg(GOLD))
        .title(Span::styled(" ◆ The Compound Loop Tax ", Style::new().fg(GOLD).add_modifier(Modifier::BOLD)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let overhead = |s: &Summary| s.orch_ms / s.calls().max(1) as f64;
    let max = sums.iter().map(|(_, s)| overhead(s)).fold(1.0_f64, f64::max);
    let bar_w = (inner.width as usize).saturating_sub(48).max(10);
    let mut lines = vec![Line::from(Span::styled(" Orchestration overhead per event (time that is neither model compute, tool work nor local GPU contention)", Style::new().fg(DIM)))];
    for (arm, s) in sums {
        let o = overhead(s);
        lines.push(Line::from(vec![
            Span::styled(format!(" {:<3}", arm.short()), Style::new().fg(arm_color(*arm)).add_modifier(Modifier::BOLD)),
            Span::styled(format!("{:>9} ", fmt_ms(o)), Style::new().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled("█".repeat(((o / max) * bar_w as f64).round().max(if o > 0.0 { 1.0 } else { 0.0 }) as usize), Style::new().fg(arm_color(*arm))),
            Span::styled(format!("  {} crossings/event", fmt_num(s.boundary_calls as f64 / s.calls().max(1) as f64)), Style::new().fg(DIM)),
        ]));
    }
    let pb = paybacks(sums, meta);
    let pb_txt: Vec<String> = pb
        .iter()
        .map(|(l, d)| {
            let l = l.replace("own GPUs (C) instead of ", "C vs ").replace("hybrid D instead of ", "D vs ");
            match d {
                Some(d) if *d < 3650.0 => format!("{l} {d:.0} days"),
                Some(_) => format!("{l} > 10 years"),
                None => format!("{l} never (no saving)"),
            }
        })
        .collect();
    lines.push(Line::from(vec![
        Span::styled(format!(" Payback of ${:.0} GPUs at {:.0}k events/day ", meta.hardware_usd, meta.events_per_day / 1000.0), Style::new().fg(DIM)),
        Span::styled(if pb_txt.is_empty() { "—".into() } else { pb_txt.join("  ·  ") }, Style::new().fg(GOLD).add_modifier(Modifier::BOLD)),
    ]));
    let per_m = |s: &Summary| match s.cost_per_event() {
        Some(c) if c > 0.0 => fmt_usd(c * 1e6),
        Some(_) => "$0".into(),
        None => "—".into(),
    };
    lines.push(Line::from(vec![
        Span::styled(" Metered $ per 1M events   ", Style::new().fg(DIM)),
        Span::raw(sums.iter().map(|(a, s)| format!("{} {}", a.short(), per_m(s))).collect::<Vec<_>>().join("  ·  ")),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" Data egress (events off-host) ", Style::new().fg(DIM)),
        Span::raw(sums.iter().map(|(a, s)| format!("{} {:.0}% ({})", a.short(), s.egress_pct(), fmt_bytes(s.bytes_off_host))).collect::<Vec<_>>().join("  ·  ")),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" Agreement, action exact   ", Style::new().fg(DIM)),
        Span::raw(
            sums.iter()
                .map(|(a, s)| format!("{} {:.0}%", a.short(), 100.0 * s.rubric_action_exact as f64 / s.rubric_graded.max(1) as f64))
                .collect::<Vec<_>>()
                .join("  ·  "),
        ),
        Span::styled(
            format!("   (D escalates if confidence < {:.2}{})", meta.escalate_below, if meta.escalate_critical { " or severity = critical" } else { "" }),
            Style::new().fg(DIM),
        ),
    ]));
    f.render_widget(Paragraph::new(lines), inner);
}

fn fmt_num(v: f64) -> String {
    if (v - v.round()).abs() < 0.05 { format!("{:.0}", v) } else { format!("{v:.1}") }
}
