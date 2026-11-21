#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

mod overview;
mod panels;

use overview::render_overview;
use panels::{empty_state, render_compact, render_full, render_tiles};

use std::sync::Arc;

use parking_lot::Mutex;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style, Stylize},
    symbols::Marker,
    text::{Line, Span},
    widgets::{
        Axis, Block, BorderType, Borders, Chart, Dataset, GraphType, Paragraph, StatefulWidget,
        Widget,
    },
};

use crate::{
    config,
    redis_client::metrics::{ClusterMetrics, ClusterSummary, MetricsHistory, NodeRow, Series},
    state::FrameData,
    utils::{fmt_bytes, fmt_compact, fmt_rate_bytes, format_duration},
    widgets::node_picker::picker_line,
};

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
    /// One history per master of a cluster.
    nodes: Arc<Mutex<ClusterMetrics>>,
    /// The master whose charts show; `None` is the overview.
    node: Option<String>,
    /// The overview as it was when paused.
    frozen_view: Option<ClusterView>,
}

/// What a cluster's dashboard shows above and instead of the charts.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClusterView {
    pub summary: Option<ClusterSummary>,
    pub rows: Vec<NodeRow>,
    /// Index in `rows` of the node on screen; `None` is the overview.
    pub selected: Option<usize>,
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
        let n = Self::ALL.len();
        let i = self.index();
        Self::ALL[if delta >= 0 {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }]
    }
}

impl Dashboard {
    pub fn new(metrics: Arc<Mutex<MetricsHistory>>) -> Self {
        Self {
            metrics,
            frozen: None,
            selected: Metric::Ops,
            all_charts: false,
            nodes: Arc::default(),
            node: None,
            frozen_view: None,
        }
    }

    /// Read the masters' histories from `nodes`.
    #[must_use]
    pub fn with_nodes(mut self, nodes: Arc<Mutex<ClusterMetrics>>) -> Self {
        self.nodes = nodes;
        self
    }

    /// `Tab`: on a cluster, the next stop of overview → each master →
    /// overview. On one server there are no nodes: the next chart.
    pub fn next_node(&mut self) {
        if !self.step_node(1) {
            self.next_chart();
        }
    }

    /// `Shift-Tab`: [`Self::next_node`] backwards.
    pub fn prev_node(&mut self) {
        if !self.step_node(-1) {
            self.prev_chart();
        }
    }

    /// Returns `false` when there are no nodes to step through.
    fn step_node(&mut self, delta: isize) -> bool {
        let ids = self.nodes.lock().ids();
        if ids.is_empty() {
            return false;
        }
        let stops = ids.len() + 1;
        let at = self
            .node
            .as_ref()
            .and_then(|node| ids.iter().position(|id| id == node))
            .map_or(0, |index| index + 1);
        let next = if delta >= 0 {
            (at + 1) % stops
        } else {
            (at + stops - 1) % stops
        };
        self.node = next.checked_sub(1).map(|index| ids[index].clone());
        // The frozen copy is of the node that was on screen.
        self.unpause();
        true
    }

    /// The picker and overview for this frame; `None` on one server.
    #[must_use]
    pub fn cluster_view(&self) -> Option<ClusterView> {
        if let Some(view) = &self.frozen_view {
            return Some(view.clone());
        }
        let rows = {
            let nodes = self.nodes.lock();
            if nodes.is_empty() {
                return None;
            }
            nodes.rows()
        };
        let summary = self.metrics.lock().cluster.clone();
        let selected = self
            .node
            .as_ref()
            .and_then(|node| rows.iter().position(|row| &row.id == node));
        Some(ClusterView {
            summary,
            rows,
            selected,
        })
    }

    /// The whole history of what is on screen.
    fn current_history(&self) -> MetricsHistory {
        if let Some(node) = &self.node {
            if let Some(history) = self.nodes.lock().history(node) {
                return history.clone();
            }
        }
        self.metrics.lock().clone()
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
        if self.frozen.is_some() {
            self.unpause();
        } else {
            self.frozen_view = self.cluster_view();
            self.frozen = Some(self.current_history());
        }
    }

