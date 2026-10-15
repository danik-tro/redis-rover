#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
use std::sync::Arc;

use parking_lot::Mutex;
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Style, Stylize},
    symbols::Marker,
    text::{Line, Span},
    widgets::{
        Axis, Block, BorderType, Borders, Chart, Dataset, Gauge, GraphType, Paragraph, Sparkline,
        StatefulWidget, Widget,
    },
};

use crate::{
    config,
    redis_client::metrics::{MetricsHistory, Series},
    utils::{fmt_bytes, fmt_compact, fmt_pct, fmt_rate_bytes, format_duration},
};

/// `(label, value, sparkline data, sparkline floor, colour)`.
type CompactRow = (&'static str, Option<String>, Vec<u64>, u64, Color);

/// A panel renderer.
type PanelFn = fn(&MetricsHistory, Rect, &mut Buffer);

/// Minimum inner size for the full chart grid.
const FULL_MIN: (u16, u16) = (80, 24);
/// Minimum inner height for the one-row-per-metric layout.
const COMPACT_MIN_HEIGHT: u16 = 9;

/// State of the dashboard view: a handle on the shared metrics history and an
/// optional frozen copy while paused.
pub struct Dashboard {
    metrics: Arc<Mutex<MetricsHistory>>,
    frozen: Option<MetricsHistory>,
    selected: Metric,
    all_charts: bool,
}

/// One chartable metric; the dashboard shows the selected one full-size.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Metric {
    Ops,
    Memory,
    Network,
    Cpu,
    HitRate,
    Clients,
    Keys,
}

impl Metric {
    const ALL: [Metric; 7] = [
        Metric::Ops,
        Metric::Memory,
        Metric::Network,
        Metric::Cpu,
        Metric::HitRate,
        Metric::Clients,
        Metric::Keys,
    ];

    fn name(self) -> &'static str {
        match self {
            Metric::Ops => "Ops/sec",
            Metric::Memory => "Memory",
            Metric::Network => "Network",
            Metric::Cpu => "CPU",
            Metric::HitRate => "Hit rate",
            Metric::Clients => "Clients",
            Metric::Keys => "Keys",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|m| *m == self).unwrap_or(0)
    }

    fn step(self, delta: isize) -> Self {
        let n = Self::ALL.len() as isize;
        Self::ALL[(self.index() as isize + delta).rem_euclid(n) as usize]
    }
}

impl Dashboard {
    pub fn new(metrics: Arc<Mutex<MetricsHistory>>) -> Self {
        Self {
            metrics,
            frozen: None,
            selected: Metric::Ops,
            all_charts: false,
        }
    }

    /// Select the next metric (and leave the all-charts overview).
    pub fn next_chart(&mut self) {
        self.selected = self.selected.step(1);
        self.all_charts = false;
    }

    /// Select the previous metric (and leave the all-charts overview).
    pub fn prev_chart(&mut self) {
        self.selected = self.selected.step(-1);
        self.all_charts = false;
    }

    /// Switch between one big chart and every chart on the page.
    pub fn toggle_all(&mut self) {
        self.all_charts = !self.all_charts;
    }

    /// Freeze the view on the current history, or resume live data.
    pub fn toggle_pause(&mut self) {
        self.frozen = match self.frozen {
            Some(_) => None,
            None => Some(self.metrics.lock().clone()),
        };
    }

    /// Resume live data (no-op when not paused).
    pub fn unpause(&mut self) {
        self.frozen = None;
    }

    pub fn is_paused(&self) -> bool {
        self.frozen.is_some()
    }
}

pub struct DashboardWidget;

/// `(i, v)` points of the last `n` samples.
fn chart_points(s: &Series, n: usize) -> Vec<(f64, f64)> {
    s.tail(n).enumerate().map(|(i, v)| (i as f64, v)).collect()
}

/// Last `n` samples as sparkline integers, multiplied by `scale`.
fn spark_data(s: &Series, n: usize, scale: f64) -> Vec<u64> {
    s.tail(n)
        .map(|v| (v * scale).round().max(0.0) as u64)
        .collect()
}

