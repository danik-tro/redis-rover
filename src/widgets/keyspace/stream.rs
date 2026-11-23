//! A stream in the value pane: entries newest first, its consumer groups,
//! and one entry or group opened with Enter. Read-only.

use std::time::{Duration, UNIX_EPOCH};

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::{Cell, HighlightSpacing, Paragraph, Row, StatefulWidget, Table, TableState, Widget},
};
use unicode_width::UnicodeWidthStr;

use super::{
    live,
    mouse::below_header,
    render::{highlight_symbol, render_header_rule, render_value_footer, selected_row_style},
    state::Focus,
    KeySpace,
};
use crate::{
    config,
    redis_client::{
        escape::printable,
        types::{GroupDetail, GroupInfo, KeyValue, StreamEntry, StreamValue},
    },
    utils::group_thousands,
    widgets::{
        local_time::hms,
        slowlog::fmt_micros,
        text_view::{self, TextCache, TextLabels, TextView},
    },
};

/// Below this width the entries table leaves out the time.
const TIME_MIN_WIDTH: u16 = 44;
/// Widest field column in an opened entry.
const FIELD_COLUMN: usize = 24;
/// Fields previewed in an entries row.
const PREVIEW_FIELDS: usize = 3;

/// Which table the pane shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamTab {
    #[default]
    Entries,
    Groups,
}

/// One entry or group opened with Enter.
struct StreamOpen {
    header: String,
    text: TextCache,
    view: TextView,
    /// The group this shows, until its data lands.
    group: Option<(String, String)>,
}

/// The pane's state. Forgotten when another key is shown.
#[derive(Default)]
pub(super) struct StreamPane {
    tab: StreamTab,
    open: Option<StreamOpen>,
    key: Option<String>,
}

impl KeySpace {
    fn stream_value(&self) -> Option<&StreamValue> {
        match self.selected_value.as_ref().map(|loaded| &loaded.value) {
            Some(KeyValue::Stream(stream)) => Some(stream),
            _ => None,
        }
    }

    /// The value on show is a stream.
    #[must_use]
    pub fn is_stream_shown(&self) -> bool {
        self.stream_value().is_some()
    }

    /// Rows of the table on show.
    pub(super) fn stream_row_count(&self, stream: &StreamValue) -> usize {
        match self.stream.tab {
            StreamTab::Entries => stream.entries.len(),
            StreamTab::Groups => stream.groups.as_ref().map_or(0, Vec::len),
        }
    }

    /// The cursor is inside a stream: element writes don't apply.
    #[must_use]
    pub fn is_stream_focused(&self) -> bool {
        self.focus == Focus::Value && self.stream_value().is_some()
    }

    /// `n` / `p` and a cursor past the end don't turn the entries page while
    /// the groups or an opened row are on show.
    #[must_use]
    pub fn stream_blocks_paging(&self) -> bool {
        self.is_stream_focused() && (self.stream.tab == StreamTab::Groups || self.is_stream_open())
    }

    #[must_use]
    pub fn is_stream_open(&self) -> bool {
        self.stream.open.is_some()
    }

    /// A new value landed: another key starts on its entries.
    pub(super) fn sync_stream_pane(&mut self) {
        let key = self
            .stream_value()
            .and_then(|_| self.table.selected())
            .and_then(|row| self.key_at(row))
            .map(|meta| meta.key.clone());
        if key != self.stream.key {
            self.stream = StreamPane {
                key,
                ..StreamPane::default()
            };
        }
    }

    /// `Tab`: entries ↔ groups, with the cursor in the table. Returns
    /// `false` when no stream is shown.
    pub fn toggle_stream_tab(&mut self) -> bool {
        if self.stream_value().is_none() {
            return false;
        }
        self.stream.open = None;
        self.stream.tab = match self.stream.tab {
            StreamTab::Entries => StreamTab::Groups,
            StreamTab::Groups => StreamTab::Entries,
        };
        if self.value_row_count() > 0 {
            self.focus = Focus::Value;
            self.value_table.select(Some(0));
        } else {
            self.exit_value_focus();
        }
        true
    }

