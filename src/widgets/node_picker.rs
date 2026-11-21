//! One row naming the cluster's nodes, the selected one highlighted. The
//! dashboard, Slowlog and MONITOR share it.

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Widget,
};

use crate::config;

/// MONITOR sees the commands of one server only.
pub const MONITOR_NODE_WARNING: &str = "⚠ covers this node only";

/// The row above the Slowlog and MONITOR screens of a cluster.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeStrip {
    pub labels: Vec<String>,
    pub selected: usize,
    pub warning: Option<&'static str>,
}

impl NodeStrip {
    /// Draw the strip on the first row of `area`; returns the rest.
    pub fn render_above(&self, area: Rect, buf: &mut Buffer) -> Rect {
        let [row, rest] =
            Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(area);
        let warning = self.warning.unwrap_or_default();
        let width = u16::try_from(warning.chars().count()).unwrap_or(0);
        let [picker, note] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(width)]).areas(row);
        picker_line(&self.labels, self.selected, picker.width).render(picker, buf);
        if !warning.is_empty() {
            let style = Style::default().fg(config::get().colors.base0a);
            Line::styled(warning, style).render(note, buf);
        }
        rest
    }
}

/// The picker for `labels` in `width` columns. Labels that don't fit are
/// left out from the start, so the selected one always shows; `‹` marks it.
#[must_use]
pub fn picker_line(labels: &[String], selected: usize, width: u16) -> Line<'static> {
    let colors = &config::get().colors;
    let cell = |label: &String| label.chars().count() + 2;
    let width = usize::from(width);
    let selected = selected.min(labels.len().saturating_sub(1));
    let mut first = 0;
    while first < selected && {
        let used: usize = labels[first..=selected].iter().map(cell).sum();
        used + usize::from(first > 0) > width
    } {
        first += 1;
    }
    let mut spans = Vec::new();
    let mut used = 0;
    if first > 0 {
        spans.push(Span::styled("‹", Style::default().fg(colors.base03)));
        used += 1;
    }
    for (index, label) in labels.iter().enumerate().skip(first) {
        used += cell(label);
        if used > width && index != selected {
            break;
        }
        let style = if index == selected {
            Style::default()
                .fg(colors.base00)
                .bg(colors.base04)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(colors.base03)
        };
        spans.push(Span::styled(format!(" {label} "), style));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::picker_line;

    fn text(labels: &[&str], selected: usize, width: u16) -> String {
        let labels: Vec<String> = labels.iter().map(|&l| l.to_owned()).collect();
        picker_line(&labels, selected, width).to_string()
    }

    #[test]
    fn everything_shows_when_it_fits() {
        assert_eq!(text(&["all", "a:1", "b:2"], 1, 40), " all  a:1  b:2 ");
    }

    #[test]
    fn the_selected_label_always_shows() {
        let labels = [
            "overview",
            "10.0.0.1:7000",
            "10.0.0.2:7000",
            "10.0.0.3:7000",
        ];
        let shown = text(&labels, 3, 34);
        assert!(shown.contains("10.0.0.3:7000"), "{shown}");
        assert!(shown.starts_with('‹'), "{shown}");
        assert!(shown.chars().count() <= 34, "{shown}");
        let shown = text(&labels, 0, 34);
        assert!(shown.starts_with(" overview "), "{shown}");
        assert!(shown.chars().count() <= 34, "{shown}");
    }
}