/// Largest visible value across `series`, never below a tiny positive floor so
/// the chart range is never empty.
fn visible_max(series: &[&Series], n: usize) -> f64 {
    let m = series
        .iter()
        .flat_map(|s| s.tail(n))
        .fold(0.0_f64, f64::max);
    if m > 0.0 {
        m
    } else {
        1.0
    }
}

fn hit_rate_color(ratio: f64) -> Color {
    let c = &config::get().colors;
    if ratio < 0.5 {
        c.base08
    } else if ratio < 0.8 {
        c.base0b
    } else {
        c.base0a
    }
}

fn memory_color(used: f64, limit: f64) -> Color {
    let c = &config::get().colors;
    let r = if limit > 0.0 { used / limit } else { 0.0 };
    if r > 0.9 {
        c.base08
    } else if r > 0.75 {
        c.base0b
    } else {
        c.base0a
    }
}

fn empty_text(h: &MetricsHistory) -> &'static str {
    if h.samples_recorded == 0 && h.latest_snapshot.is_none() {
        "collecting…"
    } else {
        "n/a"
    }
}

fn empty_state(h: &MetricsHistory, area: Rect, buf: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    let row = Rect {
        y: area.y + area.height / 2,
        height: 1,
        ..area
    };
    Paragraph::new(empty_text(h))
        .alignment(Alignment::Center)
        .render(row, buf);
}

/// Draw the panel frame and return its inner area.
fn panel(title: String, area: Rect, buf: &mut Buffer) -> Rect {
    let cfg = config::get();
    let block = Block::default()
        .title(format!(" {title} "))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(cfg.colors.base04))
        .fg(cfg.colors.base05);
    let inner = block.inner(area);
    block.render(area, buf);
    inner
}

/// Slow-moving gauges (memory, clients): rescale the visible window to
/// `1..=8` so small changes stay visible and a flat line is a thin baseline
/// instead of a full-height block.
fn spark_ranged(s: &Series, n: usize) -> Vec<u64> {
    let v: Vec<f64> = s.tail(n).collect();
    let min = v.iter().copied().fold(f64::INFINITY, f64::min);
    let max = v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let span = max - min;
    v.iter()
        .map(|x| {
            if span > 0.0 {
                1 + ((x - min) / span * 7.0).round() as u64
            } else {
                1
            }
        })
        .collect()
}

/// Draw a sparkline whose full height is at least `floor`, so an idle server
/// shows low bars rather than auto-scaling noise to the full height.
fn spark(data: &[u64], floor: u64, color: Color, area: Rect, buf: &mut Buffer) {
    let max = data.iter().copied().max().unwrap_or(0).max(floor).max(1);
    Sparkline::default()
        .max(max)
        .data(data)
        .style(Style::default().fg(color))
        .render(area, buf);
}

fn line_chart(
    datasets: Vec<Dataset<'_>>,
    n_points: usize,
    y_max: f64,
    y_label: String,
    area: Rect,
    buf: &mut Buffer,
) {
    Chart::new(datasets)
        .x_axis(Axis::default().bounds([0.0, n_points.max(1) as f64]))
        .y_axis(
            Axis::default()
                .bounds([0.0, y_max * 1.1])
                .labels(vec![Span::raw("0"), Span::raw(y_label)]),
        )
        .render(area, buf);
}

fn render_ops(h: &MetricsHistory, area: Rect, buf: &mut Buffer) {
    let n = usize::from(area.width.saturating_sub(2)) * 2;
    let peak = h.ops.tail(n).fold(0.0_f64, f64::max);
    let last = h.ops.last().map_or("n/a".into(), fmt_compact);
    let inner = panel(
        format!("Ops/sec  {last}  (peak {})", fmt_compact(peak)),
        area,
        buf,
    );
    if h.ops.is_empty() {
        return empty_state(h, inner, buf);
    }
    let pts = chart_points(&h.ops, n);
    let color = config::get().colors.base0d;
    let ds = Dataset::default()
        .marker(Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(color))
        .data(&pts);
    let y = visible_max(&[&h.ops], n);
    line_chart(vec![ds], pts.len(), y, fmt_compact(y), inner, buf);
}