    /// Resume live data (no-op when not paused).
    pub fn unpause(&mut self) {
        self.frozen = None;
        self.frozen_view = None;
    }

    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.frozen.is_some()
    }

    /// The live metrics for a frame `width` columns wide, copied under one
    /// short lock; `None` while paused (the frozen copy is drawn then).
    #[must_use]
    pub fn live_window(&self, width: u16) -> Option<MetricsHistory> {
        if self.is_paused() {
            return None;
        }
        let n = usize::from(width) * 2;
        if let Some(node) = &self.node {
            if let Some(window) = self.nodes.lock().window(node, n) {
                return Some(window);
            }
        }
        Some(self.metrics.lock().window(n))
    }
}

/// Draws the dashboard from the frame's metrics window (or the frozen copy).
pub struct DashboardWidget<'a> {
    pub frame: &'a FrameData,
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
#[allow(clippy::too_many_lines)]
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
    let mid = f64::midpoint(lo, hi);
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

impl StatefulWidget for DashboardWidget<'_> {
    type State = Dashboard;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let cfg = config::get();
        // Paused: draw the frozen copy. Live: the frame's copy.
        let Some(h) = state.frozen.as_ref().or(self.frame.metrics.as_ref()) else {
            return;
        };
        let paused = state.is_paused();

        let block = Block::default()
            .title(" Dashboard ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(cfg.colors.base04))
            .fg(cfg.colors.base05);
        let mut inner = block.inner(area);
        block.render(area, buf);
        if inner.is_empty() {
            return;
        }

        if let Some(view) = &self.frame.cluster_view {
            let [picker, rest] =
                Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(inner);
            let mut labels = vec!["overview".to_owned()];
            labels.extend(view.rows.iter().map(|row| row.addr.clone()));
            let at = view.selected.map_or(0, |index| index + 1);
            picker_line(&labels, at, picker.width).render(picker, buf);
            if view.selected.is_none() {
                render_overview(view, paused, rest, buf);
                return;
            }
            inner = rest;
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
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use parking_lot::Mutex;
    use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};

    use crate::redis_client::metrics::InfoSnapshot;
    use crate::redis_client::metrics::MetricsHistory;

    pub(super) fn history(samples: u64) -> MetricsHistory {
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

    #[test]
    fn tab_walks_the_nodes_on_a_cluster_and_the_charts_otherwise() {
        use crate::redis_client::cluster::MasterNode;

        let (mut single, _) = dashboard(history(3));
        single.next_node();
        assert!(single.selected == Metric::Memory, "one server: next chart");
        single.prev_node();
        assert!(single.selected == Metric::Ops);
        assert!(single.cluster_view().is_none());

        let mut nodes = ClusterMetrics::new(10);
        let master = |id: &str, port| MasterNode {
            id: id.into(),
            host: "h".into(),
            port,
            slots: vec![(port, port)],
        };
        nodes.sync(&[master("a", 1), master("b", 2)]);
        let (cluster, _) = dashboard(history(3));
        let mut cluster = cluster.with_nodes(Arc::new(Mutex::new(nodes)));
        let at = |d: &Dashboard| d.cluster_view().unwrap().selected;
        assert_eq!(at(&cluster), None, "starts on the overview");
        cluster.next_node();
        assert_eq!(at(&cluster), Some(0));
        cluster.next_node();
        assert_eq!(at(&cluster), Some(1));
        cluster.next_node();
        assert_eq!(at(&cluster), None, "wraps to the overview");
        cluster.prev_node();
        assert_eq!(at(&cluster), Some(1));
        assert!(cluster.selected == Metric::Ops, "the chart stays");
    }

    fn render_to_string(d: &mut Dashboard, w: u16, h: u16) -> String {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        let frame = FrameData {
            metrics: d.live_window(w),
            ..FrameData::default()
        };
        StatefulWidget::render(DashboardWidget { frame: &frame }, area, &mut buf, d);
        buf.content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
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
