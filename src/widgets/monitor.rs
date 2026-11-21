//! The MONITOR screen: a live log of the commands the server runs.

use std::{
    collections::VecDeque,
    fmt::Write as _,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use parking_lot::Mutex;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, StatefulWidget, Widget, Wrap},
};
use ratatui_textarea::TextArea;
use unicode_width::UnicodeWidthStr;

use crate::{
    config,
    redis_client::{
        bytes::bytes_from_text,
        escape::{printable, quote_arg},
        monitor_store::{MonitorLine, MonitorStatus, MonitorStore, ParsedLine},
        types::glob_match,
    },
    utils::group_thousands,
    widgets::{
        confirm::typed_input_titled,
        local_time::hms,
        pubsub::{log_state, LogBuffer, LogCursor},
        text_view::{self, TextCache, TextLabels, TextView},
    },
};

/// Seconds of history in the title's sparkline.
const SPARK_SECS: usize = 8;
const SPARK_BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
/// Width of the client column.
const CLIENT_WIDTH: usize = 21;
/// Below this width the client column is left out.
const CLIENT_MIN_AREA: usize = 60;

/// The store's lines that pass the filter. Entries are numbered without gaps
/// (`first` + index), so [`LogCursor`] counts `+N new` in matching lines.
#[derive(Debug)]
struct MatchLog {
    seqs: VecDeque<u64>,
    /// Number of the entry at the front.
    first: u64,
}

impl Default for MatchLog {
    fn default() -> Self {
        Self {
            seqs: VecDeque::new(),
            first: 1,
        }
    }
}

impl MatchLog {
    fn push(&mut self, seq: u64) {
        self.seqs.push_back(seq);
    }

    /// Forget the entries the store evicted.
    fn drop_before(&mut self, seq: u64) {
        while self.seqs.front().is_some_and(|&front| front < seq) {
            self.seqs.pop_front();
            self.first += 1;
        }
    }

    /// Forget everything; numbering goes on.
    fn clear(&mut self) {
        self.first += self.seqs.len() as u64;
        self.seqs.clear();
    }

    fn store_seq(&self, index: usize) -> Option<u64> {
        self.seqs.get(index).copied()
    }
}

impl LogBuffer for MatchLog {
    fn len(&self) -> usize {
        self.seqs.len()
    }

    fn first_seq(&self) -> Option<u64> {
        (!self.seqs.is_empty()).then_some(self.first)
    }

    fn last_seq(&self) -> Option<u64> {
        (!self.seqs.is_empty()).then(|| self.first + self.seqs.len() as u64 - 1)
    }

    fn seq_at(&self, index: usize) -> Option<u64> {
        (index < self.seqs.len()).then(|| self.first + index as u64)
    }

    fn index_of(&self, seq: u64) -> Result<usize, usize> {
        if seq < self.first {
            return Err(0);
        }
        let index = usize::try_from(seq - self.first).unwrap_or(usize::MAX);
        if index < self.seqs.len() {
            Ok(index)
        } else {
            Err(self.seqs.len())
        }
    }
}

/// A filter: a glob over the command (any case) or any argument.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LineFilter {
    text: String,
    upper: String,
}

impl LineFilter {
    fn new(text: &str) -> Option<Self> {
        let text = text.trim();
        (!text.is_empty()).then(|| Self {
            text: text.to_owned(),
            upper: text.to_ascii_uppercase(),
        })
    }

    fn matches(&self, line: &ParsedLine) -> bool {
        glob_match(&self.upper, &line.cmd.to_ascii_uppercase())
            || line.args.iter().any(|arg| glob_match(&self.text, arg))
    }
}

/// One line opened with Enter.
struct LineDetail {
    text: TextCache,
    view: TextView,
    note: Option<String>,
}

/// State of the MONITOR screen.
pub struct Monitor {
    store: Arc<Mutex<MonitorStore>>,
    log: MatchLog,
    cursor: LogCursor,
    /// Newest store line already checked against the filter.
    synced: u64,
    filter: Option<LineFilter>,
    filter_input: Option<TextArea<'static>>,
    detail: Option<LineDetail>,
}