fn render_memory(h: &MetricsHistory, area: Rect, buf: &mut Buffer) {
    let snap = h.latest_snapshot.as_ref();
    let used = h.used_memory.last();
    let limit = snap
        .and_then(|s| s.maxmemory.filter(|m| *m > 0).or(s.total_system_memory))
        .filter(|l| *l > 0);
    let title = match (used, limit) {
        (Some(u), Some(l)) => format!("Memory  {} / {}", fmt_bytes(u as u64), fmt_bytes(l)),
        (Some(u), None) => format!("Memory  {}", fmt_bytes(u as u64)),
        _ => "Memory".into(),
    };
    let inner = panel(title, area, buf);
    let Some(used) = used else {
        return empty_state(h, inner, buf);
    };
    let has_gauge = limit.is_some();
    let [g, s, t] = Layout::vertical([
        Constraint::Length(u16::from(has_gauge)),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    let color = memory_color(used, limit.map_or(0.0, |l| l as f64));
    if let Some(l) = limit {
        Gauge::default()
            .ratio((used / l as f64).clamp(0.0, 1.0))
            .gauge_style(Style::default().fg(color))
            .label(fmt_pct(used / l as f64))
            .render(g, buf);
    }
    spark(
        &spark_ranged(&h.used_memory, usize::from(s.width)),
        8,
        color,
        s,
        buf,
    );
    let mut parts = Vec::new();
    if let Some(rss) = snap.and_then(|s| s.used_memory_rss) {
        parts.push(format!("rss {}", fmt_bytes(rss)));
    }
    if let Some(f) = snap.and_then(|s| s.mem_fragmentation_ratio) {
        parts.push(format!("frag {f:.2}"));
    }
    if let Some(p) = snap.and_then(|s| s.used_memory_peak) {
        parts.push(format!("peak {}", fmt_bytes(p)));
    }
    Paragraph::new(parts.join(" · ")).render(t, buf);
}

fn render_network(h: &MetricsHistory, area: Rect, buf: &mut Buffer) {
    let n = usize::from(area.width.saturating_sub(2)) * 2;
    let rate = |v: Option<f64>| v.map_or("n/a".into(), fmt_rate_bytes);
    let inner = panel(
        format!(
            "Network  in {}  out {}",
            rate(h.net_in.last()),
            rate(h.net_out.last())
        ),
        area,
        buf,
    );
    if h.net_in.is_empty() && h.net_out.is_empty() {
        return empty_state(h, inner, buf);
    }
    let c = &config::get().colors;
    let (pin, pout) = (chart_points(&h.net_in, n), chart_points(&h.net_out, n));
    let mk = |name: &'static str, color, pts| {
        Dataset::default()
            .name(name)
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(color))
            .data(pts)
    };
    let y = visible_max(&[&h.net_in, &h.net_out], n);
    let len = pin.len().max(pout.len());
    line_chart(
        vec![mk("in", c.base0c, &pin), mk("out", c.base09, &pout)],
        len,
        y,
        fmt_bytes(y as u64),
        inner,
        buf,
    );
}

fn render_cpu(h: &MetricsHistory, area: Rect, buf: &mut Buffer) {
    let last = h
        .cpu_pct
        .last()
        .map_or("n/a".into(), |v| format!("{v:.1} %"));
    let inner = panel(format!("CPU  {last}"), area, buf);
    if h.cpu_pct.is_empty() {
        return empty_state(h, inner, buf);
    }
    let data = spark_data(&h.cpu_pct, usize::from(inner.width), 100.0);
    spark(&data, 500, config::get().colors.base0d, inner, buf);
}

