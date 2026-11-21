//! The cluster overview: state, slots, and one row per master.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};

use super::ClusterView;
use crate::{
    config,
    utils::{fmt_bytes, fmt_compact},
};

const NODE_WIDTH: usize = 24;

fn row(node: &str, ops: &str, memory: &str, keys: &str, clients: &str) -> String {
    format!("{node:<NODE_WIDTH$}{ops:>10}{memory:>12}{keys:>10}{clients:>9}")
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn memory(value: f64) -> String {
    fmt_bytes(value.max(0.0) as u64)
}

pub(super) fn render_overview(view: &ClusterView, paused: bool, area: Rect, buf: &mut Buffer) {
    let colors = &config::get().colors;
    let summary = view.summary.as_ref().map_or_else(
        || "cluster: collecting…".to_owned(),
        |c| {
            format!(
                "state {} · slots {} ok · {} fail · {} known nodes",
                if c.state_ok { "ok" } else { "fail" },
                c.slots_ok,
                c.slots_fail + c.slots_pfail,
                c.known_nodes
            )
        },
    );
    let failing = view
        .summary
        .as_ref()
        .is_some_and(|c| !c.state_ok || c.slots_fail > 0);
    let summary_style = if failing {
        Style::default()
            .fg(colors.base08)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let mut first = vec![Span::styled(summary, summary_style)];
    if paused {
        first.push(Span::styled(
            "  [PAUSED]",
            Style::default()
                .fg(colors.base08)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let mut lines = vec![
        Line::from(first),
        Line::default(),
        Line::styled(
            row("Master", "ops/sec", "memory", "keys", "clients"),
            Style::default().fg(colors.base03),
        ),
    ];
    let dash = "—";
    for node in &view.rows {
        let text = |value: Option<f64>, format: fn(f64) -> String| {
            value.map_or_else(|| dash.to_owned(), format)
        };
        if node.up {
            lines.push(Line::raw(row(
                &node.addr,
                &text(node.ops, fmt_compact),
                &text(node.memory, memory),
                &text(node.keys, fmt_compact),
                &text(node.clients, fmt_compact),
            )));
        } else {
            lines.push(Line::styled(
                row(&node.addr, "down", dash, dash, dash),
                Style::default().fg(colors.base08),
            ));
        }
    }
    Paragraph::new(lines).render(area, buf);
}