    /// Enter on a row: an entry opens at once; a group returns its
    /// `(key, group)` for the App to fetch.
    pub fn open_stream_row(&mut self) -> Option<(String, String)> {
        if self.is_stream_open() {
            return None;
        }
        let row = self.value_table.selected()?;
        let stream = self.stream_value()?;
        match self.stream.tab {
            StreamTab::Entries => {
                let entry = stream.entries.get(row)?;
                let open = StreamOpen {
                    header: format!("Entry {}", printable(&entry.id)),
                    text: TextCache::new(entry_lines(entry), false),
                    view: TextView::default(),
                    group: None,
                };
                self.stream.open = Some(open);
                None
            }
            StreamTab::Groups => {
                let name = stream.groups.as_ref()?.get(row)?.name.clone();
                let key = self.stream.key.clone()?;
                self.stream.open = Some(StreamOpen {
                    header: format!("Group {}", printable(&name)),
                    text: TextCache::from_lines(vec!["Loading…".to_owned()]),
                    view: TextView::default(),
                    group: Some((key.clone(), name.clone())),
                });
                Some((key, name))
            }
        }
    }

    /// A group's consumers and pending entries landed. Dropped if the user
    /// has moved on.
    pub fn set_stream_group(&mut self, detail: &GroupDetail) {
        if let Some(open) = &mut self.stream.open {
            if open.group.as_ref() == Some(&(detail.key.clone(), detail.group.clone())) {
                open.text = TextCache::from_lines(group_lines(detail));
                open.view = TextView::default();
            }
        }
    }

    /// Esc: close the opened row. Returns whether there was one.
    pub fn close_stream_open(&mut self) -> bool {
        self.stream.open.take().is_some()
    }

    /// `j` / `k` inside an opened row. Returns whether it took the key.
    pub(super) fn scroll_stream_open(&mut self, down: bool) -> bool {
        match &mut self.stream.open {
            Some(open) if self.focus == Focus::Value => {
                open.view.scroll(down, 1);
                true
            }
            _ => false,
        }
    }

    /// Arrows inside an opened row. Returns whether it took the key.
    pub(super) fn pan_stream_open(&mut self, right: bool) -> bool {
        match &mut self.stream.open {
            Some(open) => {
                open.view.pan(right);
                true
            }
            None => false,
        }
    }

    /// `w` inside an opened row. Returns whether it took the key.
    pub(super) fn wrap_stream_open(&mut self) -> bool {
        match &mut self.stream.open {
            Some(open) => {
                open.view.toggle_wrap();
                true
            }
            None => false,
        }
    }

    /// What `y` copies: the entry as JSON, or the group's name.
    pub(super) fn stream_row_text(&self) -> Option<String> {
        let stream = self.stream_value()?;
        let row = self.value_table.selected()?;
        match self.stream.tab {
            StreamTab::Entries => {
                serde_json::to_string_pretty(&stream.entries.get(row)?.to_json()).ok()
            }
            StreamTab::Groups => Some(stream.groups.as_ref()?.get(row)?.name.clone()),
        }
    }

    /// Draw the stream in `area`; `footer` goes under the entries.
    pub(super) fn render_stream(&mut self, footer: Option<String>, area: Rect, buf: &mut Buffer) {
        let Some(KeyValue::Stream(stream)) =
            self.selected_value.as_ref().map(|loaded| &loaded.value)
        else {
            return;
        };
        let [tabs_area, body] =
            Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(area);
        tabs_line(stream, self.stream.tab).render(tabs_area, buf);

        if let Some(open) = &mut self.stream.open {
            text_view::render(
                &open.text,
                &TextLabels {
                    header: &open.header,
                    paging: false,
                    close: Some("Esc back"),
                    changed: &[],
                    highlight: Style::new(),
                },
                None,
                true,
                body,
                buf,
                &mut open.view,
            );
            return;
        }
        match self.stream.tab {
            StreamTab::Entries => {
                let body = render_value_footer(footer, body, buf);
                self.panes.value_rows = below_header(body);
                render_entries(
                    &stream.entries,
                    &self.live.changed,
                    body,
                    buf,
                    &mut self.value_table,
                );
            }
            StreamTab::Groups => {
                self.panes.value_rows = below_header(body);
                render_groups(stream.groups.as_deref(), body, buf, &mut self.value_table);
            }
        }
    }
}