fn render_hit_rate(h: &MetricsHistory, area: Rect, buf: &mut Buffer) {
    let windowed = h.latest.as_ref().and_then(|s| s.hit_rate);
    let lifetime = h.latest.as_ref().and_then(|s| s.hit_rate_lifetime);
    let pct = |v: Option<f64>| v.map_or("n/a".into(), fmt_pct);
    let inner = panel(
        format!("Hit rate  {} (lifetime {})", pct(windowed), pct(lifetime)),
        area,
        buf,
    );
    if h.hit_rate.is_empty() && windowed.is_none() {
        return empty_state(h, inner, buf);
    }
    let [g, s] = Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(inner);
    let color = windowed.map_or(config::get().colors.base05, hit_rate_color);
    Gauge::default()
        .ratio(windowed.unwrap_or(0.0).clamp(0.0, 1.0))
        .gauge_style(Style::default().fg(color))
        .label(pct(windowed))
        .render(g, buf);
    spark(
        &spark_data(&h.hit_rate, usize::from(s.width), 100.0),
        100,
        color,
        s,
        buf,
    );
}

fn render_clients(h: &MetricsHistory, area: Rect, buf: &mut Buffer) {
    let snap = h.latest_snapshot.as_ref();
    let num = |v: Option<u64>| v.map_or("n/a".into(), |v| v.to_string());
    let connected = h.clients.last().map(|v| v as u64);
    let inner = panel(
        format!(
            "Clients  {}/{} · blocked {}",
            num(connected),
            num(snap.and_then(|s| s.maxclients)),
            num(snap.and_then(|s| s.blocked_clients)),
        ),
        area,
        buf,
    );
    if h.clients.is_empty() {
        return empty_state(h, inner, buf);
    }
    let data = spark_ranged(&h.clients, usize::from(inner.width));
    spark(&data, 8, config::get().colors.base0d, inner, buf);
}

/// The two-line stat strip.
fn render_tiles(h: &MetricsHistory, paused: bool, area: Rect, buf: &mut Buffer) {
    let cfg = config::get();
    let snap = h.latest_snapshot.as_ref();
    let s = h.latest.as_ref();
    let na = || "n/a".to_string();
    let per_sec = |v: Option<f64>| v.map_or_else(na, |v| format!("{v:.1}/s"));

    let uptime = snap
        .and_then(|s| s.uptime_secs)
        .map_or_else(na, |u| format_duration(u as usize));
    let role = snap.and_then(|s| s.role.clone()).unwrap_or_else(na);
    let replicas = snap
        .and_then(|s| s.connected_slaves)
        .map_or_else(na, |n| n.to_string());
    let keys = snap
        .and_then(|s| s.total_keys)
        .map_or_else(na, |k| fmt_compact(k as f64));
    let (cluster, cluster_style) = match h.cluster.as_ref() {
        Some(c) if snap.is_some_and(|s| s.cluster_enabled) => {
            let style = if !c.state_ok || c.slots_fail > 0 {
                Style::default().fg(cfg.colors.base08)
            } else if c.slots_pfail > 0 {
                Style::default().fg(cfg.colors.base0b)
            } else {
                Style::default()
            };
            let state = if c.state_ok { "ok" } else { "fail" };
            (
                format!(
                    "{state} · {} nodes · {} masters · {}/16384 slots",
                    c.known_nodes, c.size, c.slots_ok
                ),
                style,
            )
        }
        _ if snap.is_some_and(|s| s.cluster_enabled) => {
            ("collecting…".to_string(), Style::default())
        }
        _ => ("standalone".to_string(), Style::default()),
    };
    let rejected = s.and_then(|s| s.rejected_ps);
    let rejected_style = if rejected.is_some_and(|r| r > 0.0) {
        Style::default().fg(cfg.colors.base08)
    } else {
        Style::default()
    };

    // (display position, tile), listed in priority order: when the row is too
    // narrow the lowest-priority tiles drop off whole instead of being cut.
    let tiles = [
        (0, Span::raw(format!("up {uptime}"))),
        (1, Span::raw(format!("{role} · {replicas} replicas"))),
        (2, Span::raw(format!("keys {keys}"))),
        (
            6,
            Span::styled(format!("cluster: {cluster}"), cluster_style),
        ),
        (
            5,
            Span::styled(format!("rejected {}", per_sec(rejected)), rejected_style),
        ),
        (
            4,
            Span::raw(format!("expired {}", per_sec(s.and_then(|s| s.expired_ps)))),
        ),
        (
            3,
            Span::raw(format!("evicted {}", per_sec(s.and_then(|s| s.evicted_ps)))),
        ),
    ];
    let sep_w = 3;
    let mut used = 0;
    let mut shown = Vec::new();
    for (pos, span) in tiles {
        let w = span.width() + if shown.is_empty() { 0 } else { sep_w };
        if used + w > usize::from(area.width) {
            break;
        }
        used += w;
        shown.push((pos, span));
    }
    shown.sort_by_key(|(pos, _)| *pos);
    let mut line1_spans = Vec::new();
    for (n, (_, span)) in shown.into_iter().enumerate() {
        if n > 0 {
            line1_spans.push(Span::styled(" │ ", Style::default().fg(cfg.colors.base03)));
        }
        line1_spans.push(span);
    }
    let line1 = Line::from(line1_spans);

    let interval = cfg.dashboard.interval();
    let window = (h.window_len() as f64 * interval.as_secs_f64()).round() as usize;
    let mut spans = Vec::new();
    if paused {
        spans.push(Span::styled(
            "[PAUSED] ",
            Style::default().fg(cfg.colors.base08).bold(),
        ));
    }
    spans.push(Span::raw(format!(
        "window: last {} · sample {}s",
        format_duration(window),
        interval.as_secs_f64()
    )));

    Paragraph::new(vec![line1, Line::from(spans)]).render(area, buf);
}

