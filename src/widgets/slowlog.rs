//! The Slowlog screen: the server's slowest recent commands.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph, StatefulWidget, Widget, Wrap},
};

use crate::{
    config,
    redis_client::{
        bytes::bytes_from_text,
        escape::{printable, quote_arg},
        slowlog::{SlowlogData, SlowlogEntry},
    },
    utils::group_thousands,
    widgets::{
        local_time::hms,
        text_view::{self, TextCache, TextLabels, TextView},
    },
};

/// Width of the client column.
const CLIENT_WIDTH: usize = 21;
/// Below this width the client column is left out.
const CLIENT_MIN_AREA: usize = 70;
const DAY_SECS: u64 = 24 * 60 * 60;

/// The table's order, newest or slowest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SlowlogSort {
    #[default]
    Time,
    Duration,
}

struct EntryDetail {
    text: TextCache,
    view: TextView,
}

/// State of the Slowlog screen. It owns its copy of the data, so drawing
/// locks nothing.
#[derive(Default)]
pub struct Slowlog {
    data: SlowlogData,
    loaded: bool,
    sort: SlowlogSort,
    /// Row of the cursor in the sorted table.
    selected: usize,
    top: usize,
    detail: Option<EntryDetail>,
}

impl Slowlog {
    /// New data: sorted as before, the cursor staying on its entry.
    pub fn load(&mut self, data: SlowlogData) {
        let id = self.selected_entry().map(|e| e.id);
        self.data = data;
        self.loaded = true;
        self.apply_sort(id);
    }

    fn apply_sort(&mut self, keep: Option<u64>) {
        match self.sort {
            SlowlogSort::Time => self
                .data
                .entries
                .sort_by_key(|e| std::cmp::Reverse((e.ts_secs, e.id))),
            SlowlogSort::Duration => self
                .data
                .entries
                .sort_by_key(|e| std::cmp::Reverse((e.micros, e.id))),
        }
        let found = keep.and_then(|id| self.data.entries.iter().position(|e| e.id == id));
        self.selected = found
            .unwrap_or(self.selected)
            .min(self.data.entries.len().saturating_sub(1));
    }

    /// `o`: newest first ↔ slowest first.
    pub fn toggle_sort(&mut self) {
        let id = self.selected_entry().map(|e| e.id);
        self.sort = match self.sort {
            SlowlogSort::Time => SlowlogSort::Duration,
            SlowlogSort::Duration => SlowlogSort::Time,
        };
        self.apply_sort(id);
    }

    fn selected_entry(&self) -> Option<&SlowlogEntry> {
        self.data.entries.get(self.selected)
    }

    /// `j` / `k`: the detail's text, or the table's cursor.
    pub fn move_selection(&mut self, down: bool) {
        if let Some(detail) = &mut self.detail {
            detail.view.scroll(down, 1);
        } else if down {
            if self.selected + 1 < self.data.entries.len() {
                self.selected += 1;
            }
        } else {
            self.selected = self.selected.saturating_sub(1);
        }
    }

    pub fn pan_detail(&mut self, right: bool) {
        if let Some(detail) = &mut self.detail {
            detail.view.pan(right);
        }
    }

    pub fn toggle_detail_wrap(&mut self) {
        if let Some(detail) = &mut self.detail {
            detail.view.toggle_wrap();
        }
    }

    /// Enter: the selected entry, every argument on its own row.
    pub fn open_detail(&mut self) {
        if let Some(entry) = self.selected_entry() {
            self.detail = Some(EntryDetail {
                text: TextCache::from_lines(detail_lines(entry)),
                view: TextView::default(),
            });
        }
    }

    #[must_use]
    pub fn is_detail(&self) -> bool {
        self.detail.is_some()
    }

    pub fn close_detail(&mut self) {
        self.detail = None;
    }
}

/// Draws the screen; `wall` dates the entries.
pub struct SlowlogWidget {
    pub wall: SystemTime,
}