/// `[Entries 10,000]  Groups 2  last 1700000000000-0`.
fn tabs_line(stream: &StreamValue, tab: StreamTab) -> Paragraph<'static> {
    let colors = &config::get().colors;
    let on = Style::new().fg(colors.base0a).bold();
    let off = Style::new().fg(colors.base03);
    let entries = group_thousands(stream.length);
    let groups = stream
        .groups
        .as_ref()
        .map_or_else(|| "?".to_owned(), |g| group_thousands(g.len()));
    let (entries, groups) = match tab {
        StreamTab::Entries => (
            Span::styled(format!("[Entries {entries}]"), on),
            Span::styled(format!("Groups {groups}"), off),
        ),
        StreamTab::Groups => (
            Span::styled(format!("Entries {entries}"), off),
            Span::styled(format!("[Groups {groups}]"), on),
        ),
    };
    let mut spans = vec![entries, Span::raw("  "), groups];
    if !stream.last_id.is_empty() {
        spans.push(Span::styled(
            format!("  last {}", printable(&stream.last_id)),
            off,
        ));
    }
    Paragraph::new(Line::from(spans))
}

/// The ID's millisecond part as a time of day.
fn entry_time(entry: &StreamEntry) -> String {
    entry.millis().map_or_else(
        || "—".to_owned(),
        |ms| hms(UNIX_EPOCH + Duration::from_millis(ms)),
    )
}

/// `a=1 b=2 c=3 …`.
fn fields_preview(entry: &StreamEntry) -> String {
    let mut out = entry
        .fields
        .iter()
        .take(PREVIEW_FIELDS)
        .map(|(f, v)| format!("{}={}", printable(f), printable(v)))
        .collect::<Vec<_>>()
        .join(" ");
    if entry.fields.len() > PREVIEW_FIELDS {
        out.push_str(" …");
    }
    out
}

fn render_entries(
    entries: &[StreamEntry],
    changed: &[usize],
    area: Rect,
    buf: &mut Buffer,
    table: &mut TableState,
) {
    let colors = &config::get().colors;
    if entries.is_empty() {
        Paragraph::new(Line::from("The stream is empty.").fg(colors.base03)).render(area, buf);
        return;
    }
    let with_time = area.width >= TIME_MIN_WIDTH;
    let id_width = entries.iter().map(|e| e.id.width()).max().unwrap_or(2);
    let id_width = u16::try_from(id_width).unwrap_or(u16::MAX);
    let mut widths = vec![Constraint::Length(id_width)];
    let mut labels = vec!["ID"];
    if with_time {
        widths.push(Constraint::Length(8));
        labels.push("Time");
    }
    widths.extend([Constraint::Length(3), Constraint::Fill(1)]);
    labels.extend(["#", "Fields"]);
    let header = Row::new(labels).bottom_margin(1).fg(colors.base03).bold();
    let rows = entries.iter().enumerate().map(|(i, entry)| {
        let mut cells = vec![Cell::from(printable(&entry.id).into_owned()).fg(colors.base05)];
        if with_time {
            cells.push(Cell::from(entry_time(entry)).fg(colors.base04));
        }
        cells.push(Cell::from(entry.fields.len().to_string()).fg(colors.base03));
        cells.push(Cell::from(fields_preview(entry)).fg(colors.base04));
        let row = Row::new(cells);
        if changed.binary_search(&i).is_ok() {
            row.style(live::changed_style())
        } else {
            row
        }
    });
    let widget = Table::new(rows, widths)
        .header(header)
        .column_spacing(1)
        .highlight_symbol(highlight_symbol())
        .row_highlight_style(selected_row_style())
        .highlight_spacing(HighlightSpacing::Always);
    StatefulWidget::render(widget, area, buf, table);
    render_header_rule(area, buf);
}

fn render_groups(
    groups: Option<&[GroupInfo]>,
    area: Rect,
    buf: &mut Buffer,
    table: &mut TableState,
) {
    let colors = &config::get().colors;
    let Some(groups) = groups.filter(|g| !g.is_empty()) else {
        let text = if groups.is_some() {
            "No consumer groups."
        } else {
            "XINFO GROUPS was refused."
        };
        Paragraph::new(Line::from(text).fg(colors.base03)).render(area, buf);
        return;
    };
    let widths = [
        Constraint::Fill(1),
        Constraint::Length(4),
        Constraint::Length(6),
        Constraint::Length(6),
        Constraint::Fill(1),
    ];
    let header = Row::new(["Group", "Cons", "Pend", "Lag", "Last delivered"])
        .bottom_margin(1)
        .fg(colors.base03)
        .bold();
    let rows = groups.iter().map(|group| {
        let lag = group
            .lag
            .map_or_else(|| "—".to_owned(), |lag| lag.to_string());
        Row::new([
            Cell::from(printable(&group.name).into_owned()).fg(colors.base05),
            Cell::from(group.consumers.to_string()).fg(colors.base04),
            Cell::from(group.pending.to_string()).fg(colors.base0b),
            Cell::from(lag).fg(colors.base04),
            Cell::from(printable(&group.last_delivered).into_owned()).fg(colors.base03),
        ])
    });
    let widget = Table::new(rows, widths)
        .header(header)
        .column_spacing(1)
        .highlight_symbol(highlight_symbol())
        .row_highlight_style(selected_row_style())
        .highlight_spacing(HighlightSpacing::Always);
    StatefulWidget::render(widget, area, buf, table);
    render_header_rule(area, buf);
}