fn render_full(h: &MetricsHistory, paused: bool, area: Rect, buf: &mut Buffer) {
    let [grid, tiles] = Layout::vertical([Constraint::Fill(1), Constraint::Length(2)]).areas(area);
    let rows: [Rect; 3] = Layout::vertical([Constraint::Fill(1); 3]).areas(grid);
    let panels: [[PanelFn; 2]; 3] = [
        [render_ops, render_memory],
        [render_network, render_cpu],
        [render_hit_rate, render_clients],
    ];
    for (row, fns) in rows.into_iter().zip(panels) {
        let cols: [Rect; 2] =
            Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(row);
        for (col, f) in cols.into_iter().zip(fns) {
            f(h, col, buf);
        }
    }
    render_tiles(h, paused, tiles, buf);
}

/// One row per metric: label (12) | value (12) | sparkline (rest).
fn render_compact(h: &MetricsHistory, paused: bool, area: Rect, buf: &mut Buffer) {
    let c = &config::get().colors;
    let [rows_area, tiles] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(2)]).areas(area);
    let empty = empty_text(h).to_string();
    let val = |v: Option<String>| v.unwrap_or_else(|| empty.clone());
    let rows: [CompactRow; 7] = {
        let w = usize::from(rows_area.width.saturating_sub(24));
        [
            (
                "Ops/sec",
                h.ops.last().map(fmt_compact),
                spark_data(&h.ops, w, 1.0),
                100,
                c.base0d,
            ),
            (
                "Memory",
                h.used_memory.last().map(|v| fmt_bytes(v as u64)),
                spark_ranged(&h.used_memory, w),
                8,
                c.base0a,
            ),
            (
                "Net in",
                h.net_in.last().map(fmt_rate_bytes),
                spark_data(&h.net_in, w, 1.0),
                1024,
                c.base0c,
            ),
            (
                "Net out",
                h.net_out.last().map(fmt_rate_bytes),
                spark_data(&h.net_out, w, 1.0),
                1024,
                c.base09,
            ),
            (
                "CPU",
                h.cpu_pct.last().map(|v| format!("{v:.1} %")),
                spark_data(&h.cpu_pct, w, 100.0),
                500,
                c.base0d,
            ),
            (
                "Hit rate",
                h.latest.as_ref().and_then(|s| s.hit_rate).map(fmt_pct),
                spark_data(&h.hit_rate, w, 100.0),
                100,
                c.base0a,
            ),
            (
                "Clients",
                h.clients.last().map(|v| (v as u64).to_string()),
                spark_ranged(&h.clients, w),
                8,
                c.base0d,
            ),
        ]
    };
    for (i, (label, value, data, floor, color)) in rows.into_iter().enumerate() {
        let Ok(dy) = u16::try_from(i) else { break };
        if dy >= rows_area.height {
            break;
        }
        let y = rows_area.y + dy;
        let [l, v, s] = Layout::horizontal([
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Fill(1),
        ])
        .areas(Rect {
            y,
            height: 1,
            ..rows_area
        });
        Paragraph::new(label).render(l, buf);
        Paragraph::new(val(value)).render(v, buf);
        spark(&data, floor, color, s, buf);
    }
    render_tiles(h, paused, tiles, buf);
}