impl StatefulWidget for SlowlogWidget {
    type State = Slowlog;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Slowlog) {
        let colors = &config::get().colors;
        let block = Block::default()
            .title(title(state))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(colors.base04)
            .fg(colors.base05);
        let inner = block.inner(area);
        block.render(area, buf);

        if let Some(detail) = &mut state.detail {
            text_view::render(
                &detail.text,
                &TextLabels {
                    header: "Command",
                    paging: false,
                    close: Some("Esc close"),
                    changed: &[],
                    highlight: Style::new(),
                },
                None,
                true,
                inner,
                buf,
                &mut detail.view,
            );
            return;
        }
        if let Some(error) = &state.data.error {
            Paragraph::new(Line::from(format!("SLOWLOG failed: {}", printable(error))))
                .fg(colors.base08)
                .wrap(Wrap { trim: false })
                .render(inner, buf);
            return;
        }
        if state.data.entries.is_empty() {
            let text = if state.loaded {
                "The slowlog is empty."
            } else {
                "Loading…"
            };
            Paragraph::new(Line::from(text).fg(colors.base03)).render(inner, buf);
            return;
        }

        let width = usize::from(inner.width);
        let rows = usize::from(inner.height.saturating_sub(1));
        if state.selected < state.top {
            state.top = state.selected;
        } else if rows > 0 && state.selected >= state.top + rows {
            state.top = state.selected + 1 - rows;
        }
        state.top = state
            .top
            .min(state.data.entries.len().saturating_sub(rows.max(1)));
        let mut lines = vec![header(width)];
        lines.extend(
            state
                .data
                .entries
                .iter()
                .enumerate()
                .skip(state.top)
                .take(rows)
                .map(|(i, entry)| row(entry, i == state.selected, width, self.wall)),
        );
        Paragraph::new(lines).render(inner, buf);
    }
}

fn title(state: &Slowlog) -> String {
    let sort = match state.sort {
        SlowlogSort::Time => "newest first",
        SlowlogSort::Duration => "slowest first",
    };
    let threshold = match state.data.slower_than {
        None => "threshold unknown".to_owned(),
        Some(n) if n < 0 => "logging off".to_owned(),
        Some(0) => "logs every command".to_owned(),
        Some(n) => format!("over {}", fmt_micros(n.unsigned_abs())),
    };
    let max = state
        .data
        .max_len
        .map_or_else(|| "unknown".to_owned(), |n| n.to_string());
    format!(
        " Slowlog · {} · {sort} · {threshold} · max {max} ",
        group_thousands(state.data.entries.len())
    )
}

fn header(width: usize) -> Line<'static> {
    let client = if width >= CLIENT_MIN_AREA {
        format!("{:<CLIENT_WIDTH$} ", "CLIENT")
    } else {
        String::new()
    };
    Line::from(format!(
        "{:>7} {:<9} {:>9} {client}COMMAND",
        "ID", "TIME", "DURATION"
    ))
    .fg(config::get().colors.base03)
    .bold()
}

fn row(entry: &SlowlogEntry, selected: bool, width: usize, wall: SystemTime) -> Line<'static> {
    let colors = &config::get().colors;
    let mut spans = vec![
        Span::from(format!("{:>7} ", entry.id)).fg(colors.base03),
        Span::from(format!("{:<9} ", when(entry.ts_secs, wall))).fg(colors.base04),
        Span::from(format!("{:>9} ", fmt_micros(entry.micros))).fg(colors.base0b),
    ];
    if width >= CLIENT_MIN_AREA {
        spans.push(Span::from(format!("{} ", client_cell(&entry.client))).fg(colors.base03));
    }
    let used: usize = spans.iter().map(Span::width).sum();
    let mut text = String::new();
    for arg in &entry.args {
        if text.chars().count() + used >= width {
            break;
        }
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&quote_arg(&bytes_from_text(arg)));
    }
    spans.push(Span::from(text).fg(colors.base05));
    let line = Line::from(spans);
    if selected {
        line.style(Style::new().bg(colors.base02))
    } else {
        line
    }
}

/// Today's time of day, or how many days ago.
fn when(ts_secs: u64, wall: SystemTime) -> String {
    let now = wall.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let age = now.saturating_sub(ts_secs);
    if age >= DAY_SECS {
        format!("{}d ago", age / DAY_SECS)
    } else {
        hms(UNIX_EPOCH + Duration::from_secs(ts_secs))
    }
}

fn client_cell(client: &str) -> String {
    let shown = printable(client);
    let count = shown.chars().count();
    if count > CLIENT_WIDTH {
        let tail: String = shown.chars().skip(count - (CLIENT_WIDTH - 1)).collect();
        format!("…{tail}")
    } else {
        format!("{shown:<CLIENT_WIDTH$}")
    }
}