impl Monitor {
    #[must_use]
    pub fn new(store: Arc<Mutex<MonitorStore>>) -> Self {
        Self {
            store,
            log: MatchLog::default(),
            cursor: LogCursor::default(),
            synced: 0,
            filter: None,
            filter_input: None,
            detail: None,
        }
    }

    /// Bring the match log up to date: only lines new since the last frame
    /// are checked, so a frame costs the same however full the store is.
    fn sync_log(&mut self, store: &MonitorStore) {
        match store.first_seq() {
            Some(first) => self.log.drop_before(first),
            None => self.log.clear(),
        }
        for line in store.since(self.synced) {
            if self.filter.as_ref().is_none_or(|f| f.matches(&line.line)) {
                self.log.push(line.seq);
            }
        }
        self.synced = store.last_seq().unwrap_or(self.synced);
    }

    /// Copy what the frame draws, under one short lock.
    pub fn build_view(&mut self, area: Rect, wall: SystemTime) -> MonitorView {
        let width = usize::from(area.width.saturating_sub(2));
        let height = usize::from(area.height.saturating_sub(2));
        let sec = wall.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let store = Arc::clone(&self.store);
        let store = store.lock();
        self.sync_log(&store);
        let (top, selected) = self.cursor.sync(&self.log, height);
        let paused = self.cursor.is_paused();
        // A paused log doesn't grow into the free rows.
        let end = self.cursor.visible_end(&self.log).min(top + height);
        let lines: Vec<Line<'static>> = (top..end)
            .filter_map(|i| {
                let line = store.get(self.log.store_seq(i)?)?;
                Some(row_line(line, paused && selected == Some(i), width))
            })
            .collect();
        let state = match store.status() {
            MonitorStatus::Running => log_state(&self.cursor, &self.log),
            MonitorStatus::Connecting => "connecting".to_owned(),
            MonitorStatus::Reconnecting { .. } => "reconnecting".to_owned(),
            MonitorStatus::Stopped | MonitorStatus::Failed(_) => "stopped".to_owned(),
        };
        let title = title_text(
            &state,
            store.sample_every(),
            &store.rates(sec),
            self.filter.as_ref().map(|f| f.text.as_str()),
            width,
        );
        let placeholder = placeholder(store.status(), self.filter.is_some());
        MonitorView {
            title,
            lines,
            placeholder,
        }
    }

    pub fn is_paused(&self) -> bool {
        self.cursor.is_paused()
    }

    pub fn toggle_pause(&mut self) {
        if self.detail.is_none() {
            self.cursor.toggle_pause(&self.log);
        }
    }