/// One plotted line: label, values (already scaled for display), colour.
struct PlotLine {
    label: &'static str,
    values: Vec<f64>,
    color: Color,
}

/// Y-axis style of a metric.
struct Spec {
    lines: Vec<PlotLine>,
    fmt: fn(f64) -> String,
    /// Fixed Y range, e.g. `0..=100` for a percentage.
    fixed: Option<(f64, f64)>,
    /// Start the axis at 0 (rates) rather than zooming on the data (gauges).
    zero_based: bool,
}

fn fmt_percent(v: f64) -> String {
    format!("{v:.1} %")
}

fn fmt_bytes_f(v: f64) -> String {
    fmt_bytes(v.max(0.0) as u64)
}

fn metric_spec(h: &MetricsHistory, m: Metric, n: usize) -> Spec {
    let c = &config::get().colors;
    let line = |label, s: &Series, scale: f64, color| PlotLine {
        label,
        values: s.tail(n).map(|v| v * scale).collect(),
        color,
    };
    let (lines, fmt, fixed, zero_based): (Vec<PlotLine>, fn(f64) -> String, _, _) = match m {
        Metric::Ops => (
            vec![line("ops/s", &h.ops, 1.0, c.base0d)],
            fmt_compact,
            None,
            true,
        ),
        Metric::Memory => (
            vec![line("used", &h.used_memory, 1.0, c.base0a)],
            fmt_bytes_f,
            None,
            false,
        ),
        Metric::Network => (
            vec![
                line("in", &h.net_in, 1.0, c.base0c),
                line("out", &h.net_out, 1.0, c.base09),
            ],
            fmt_rate_bytes,
            None,
            true,
        ),
        Metric::Cpu => (
            vec![line("cpu", &h.cpu_pct, 1.0, c.base0d)],
            fmt_percent,
            None,
            true,
        ),
        Metric::HitRate => (
            vec![line("hit rate", &h.hit_rate, 100.0, c.base0a)],
            fmt_percent,
            Some((0.0, 100.0)),
            true,
        ),
        Metric::Clients => (
            vec![line("clients", &h.clients, 1.0, c.base0d)],
            fmt_compact,
            None,
            false,
        ),
        Metric::Keys => (
            vec![line("keys", &h.total_keys, 1.0, c.base0b)],
            fmt_compact,
            None,
            false,
        ),
    };
    Spec {
        lines,
        fmt,
        fixed,
        zero_based,
    }
}

/// Y range for the data: fixed, zero-based, or zoomed with some padding.
fn y_range(spec: &Spec) -> (f64, f64) {
    if let Some(r) = spec.fixed {
        return r;
    }
    let all = spec.lines.iter().flat_map(|l| l.values.iter().copied());
    let (lo, hi) = all.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
        (lo.min(v), hi.max(v))
    });
    if !lo.is_finite() {
        return (0.0, 1.0);
    }
    if spec.zero_based {
        return (0.0, if hi > 0.0 { hi * 1.1 } else { 1.0 });
    }
    let pad = ((hi - lo) * 0.2).max(hi.abs() * 0.01).max(1.0);
    ((lo - pad).max(0.0), hi + pad)
}