/// `850µs`, `12.4ms`, `1.20s`.
#[allow(clippy::cast_precision_loss)] // A duration shown to a few digits.
#[must_use]
pub fn fmt_micros(micros: u64) -> String {
    if micros < 1000 {
        format!("{micros}µs")
    } else if micros < 1_000_000 {
        format!("{:.1}ms", micros as f64 / 1000.0)
    } else {
        format!("{:.2}s", micros as f64 / 1_000_000.0)
    }
}

fn detail_lines(entry: &SlowlogEntry) -> Vec<String> {
    let mut head = format!(
        "#{}  {}  {}",
        entry.id,
        hms(UNIX_EPOCH + Duration::from_secs(entry.ts_secs)),
        fmt_micros(entry.micros)
    );
    if !entry.client.is_empty() {
        head.push_str("  ");
        head.push_str(&printable(&entry.client));
    }
    if !entry.name.is_empty() {
        head.push_str(" (");
        head.push_str(&printable(&entry.name));
        head.push(')');
    }
    let mut out = vec![head, String::new()];
    out.extend(entry.args.iter().enumerate().map(|(i, arg)| {
        if i == 0 {
            printable(arg).into_owned()
        } else {
            format!("[{i}] {}", printable(arg))
        }
    }));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u64, ts_secs: u64, micros: u64) -> SlowlogEntry {
        SlowlogEntry {
            id,
            ts_secs,
            micros,
            args: vec!["GET".into(), format!("k{id}")],
            client: "1.2.3.4:5".into(),
            name: String::new(),
        }
    }

    fn loaded() -> Slowlog {
        let mut s = Slowlog::default();
        s.load(SlowlogData {
            entries: vec![entry(1, 100, 900), entry(2, 200, 50), entry(3, 300, 400)],
            ..SlowlogData::default()
        });
        s
    }

    fn ids(s: &Slowlog) -> Vec<u64> {
        s.data.entries.iter().map(|e| e.id).collect()
    }

    #[test]
    fn sorts_by_time_and_duration_keeping_the_cursor() {
        let mut s = loaded();
        assert_eq!(ids(&s), [3, 2, 1]);
        s.move_selection(true);
        assert_eq!(s.selected_entry().unwrap().id, 2);
        s.toggle_sort();
        assert_eq!(ids(&s), [1, 3, 2]);
        assert_eq!(s.selected_entry().unwrap().id, 2, "same entry");
        s.toggle_sort();
        assert_eq!(ids(&s), [3, 2, 1]);
    }

    #[test]
    fn a_reload_keeps_the_entry_or_clamps() {
        let mut s = loaded();
        s.move_selection(true);
        s.move_selection(true);
        assert_eq!(s.selected_entry().unwrap().id, 1);
        s.load(SlowlogData {
            entries: vec![entry(1, 100, 900), entry(4, 400, 1)],
            ..SlowlogData::default()
        });
        assert_eq!(s.selected_entry().unwrap().id, 1);
        s.load(SlowlogData::default());
        assert_eq!(s.selected, 0);
        assert!(s.selected_entry().is_none());
        s.move_selection(true);
        s.open_detail();
        assert!(!s.is_detail(), "nothing to open");
    }

    #[test]
    fn durations_read_as_humans_do() {
        for (micros, text) in [
            (0, "0µs"),
            (999, "999µs"),
            (1000, "1.0ms"),
            (12_400, "12.4ms"),
            (1_200_000, "1.20s"),
        ] {
            assert_eq!(fmt_micros(micros), text);
        }
    }

    #[test]
    fn old_entries_show_their_age() {
        let wall = UNIX_EPOCH + Duration::from_secs(10 * DAY_SECS);
        assert_eq!(when(DAY_SECS, wall), "9d ago");
    }

    #[test]
    fn the_title_says_when_a_setting_is_unknown_or_off() {
        let mut s = Slowlog::default();
        assert!(title(&s).contains("threshold unknown"));
        assert!(title(&s).contains("max unknown"));
        s.data.slower_than = Some(-1);
        assert!(title(&s).contains("logging off"));
        s.data.slower_than = Some(0);
        assert!(title(&s).contains("logs every command"));
        s.data.slower_than = Some(10_000);
        s.data.max_len = Some(128);
        assert!(title(&s).contains("over 10.0ms"), "{}", title(&s));
        assert!(title(&s).contains("max 128"));
    }

    #[test]
    fn the_detail_shows_each_argument() {
        let mut s = loaded();
        s.open_detail();
        let lines: Vec<String> = s
            .detail
            .as_ref()
            .unwrap()
            .text
            .lines
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(lines[2], "GET");
        assert_eq!(lines[3], "[1] k3");
    }
}