    /// `j` / `k`: the detail's text, or the log's selection.
    pub fn move_selection(&mut self, down: bool) {
        if let Some(detail) = &mut self.detail {
            detail.view.scroll(down, 1);
        } else if down {
            self.cursor.down(&self.log);
        } else {
            self.cursor.up(&self.log);
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

    /// `c`: drop the lines. Numbering goes on.
    pub fn clear(&mut self) {
        self.store.lock().clear();
        self.log.clear();
        self.cursor.follow();
        self.detail = None;
    }

    /// Enter: the selected line, with its arguments in full.
    pub fn open_detail(&mut self) {
        let Some(number) = self.cursor.selected_seq(&self.log) else {
            return;
        };
        let Ok(index) = self.log.index_of(number) else {
            return;
        };
        let Some(seq) = self.log.store_seq(index) else {
            return;
        };
        let line = self.store.lock().get(seq).map(|l| l.line.clone());
        if let Some(line) = line {
            self.detail = Some(LineDetail {
                text: TextCache::from_lines(detail_lines(&line)),
                view: TextView::default(),
                note: (line.truncated || line.more_args > 0).then(|| "cut by rrover".to_owned()),
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

    pub fn open_filter(&mut self) {
        let mut input = typed_input_titled("Filter: command or key glob", "SET or user:*");
        if let Some(filter) = &self.filter {
            input.insert_str(&filter.text);
        }
        self.filter_input = Some(input);
    }

    #[must_use]
    pub fn is_filter_open(&self) -> bool {
        self.filter_input.is_some()
    }

    pub fn close_filter(&mut self) {
        self.filter_input = None;
    }

    pub fn filter_key(&mut self, key: crossterm::event::KeyEvent) {
        if let Some(input) = &mut self.filter_input {
            input.input(key);
        }
    }

    /// Enter in the filter: check the whole buffer against it on the next
    /// frame, and follow the newest match.
    pub fn apply_filter(&mut self) {
        let Some(input) = self.filter_input.take() else {
            return;
        };
        self.filter = LineFilter::new(&input.lines().join(" "));
        self.log.clear();
        self.synced = 0;
        self.cursor.follow();
    }
}

/// What one frame of the MONITOR screen draws.
#[derive(Debug, Default)]
pub struct MonitorView {
    title: String,
    lines: Vec<Line<'static>>,
    /// Shown when there are no lines.
    placeholder: String,
}

pub struct MonitorWidget {
    pub view: MonitorView,
}

impl StatefulWidget for MonitorWidget {
    type State = Monitor;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Monitor) {
        let colors = &config::get().colors;
        let block = Block::default()
            .title(self.view.title)
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
                detail.note.clone(),
                true,
                inner,
                buf,
                &mut detail.view,
            );
        } else if self.view.lines.is_empty() {
            Paragraph::new(Line::from(self.view.placeholder).fg(colors.base03))
                .wrap(Wrap { trim: false })
                .render(inner, buf);
        } else {
            Paragraph::new(self.view.lines).render(inner, buf);
        }

        if let Some(input) = &state.filter_input {
            let [_, row] =
                Layout::vertical([Constraint::Fill(1), Constraint::Length(3)]).areas(inner);
            Clear.render(row, buf);
            input.render(row, buf);
        }
    }
}

/// `14:22:07.123 0  127.0.0.1:60866  "SET" "a" "1"`.
fn row_line(line: &MonitorLine, selected: bool, width: usize) -> Line<'static> {
    let colors = &config::get().colors;
    let parsed = &line.line;
    let at = UNIX_EPOCH + Duration::from_micros(parsed.ts_micros);
    let millis = parsed.ts_micros / 1000 % 1000;
    let mut spans = vec![
        Span::from(format!("{}.{millis:03} ", hms(at))).fg(colors.base03),
        Span::from(format!("{:<2} ", parsed.db)).fg(colors.base04),
    ];
    if width >= CLIENT_MIN_AREA {
        spans.push(Span::from(format!("{} ", client_cell(&parsed.client))).fg(colors.base03));
    }
    let used: usize = spans.iter().map(Span::width).sum();
    spans.push(Span::from(quote_arg(&bytes_from_text(&parsed.cmd))).fg(colors.base0b));
    let mut text = String::new();
    let mut room = width.saturating_sub(used + spans[spans.len() - 1].width());
    for arg in &parsed.args {
        if room == 0 {
            break;
        }
        let quoted = quote_arg(&bytes_from_text(arg));
        room = room.saturating_sub(quoted.width() + 1);
        text.push(' ');
        text.push_str(&quoted);
    }
    if parsed.more_args > 0 && room > 0 {
        let _ = write!(text, " …(+{} more)", parsed.more_args);
    }
    spans.push(Span::from(text).fg(colors.base05));
    let row = Line::from(spans);
    if selected {
        row.style(Style::new().bg(colors.base02))
    } else {
        row
    }
}

/// The client, padded or shortened to its column.
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

/// The detail's text: where the command came from, then one row per argument.
fn detail_lines(line: &ParsedLine) -> Vec<String> {
    let at = UNIX_EPOCH + Duration::from_micros(line.ts_micros);
    let mut out = vec![
        format!(
            "{}.{:06}  db {}  {}",
            hms(at),
            line.ts_micros % 1_000_000,
            line.db,
            printable(&line.client)
        ),
        String::new(),
        printable(&line.cmd).into_owned(),
    ];
    out.extend(
        line.args
            .iter()
            .enumerate()
            .map(|(i, arg)| format!("[{}] {}", i + 1, printable(arg))),
    );
    if line.more_args > 0 {
        out.push(format!(
            "…and {} more arguments",
            group_thousands(line.more_args)
        ));
    }
    out
}

/// ` Monitor · following · sampling 1/7 · 5.0k/s ▂▃▅ · filter: x `, leaving
/// out what doesn't fit, the least important last.
fn title_text(
    state: &str,
    sample_every: u32,
    rates: &[u64],
    filter: Option<&str>,
    width: usize,
) -> String {
    let room = width.saturating_sub(2);
    let mut title = format!(" Monitor · {state}");
    let mut add = |part: String| {
        if title.width() + part.width() + 3 < room {
            title.push_str(" · ");
            title.push_str(&part);
        }
    };
    if sample_every > 1 {
        add(format!("sampling 1/{sample_every}"));
    }
    if let Some(part) = rate_part(rates) {
        add(part);
    }
    if let Some(filter) = filter {
        add(format!("filter: {}", printable(filter)));
    }
    title.push(' ');
    title
}

/// `5.0k/s ▂▃▅`, from the seconds that are over. `None` before any traffic.
fn rate_part(rates: &[u64]) -> Option<String> {
    let done = &rates[..rates.len().saturating_sub(1)];
    let recent = &done[done.len().saturating_sub(SPARK_SECS)..];
    let peak = recent.iter().copied().max().filter(|&p| p > 0)?;
    let bars: String = recent
        .iter()
        .map(|&rate| {
            let top = SPARK_BARS.len() as u64 - 1;
            let level = (rate * top).div_ceil(peak).min(top);
            SPARK_BARS[usize::try_from(level).unwrap_or(0)]
        })
        .collect();
    Some(format!(
        "{} {bars}",
        fmt_rate(recent.last().copied().unwrap_or(0))
    ))
}

#[allow(clippy::cast_precision_loss)] // A rate shown to one decimal.
fn fmt_rate(rate: u64) -> String {
    if rate < 1000 {
        format!("{rate}/s")
    } else {
        format!("{:.1}k/s", rate as f64 / 1000.0)
    }
}

fn placeholder(status: &MonitorStatus, filtered: bool) -> String {
    match status {
        MonitorStatus::Stopped => "MONITOR is stopped. t starts it (it costs server CPU).".into(),
        MonitorStatus::Connecting => "Connecting…".into(),
        MonitorStatus::Running if filtered => "No line matches the filter.".into(),
        MonitorStatus::Running => "Waiting for commands…".into(),
        MonitorStatus::Reconnecting { attempt, .. } => {
            format!("Connection lost, reconnecting (attempt {attempt})…")
        }
        MonitorStatus::Failed(reason) => format!("MONITOR failed: {}", printable(reason)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redis_client::monitor_store::{parse_line, MonitorLimits};

    fn store(lines: usize) -> Arc<Mutex<MonitorStore>> {
        Arc::new(Mutex::new(MonitorStore::new(MonitorLimits {
            lines,
            bytes: usize::MAX,
        })))
    }

    fn push(store: &Arc<Mutex<MonitorStore>>, raw: &[&str]) {
        let lines: Vec<_> = raw
            .iter()
            .map(|r| parse_line(&format!("1.000000 [0 1.2.3.4:5] {r}")).unwrap())
            .collect();
        let seen = lines.len() as u64;
        store.lock().push_batch(lines, seen, 1, 1);
    }

    fn shown(monitor: &mut Monitor) -> Vec<String> {
        let view = monitor.build_view(Rect::new(0, 0, 100, 12), UNIX_EPOCH);
        view.lines.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn filter_matches_command_in_any_case_or_any_argument() {
        let f = LineFilter::new(" set ").unwrap();
        let line = |raw: &str| parse_line(&format!("1.0 [0 lua] {raw}")).unwrap();
        assert!(f.matches(&line(r#""SET" "a" "1""#)));
        assert!(!f.matches(&line(r#""GET" "a""#)));
        let f = LineFilter::new("user:*").unwrap();
        assert!(f.matches(&line(r#""DEL" "x" "user:1""#)));
        assert!(!f.matches(&line(r#""DEL" "x""#)));
        assert_eq!(LineFilter::new("   "), None);
    }

    #[test]
    fn a_new_filter_rechecks_the_buffer_and_new_lines_follow_it() {
        let s = store(100);
        push(&s, &[r#""SET" "a" "1""#, r#""GET" "a""#]);
        let mut m = Monitor::new(s.clone());
        assert_eq!(shown(&mut m).len(), 2);

        m.open_filter();
        for c in "get".chars() {
            m.filter_key(crossterm::event::KeyEvent::from(
                crossterm::event::KeyCode::Char(c),
            ));
        }
        m.apply_filter();
        let rows = shown(&mut m);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].contains(r#""GET" "a""#), "{rows:?}");

        push(&s, &[r#""SET" "b" "2""#, r#""get" "b""#]);
        let rows = shown(&mut m);
        assert_eq!(rows.len(), 2);
        assert!(rows[1].contains(r#""get" "b""#), "{rows:?}");
    }

    #[test]
    fn paused_counts_only_matching_lines() {
        let s = store(100);
        push(&s, &[r#""GET" "a""#]);
        let mut m = Monitor::new(s.clone());
        m.open_filter();
        m.filter_key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Char('g'),
        ));
        m.filter_key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Char('*'),
        ));
        m.apply_filter();
        shown(&mut m);
        m.toggle_pause();
        push(&s, &[r#""SET" "x" "1""#, r#""GET" "b""#, r#""GET" "c""#]);
        let view = m.build_view(Rect::new(0, 0, 100, 12), UNIX_EPOCH);
        assert_eq!(view.lines.len(), 1, "the view is frozen");
        assert_eq!(m.cursor.new_count(&m.log), 2);
    }

    #[test]
    fn evicted_lines_leave_the_match_log() {
        let s = store(3);
        push(&s, &[r#""A""#, r#""B""#, r#""C""#]);
        let mut m = Monitor::new(s.clone());
        assert_eq!(shown(&mut m).len(), 3);
        push(&s, &[r#""D""#, r#""E""#]);
        let rows = shown(&mut m);
        assert_eq!(rows.len(), 3);
        assert!(rows[0].contains(r#""C""#), "{rows:?}");
        assert_eq!(m.log.len(), 3);
    }

    #[test]
    fn clear_empties_the_view_and_new_lines_still_show() {
        let s = store(10);
        push(&s, &[r#""A""#]);
        let mut m = Monitor::new(s.clone());
        shown(&mut m);
        m.toggle_pause();
        m.clear();
        assert!(shown(&mut m).is_empty());
        assert!(!m.is_paused());
        push(&s, &[r#""B""#]);
        assert_eq!(shown(&mut m).len(), 1);
    }

    #[test]
    fn escape_sequences_from_the_server_never_reach_the_terminal() {
        let s = store(10);
        push(&s, &[r#""SET" "\x1b[31m" "1""#]);
        let mut m = Monitor::new(s);
        let rows = shown(&mut m);
        assert!(!rows[0].contains('\u{1b}'), "{rows:?}");
        assert!(rows[0].contains(r"\x1b"), "{rows:?}");
    }

    #[test]
    fn the_detail_shows_every_stored_argument() {
        let s = store(10);
        push(&s, &[r#""SET" "a" "line\nbreak""#]);
        let mut m = Monitor::new(s);
        shown(&mut m);
        m.open_detail();
        assert!(m.is_detail());
        let text: Vec<String> = m
            .detail
            .as_ref()
            .unwrap()
            .text
            .lines
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(text[2], "SET");
        assert_eq!(text[3], "[1] a");
        assert_eq!(text[4], r"[2] line\nbreak");
    }

    #[test]
    fn the_title_drops_what_does_not_fit() {
        let mut rates = vec![0; 60];
        rates[58] = 5000;
        let wide = title_text("following", 7, &rates, Some("user:*"), 100);
        assert!(wide.contains("sampling 1/7"), "{wide}");
        assert!(wide.contains("5.0k/s"), "{wide}");
        assert!(wide.contains("filter: user:*"), "{wide}");
        let narrow = title_text("following", 7, &rates, Some("user:*"), 40);
        assert!(narrow.contains("sampling 1/7"), "{narrow}");
        assert!(!narrow.contains("filter"), "{narrow}");
        assert!(narrow.width() <= 40, "{narrow}");
    }
}