/// The selected metric as one large chart with axes and a stats footer.
fn render_single(h: &MetricsHistory, selected: Metric, paused: bool, area: Rect, buf: &mut Buffer) {
    let cfg = config::get();
    let c = &cfg.colors;
    let [tabs, plot, stats] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(selected_stat_rows(selected)),
    ])
    .areas(area);

    // Tab strip: every metric, the selected one highlighted.
    let mut spans = Vec::new();
    for m in Metric::ALL {
        let style = if m == selected {
            Style::default().fg(c.base00).bg(c.base04).bold()
        } else {
            Style::default().fg(c.base03)
        };
        spans.push(Span::styled(format!(" {} ", m.name()), style));
    }
    if paused {
        spans.push(Span::styled(
            "  [PAUSED]",
            Style::default().fg(c.base08).bold(),
        ));
    }
    Paragraph::new(Line::from(spans)).render(tabs, buf);

    // Leave room for the Y labels; braille packs two points per cell.
    let n = usize::from(plot.width.saturating_sub(10)).max(1) * 2;
    let spec = metric_spec(h, selected, n);
    if spec.lines.iter().all(|l| l.values.is_empty()) {
        return empty_state(h, plot, buf);
    }
    let (lo, hi) = y_range(&spec);
    let interval = cfg.dashboard.interval().as_secs_f64();
    let span = (n as f64 * interval).round() as usize;

    // Right-align so "now" is always the right edge as history fills.
    let pts: Vec<Vec<(f64, f64)>> = spec
        .lines
        .iter()
        .map(|l| {
            let off = n.saturating_sub(l.values.len());
            l.values
                .iter()
                .enumerate()
                .map(|(i, v)| ((off + i) as f64, *v))
                .collect()
        })
        .collect();
    let datasets: Vec<Dataset<'_>> = spec
        .lines
        .iter()
        .zip(&pts)
        .map(|(l, p)| {
            Dataset::default()
                .name(l.label)
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(l.color))
                .data(p)
        })
        .collect();
    let mid = (lo + hi) / 2.0;
    Chart::new(datasets)
        .x_axis(
            Axis::default()
                .style(Style::default().fg(c.base03))
                .bounds([0.0, n as f64])
                .labels(vec![
                    Span::raw(format!("-{}", format_duration(span))),
                    Span::raw("now"),
                ]),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(c.base03))
                .bounds([lo, hi])
                .labels(vec![
                    Span::raw((spec.fmt)(lo)),
                    Span::raw((spec.fmt)(mid)),
                    Span::raw((spec.fmt)(hi)),
                ]),
        )
        .render(plot, buf);

    // Stats footer: now / min / avg / max per line.
    let rows: Vec<Line<'static>> = spec
        .lines
        .iter()
        .map(|l| {
            let v = &l.values;
            let text = if v.is_empty() {
                "n/a".to_string()
            } else {
                let min = v.iter().copied().fold(f64::INFINITY, f64::min);
                let max = v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                let avg = v.iter().sum::<f64>() / v.len() as f64;
                format!(
                    "now {}  min {}  avg {}  max {}",
                    (spec.fmt)(v[v.len() - 1]),
                    (spec.fmt)(min),
                    (spec.fmt)(avg),
                    (spec.fmt)(max)
                )
            };
            Line::from(vec![
                Span::styled(format!("{:<8}", l.label), Style::default().fg(l.color)),
                Span::raw(text),
            ])
        })
        .collect();
    Paragraph::new(rows).render(stats, buf);
}

fn selected_stat_rows(m: Metric) -> u16 {
    if m == Metric::Network {
        2
    } else {
        1
    }
}

impl StatefulWidget for DashboardWidget {
    type State = Dashboard;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let cfg = config::get();
        // Paused: draw the frozen copy. Live: one short lock, then draw from
        // the clone.
        let live;
        let h = match &state.frozen {
            Some(f) => f,
            None => {
                live = state.metrics.lock().window(usize::from(area.width) * 2);
                &live
            }
        };
        let paused = state.is_paused();