/// An opened entry: field and value in two columns; a value that holds JSON
/// goes under its field, pretty-printed.
fn entry_lines(entry: &StreamEntry) -> Vec<Line<'static>> {
    let colors = &config::get().colors;
    let mut out = vec![
        Line::from(format!("Time  {}", entry_time(entry))).fg(colors.base03),
        Line::default(),
    ];
    let column = entry
        .fields
        .iter()
        .map(|(f, _)| printable(f).width())
        .max()
        .unwrap_or(0)
        .min(FIELD_COLUMN);
    for (field, value) in &entry.fields {
        let field = printable(field);
        let json = TextCache::from_text(value, true);
        if json.json {
            out.push(Line::from(field.into_owned()).fg(colors.base0d));
            out.extend(json.lines.into_iter().map(|mut line| {
                line.spans.insert(0, Span::raw("  "));
                line
            }));
        } else {
            let pad = column.saturating_sub(field.width());
            out.push(Line::from(vec![
                Span::from(format!("{field}{}  ", " ".repeat(pad))).fg(colors.base0d),
                Span::from(printable(value).into_owned()).fg(colors.base05),
            ]));
        }
    }
    out
}

/// An opened group: its consumers, then its first pending entries.
fn group_lines(detail: &GroupDetail) -> Vec<String> {
    let idle = |ms: u64| fmt_micros(ms.saturating_mul(1000));
    let mut out = Vec::new();
    if let Some(error) = &detail.error {
        out.push(format!("Refused: {}", printable(error)));
        out.push(String::new());
    }
    out.push(format!("Consumers ({})", detail.consumers.len()));
    let width = detail
        .consumers
        .iter()
        .map(|c| printable(&c.name).width())
        .chain(
            detail
                .pending
                .iter()
                .map(|p| printable(&p.consumer).width()),
        )
        .max()
        .unwrap_or(8)
        .max(8);
    out.push(format!(
        "{:<width$}  {:>7}  {:>8}",
        "NAME", "PENDING", "IDLE"
    ));
    out.extend(detail.consumers.iter().map(|c| {
        format!(
            "{:<width$}  {:>7}  {:>8}",
            printable(&c.name),
            c.pending,
            idle(c.idle_ms)
        )
    }));
    out.push(String::new());
    out.push(format!("Pending (first {})", detail.pending.len()));
    let id_width = detail
        .pending
        .iter()
        .map(|p| printable(&p.id).width())
        .max()
        .unwrap_or(2)
        .max(2);
    out.push(format!(
        "{:<id_width$}  {:<width$}  {:>8}  {:>5}",
        "ID", "CONSUMER", "IDLE", "DELIV"
    ));
    out.extend(detail.pending.iter().map(|p| {
        format!(
            "{:<id_width$}  {:<width$}  {:>8}  {:>5}",
            printable(&p.id),
            printable(&p.consumer),
            idle(p.idle_ms),
            p.deliveries
        )
    }));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redis_client::types::{
        ConsumerInfo, KeyMeta, LoadedValue, PendingEntry, RedisType, ValuePageInfo,
    };

    fn entry(id: &str, fields: &[(&str, &str)]) -> StreamEntry {
        StreamEntry {
            id: id.into(),
            fields: fields
                .iter()
                .map(|&(f, v)| (f.to_owned(), v.to_owned()))
                .collect(),
        }
    }

    fn group(name: &str) -> GroupInfo {
        GroupInfo {
            name: name.into(),
            consumers: 1,
            pending: 2,
            last_delivered: "2-0".into(),
            lag: None,
        }
    }

    fn pane(groups: Option<Vec<GroupInfo>>) -> KeySpace {
        let mut k = KeySpace::new(Vec::new());
        k.set_keys(vec![KeyMeta {
            key: "events".into(),
            r_type: RedisType::Stream,
            size: 10,
            ttl: -1,
            value: KeyValue::Unknown,
        }]);
        assert!(k.select_key_by_name("events"));
        k.set_selected_value(Some(LoadedValue {
            value: KeyValue::Stream(StreamValue {
                entries: vec![entry("2-0", &[("a", "1")]), entry("1-0", &[("b", "2")])],
                length: 2,
                last_id: "2-0".into(),
                groups,
            }),
            page: ValuePageInfo::default(),
        }));
        k
    }

    #[test]
    fn tab_switches_the_table_and_takes_the_cursor() {
        let mut k = pane(Some(vec![group("g1")]));
        assert!(k.toggle_stream_tab());
        assert!(k.is_stream_focused());
        assert_eq!(k.value_row_count(), 1);
        assert!(k.stream_blocks_paging());
        assert!(k.toggle_stream_tab());
        assert_eq!(k.value_row_count(), 2);
        assert!(!k.stream_blocks_paging());
    }

    #[test]
    fn tab_on_a_stream_without_groups_leaves_the_table() {
        let mut k = pane(Some(Vec::new()));
        k.enter_value();
        assert!(k.toggle_stream_tab());
        assert!(!k.is_stream_focused());
        assert!(k.toggle_stream_tab(), "and comes back");
    }

    #[test]
    fn tab_is_for_streams_only() {
        let mut k = KeySpace::new(Vec::new());
        assert!(!k.toggle_stream_tab());
    }

    #[test]
    fn enter_opens_an_entry_and_esc_closes_it() {
        let mut k = pane(None);
        k.enter_value();
        k.scroll_next();
        assert_eq!(k.open_stream_row(), None);
        assert!(k.is_stream_open());
        assert!(k.close_stream_open());
        assert!(!k.close_stream_open());
    }

    #[test]
    fn enter_on_a_group_asks_for_it_and_only_its_data_lands() {
        let mut k = pane(Some(vec![group("g1")]));
        k.toggle_stream_tab();
        assert_eq!(
            k.open_stream_row(),
            Some(("events".to_owned(), "g1".to_owned()))
        );
        let mut detail = GroupDetail {
            key: "events".into(),
            group: "other".into(),
            ..GroupDetail::default()
        };
        k.set_stream_group(&detail);
        let shown = |k: &KeySpace| k.stream.open.as_ref().unwrap().text.lines[0].to_string();
        assert_eq!(shown(&k), "Loading…", "another group's reply");
        detail.group = "g1".into();
        detail.consumers.push(ConsumerInfo {
            name: "w".into(),
            pending: 1,
            idle_ms: 5,
        });
        detail.pending.push(PendingEntry {
            id: "1-0".into(),
            consumer: "w".into(),
            idle_ms: 5,
            deliveries: 2,
        });
        k.set_stream_group(&detail);
        assert_eq!(shown(&k), "Consumers (1)");
    }

    #[test]
    fn yank_copies_the_entry_as_json() {
        let mut k = pane(None);
        k.enter_value();
        let text = k.selected_element_text().expect("text");
        let json: serde_json::Value = serde_json::from_str(&text).expect("JSON");
        assert_eq!(json["id"], "2-0");
        assert_eq!(json["fields"]["a"], "1");
    }

    #[test]
    fn a_json_value_is_pretty_printed_under_its_field() {
        let lines = entry_lines(&entry("1-0", &[("doc", r#"{"x":1}"#), ("n", "2")]));
        let text: Vec<String> = lines.iter().map(ToString::to_string).collect();
        assert!(text.contains(&"doc".to_owned()), "{text:?}");
        assert!(text.iter().any(|l| l.contains(r#""x": 1"#)), "{text:?}");
        assert!(text.iter().any(|l| l.starts_with('n') && l.ends_with('2')));
    }

    #[test]
    fn escapes_from_the_server_never_reach_the_terminal() {
        let e = entry("1-0", &[("a\u{1b}[31m", "b\u{1b}]52;")]);
        assert!(!fields_preview(&e).contains('\u{1b}'));
        let text: String = entry_lines(&e).iter().map(ToString::to_string).collect();
        assert!(!text.contains('\u{1b}'));
    }
}