        let block = Block::default()
            .title(" Dashboard ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(cfg.colors.base04))
            .fg(cfg.colors.base05);
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.is_empty() {
            return;
        }

        if !state.all_charts && inner.height >= 6 {
            // One big chart plus a one-line stat strip.
            let [chart, tiles] =
                Layout::vertical([Constraint::Fill(1), Constraint::Length(2)]).areas(inner);
            render_single(h, state.selected, paused, chart, buf);
            render_tiles(h, paused, tiles, buf);
        } else if inner.width >= FULL_MIN.0 && inner.height >= FULL_MIN.1 {
            render_full(h, paused, inner, buf);
        } else if inner.height >= COMPACT_MIN_HEIGHT {
            render_compact(h, paused, inner, buf);
        } else {
            // Tiles only; with a single row, just the first line.
            render_tiles(h, paused, inner, buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::redis_client::metrics::InfoSnapshot;

    fn history(samples: u64) -> MetricsHistory {
        let t0 = Instant::now();
        let mut h = MetricsHistory::new(100);
        for i in 0..samples {
            let mut s = InfoSnapshot::empty(t0 + Duration::from_secs(i));
            s.total_commands = Some(i * 1000);
            s.net_in_bytes = Some(i * 4096);
            s.net_out_bytes = Some(i * 8192);
            s.used_memory = Some(1_000_000 + i * 1000);
            s.maxmemory = Some(10_000_000);
            s.connected_clients = Some(3);
            s.keyspace_hits = Some(i * 90);
            s.keyspace_misses = Some(i * 10);
            s.cpu_user = Some(i as f64 * 0.1);
            s.cpu_sys = Some(i as f64 * 0.05);
            h.record(s);
        }
        h
    }

    fn dashboard(h: MetricsHistory) -> (Dashboard, Arc<Mutex<MetricsHistory>>) {
        let shared = Arc::new(Mutex::new(h));
        (Dashboard::new(shared.clone()), shared)
    }

    fn render_to_string(d: &mut Dashboard, w: u16, h: u16) -> String {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        StatefulWidget::render(DashboardWidget, area, &mut buf, d);
        buf.content().iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn narrow_tiles_drop_whole_tiles() {
        let h = history(3);
        let area = Rect::new(0, 0, 40, 2);
        let mut buf = Buffer::empty(area);
        render_tiles(&h, false, area, &mut buf);
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("up "));
        assert!(!text.contains("cluster:"));
        assert!(!text.contains("evicted"));
    }

    #[test]
    fn toggle_pause_freezes_and_resumes() {
        let (mut d, shared) = dashboard(history(3));
        d.toggle_pause();
        assert!(d.is_paused());
        let frozen_len = d.frozen.as_ref().unwrap().ops.len();
        shared.lock().ops.push(1.0);
        assert_eq!(d.frozen.as_ref().unwrap().ops.len(), frozen_len);
        assert_eq!(shared.lock().ops.len(), frozen_len + 1);
        d.toggle_pause();
        assert!(!d.is_paused());
    }

    #[test]
    fn helpers_slice_and_scale() {
        let mut s = Series::new(10);
        for v in [0.1, 0.5, 0.25] {
            s.push(v);
        }
        assert_eq!(chart_points(&s, 2), vec![(0.0, 0.5), (1.0, 0.25)]);
        assert_eq!(spark_data(&s, 10, 100.0), vec![10, 50, 25]);
        assert_eq!(visible_max(&[&Series::new(3)], 3), 1.0);
    }

    #[test]
    fn renders_full_and_compact_without_panic() {
        let (mut d, _s) = dashboard(history(5));
        d.toggle_all();
        assert!(render_to_string(&mut d, 120, 40).contains("Ops/sec"));
        assert!(render_to_string(&mut d, 60, 15).contains("Ops/sec"));
        d.toggle_all();
        for _ in 0..Metric::ALL.len() {
            let out = render_to_string(&mut d, 100, 30);
            assert!(out.contains("now") || out.contains("n/a"));
            d.next_chart();
        }
        d.toggle_pause();
        assert!(render_to_string(&mut d, 120, 40).contains("[PAUSED]"));
    }

    #[test]
    fn renders_collecting_when_empty() {
        let (mut d, _s) = dashboard(MetricsHistory::new(10));
        assert!(render_to_string(&mut d, 120, 40).contains("collecting"));
        assert!(render_to_string(&mut d, 60, 15).contains("collecting"));
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        let (mut d, _s) = dashboard(history(5));
        for (w, h) in [(20, 5), (1, 1), (0, 0), (80, 8), (200, 3)] {
            render_to_string(&mut d, w, h);
        }
    }
}
