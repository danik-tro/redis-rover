//! The Pub/Sub tab: own subscriptions and the server's channels on the left,
//! the selected subscription's live log on the right, and the add / publish
//! popups.

use std::{
    fmt::Write as _,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use parking_lot::Mutex;
use ratatui::{
    buffer::Buffer,
    crossterm::event::{KeyCode, KeyEvent},
    layout::{Alignment, Constraint, Flex, Layout, Rect},
    style::{Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, StatefulWidget, Widget, Wrap},
};
use ratatui_textarea::{CursorMove, TextArea};
use unicode_width::UnicodeWidthStr;

use crate::{
    config,
    redis_client::pubsub_store::{
        detect_kind, ConnStatus, EntryKind, Payload, PubSubMessage, PubSubStore, Target, TargetKind,
    },
    widgets::keyspace::{format_bytes, truncate_to_width},
};

/// Bytes shown in a binary payload's hex preview.
const HEX_PREVIEW: usize = 16;
/// Width of the subscriptions pane.
const LIST_WIDTH: u16 = 32;
/// Log row standing for a reconnect.
const RECONNECT_MARKER: &str = "— reconnected, messages may be missing —";
/// Header note in cluster mode.
const CLUSTER_NOTE: &str = "⚠ PUBSUB CHANNELS: this node only";
/// Publish popup notice under `--read-only`.
const READ_ONLY_PUBLISH: &str = "Read-only mode: publishing is disabled";

/// A selectable row of the left pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// Index into `PubSubStore::subs`.
    Sub(usize),
    /// A server channel we aren't subscribed to.
    Server(String),
}

/// Number of selectable rows: subscriptions, then server-only channels.
fn row_count(store: &PubSubStore) -> usize {
    store.subs.len() + store.server_only_channels().count()
}

fn row_at(store: &PubSubStore, index: usize) -> Option<Row> {
    if index < store.subs.len() {
        return Some(Row::Sub(index));
    }
    store
        .server_only_channels()
        .nth(index - store.subs.len())
        .map(|(name, _)| Row::Server(name.clone()))
}

/// Channel the publish popup starts with: the selected exact subscription
/// or server channel; for a pattern, the concrete channel of its newest
/// message; otherwise empty.
pub fn publish_prefill(store: &PubSubStore, row: Option<&Row>) -> String {
    match row {
        Some(Row::Server(name)) => name.clone(),
        Some(Row::Sub(i)) => match store.subs.get(*i) {
            Some(sub) => match &sub.target {
                Target::Channel(name) => name.clone(),
                Target::Pattern(_) => sub
                    .buffer
                    .last_n(usize::MAX)
                    .rev()
                    .find(|m| m.kind == EntryKind::Message)
                    .map(|m| m.channel.clone())
                    .unwrap_or_default(),
            },
            None => String::new(),
        },
        None => String::new(),
    }
}

/// State of the add-subscription popup.
struct AddPopup {
    input: TextArea<'static>,
    /// Set once the user flips the kind with Tab; otherwise the kind follows
    /// the text (glob characters mean pattern).
    kind_override: Option<TargetKind>,
}

impl AddPopup {
    fn new() -> Self {
        let cfg = config::get();
        let mut input = TextArea::default();
        input.set_placeholder_text("channel or pattern, e.g. orders:*");
        input.set_block(
            Block::default()
                .border_style(cfg.colors.base04)
                .border_type(BorderType::Rounded)
                .borders(Borders::ALL)
                .title(" Subscribe "),
        );
        Self {
            input,
            kind_override: None,
        }
    }

    fn text(&self) -> String {
        self.input.lines().first().cloned().unwrap_or_default()
    }

    fn kind(&self) -> TargetKind {
        self.kind_override
            .unwrap_or_else(|| detect_kind(&self.text()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublishField {
    Channel,
    Payload,
}

/// State of the publish popup.
struct PublishPopup {
    channel: TextArea<'static>,
    payload: TextArea<'static>,
    focus: PublishField,
    /// `--read-only`: shown with a notice, sending is refused.
    read_only: bool,
}

impl PublishPopup {
    fn new(prefill: String, read_only: bool) -> Self {
        let focus = if prefill.is_empty() {
            PublishField::Channel
        } else {
            PublishField::Payload
        };
        let mut channel = TextArea::new(vec![prefill]);
        channel.move_cursor(CursorMove::End);
        channel.set_placeholder_text("any channel, e.g. orders:new");
        let mut payload = TextArea::default();
        payload.set_placeholder_text("message");
        let mut popup = Self {
            channel,
            payload,
            focus,
            read_only,
        };
        popup.style_fields();
        popup
    }

    fn focus(&mut self, focus: PublishField) {
        self.focus = focus;
        self.style_fields();
    }

    /// Highlight the focused field; hide the other one's cursor.
    fn style_fields(&mut self) {
        let cfg = config::get();
        let focus = self.focus;
        for (field, area, title) in [
            (PublishField::Channel, &mut self.channel, " Channel "),
            (PublishField::Payload, &mut self.payload, " Payload "),
        ] {
            let focused = field == focus;
            area.set_block(
                Block::default()
                    .border_style(if focused {
                        cfg.colors.base0d
                    } else {
                        cfg.colors.base03
                    })
                    .border_type(BorderType::Rounded)
                    .borders(Borders::ALL)
                    .title(title),
            );
            area.set_cursor_line_style(Style::default());
            area.set_cursor_style(if focused {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            });
        }
    }

    fn channel(&self) -> String {
        self.channel.lines().first().cloned().unwrap_or_default()
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match (key.code, self.focus) {
            (KeyCode::Tab | KeyCode::BackTab | KeyCode::Enter, PublishField::Channel) => {
                self.focus(PublishField::Payload);
            }
            (KeyCode::Tab | KeyCode::BackTab, PublishField::Payload) => {
                self.focus(PublishField::Channel);
            }
            (_, PublishField::Channel) => {
                self.channel.input(key);
            }
            (_, PublishField::Payload) => {
                self.payload.input(key);
            }
        }
    }
}

enum Popup {
    Add(Box<AddPopup>),
    Publish(Box<PublishPopup>),
}

pub struct PubSub {
    store: Arc<Mutex<PubSubStore>>,
    /// Index of the selected row (see [`Row`]).
    selected: usize,
    popup: Option<Popup>,
    /// The server runs in cluster mode: `PUBSUB CHANNELS` is this node's view.
    pub cluster_mode: bool,
}

impl PubSub {
    pub fn new(store: Arc<Mutex<PubSubStore>>) -> Self {
        Self {
            store,
            selected: 0,
            popup: None,
            cluster_mode: false,
        }
    }

    pub fn select_next(&mut self) {
        let len = row_count(&self.store.lock());
        if self.selected + 1 < len {
            self.selected += 1;
        }
    }

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Select subscription `index` (subscriptions come first in the list).
    pub fn select(&mut self, index: usize) {
        self.selected = index;
    }

    pub fn selected_row(&self) -> Option<Row> {
        row_at(&self.store.lock(), self.selected)
    }

    /// Index into `PubSubStore::subs` of the selected subscription, if a
    /// subscription (not a server channel) is selected.
    pub fn selected_sub(&self) -> Option<usize> {
        match self.selected_row() {
            Some(Row::Sub(i)) => Some(i),
            _ => None,
        }
    }

    /// Target of the selected subscription.
    pub fn selected_target(&self) -> Option<Target> {
        let store = self.store.lock();
        match row_at(&store, self.selected) {
            Some(Row::Sub(i)) => store.subs.get(i).map(|s| s.target.clone()),
            _ => None,
        }
    }

    /// Keep the selection on an existing row after the list changed.
    pub fn clamp_selection(&mut self) {
        let len = row_count(&self.store.lock());
        self.selected = self.selected.min(len.saturating_sub(1));
    }

    pub fn open_add(&mut self) {
        self.popup = Some(Popup::Add(Box::new(AddPopup::new())));
    }

    /// Open the publish popup, prefilled from the selection.
    pub fn open_publish(&mut self, read_only: bool) {
        let prefill = {
            let store = self.store.lock();
            publish_prefill(&store, row_at(&store, self.selected).as_ref())
        };
        self.popup = Some(Popup::Publish(Box::new(PublishPopup::new(
            prefill, read_only,
        ))));
    }

    pub fn is_popup(&self) -> bool {
        self.popup.is_some()
    }

    pub fn is_publish_popup(&self) -> bool {
        matches!(self.popup, Some(Popup::Publish(_)))
    }

    pub fn close_popup(&mut self) {
        self.popup = None;
    }

    /// A key typed into the open popup (the App handles confirm / cancel).
    /// Add popup: Tab flips the channel / pattern kind. Publish popup: Tab
    /// switches fields, Enter in the channel field moves to the payload.
    pub fn handle_popup_key(&mut self, key: KeyEvent) {
        match self.popup.as_mut() {
            Some(Popup::Add(popup)) => {
                if key.code == KeyCode::Tab || key.code == KeyCode::BackTab {
                    popup.kind_override = Some(popup.kind().flipped());
                    return;
                }
                popup.input.input(key);
            }
            Some(Popup::Publish(popup)) => popup.handle_key(key),
            None => {}
        }
    }

    /// Close the add popup and return the target typed, if any. Surrounding
    /// whitespace is dropped.
    pub fn take_add_request(&mut self) -> Option<Target> {
        if !matches!(self.popup, Some(Popup::Add(_))) {
            return None;
        }
        let Some(Popup::Add(popup)) = self.popup.take() else {
            return None;
        };
        let kind = popup.kind();
        let name = popup.text().trim().to_owned();
        (!name.is_empty()).then(|| Target::new(kind, name))
    }

    /// `(channel, payload)` to publish, closing the popup. `None` keeps the
    /// popup open: read-only mode, or no channel typed yet.
    pub fn take_publish_request(&mut self) -> Option<(String, String)> {
        let Some(Popup::Publish(popup)) = self.popup.as_ref() else {
            return None;
        };
        let channel = popup.channel().trim().to_owned();
        if popup.read_only || channel.is_empty() {
            return None;
        }
        let payload = popup.payload.lines().join("\n");
        self.popup = None;
        Some((channel, payload))
    }
}

pub struct PubSubWidget;

impl StatefulWidget for PubSubWidget {
    type State = PubSub;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let cfg = config::get();
        state.clamp_selection();

        let outer = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(cfg.colors.base04)
            .bg(cfg.colors.base00)
            .fg(cfg.colors.base05);
        let inner = outer.inner(area);

        let [list_area, log_area] =
            Layout::horizontal([Constraint::Length(LIST_WIDTH), Constraint::Fill(1)]).areas(inner);
        let list_inner = pane_block("").inner(list_area);
        let log_inner = pane_block("").inner(log_area);

        // Build everything under one short lock, render after releasing it.
        let now = SystemTime::now();
        let (status, list, log) = {
            let store = state.store.lock();
            let row = row_at(&store, state.selected);
            (
                store.status.clone(),
                list_lines(&store, state.selected, list_inner, now),
                log_view(&store, row.as_ref(), log_inner, now),
            )
        };

        let mut title = vec![
            Span::raw(" Pub/Sub ── "),
            Span::styled(
                status.label(std::time::Instant::now()),
                status_style(&status),
            ),
            Span::raw(" "),
        ];
        if state.cluster_mode {
            title.push(Span::raw("── "));
            title.push(Span::styled(CLUSTER_NOTE, cfg.colors.base0b));
            title.push(Span::raw(" "));
        }
        outer.title(Line::from(title)).render(area, buf);

        Paragraph::new(list)
            .block(pane_block(" Subscriptions "))
            .render(list_area, buf);

        let mut block = pane_block(&log.title);
        if let Some(footer) = log.footer {
            block = block.title_bottom(Line::from(footer).right_aligned());
        }
        if log.lines.is_empty() {
            block.render(log_area, buf);
            render_placeholder(&log.placeholder, log_inner, buf);
        } else {
            Paragraph::new(log.lines).block(block).render(log_area, buf);
        }

        match state.popup.as_ref() {
            Some(Popup::Add(popup)) => render_add_popup(popup, area, buf),
            Some(Popup::Publish(popup)) => render_publish_popup(popup, area, buf),
            None => {}
        }
    }
}

fn pane_block(title: &str) -> Block<'_> {
    let cfg = config::get();
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(cfg.colors.base03)
        .title(title)
}

fn status_style(status: &ConnStatus) -> Style {
    let cfg = config::get();
    match status {
        ConnStatus::Connected => Style::new().fg(cfg.colors.base0a),
        ConnStatus::Connecting => Style::new().fg(cfg.colors.base0b),
        ConnStatus::Reconnecting { .. } => Style::new().fg(cfg.colors.base08),
        ConnStatus::NotStarted => Style::new().fg(cfg.colors.base03),
    }
}

/// Lines of the left pane: own subscriptions (`● name  N/s`), then the
/// server's other channels (`○ name  N subs`), scrolled to keep the
/// selection visible.
fn list_lines(
    store: &PubSubStore,
    selected: usize,
    area: Rect,
    now: SystemTime,
) -> Vec<Line<'static>> {
    let cfg = config::get();
    let width = usize::from(area.width);
    let selected_style = Style::new()
        .bg(cfg.colors.base02)
        .add_modifier(Modifier::BOLD);
    let mut lines = Vec::new();
    let mut selected_line = None;
    let mut row = 0;
    let mut push_row = |lines: &mut Vec<Line<'static>>, line: Line<'static>| {
        if row == selected {
            selected_line = Some(lines.len());
            lines.push(line.style(selected_style));
        } else {
            lines.push(line);
        }
        row += 1;
    };

    if store.subs.is_empty() {
        lines.push(Line::styled(" a add subscription", cfg.colors.base03));
    }
    for sub in &store.subs {
        let dot = if sub.target.is_pattern() {
            cfg.colors.base0e
        } else {
            cfg.colors.base0d
        };
        let rate = format!(" {}/s", sub.stats.rate_1s(now));
        push_row(
            &mut lines,
            list_row(Span::styled(" ● ", dot), sub.target.name(), rate, width),
        );
    }

    if let Some(server) = store.server.as_ref() {
        let title = "─ Server channels ";
        lines.push(Line::styled(
            format!("{title}{}", "─".repeat(width.saturating_sub(title.width()))),
            cfg.colors.base03,
        ));
        let before = lines.len();
        for (name, subs) in store.server_only_channels() {
            let count = format!(" {subs} {}", if *subs == 1 { "sub" } else { "subs" });
            push_row(
                &mut lines,
                list_row(Span::styled(" ○ ", cfg.colors.base04), name, count, width),
            );
        }
        if lines.len() == before {
            lines.push(Line::styled(" no other channels", cfg.colors.base03));
        }
        lines.push(Line::styled(
            format!(" patterns on server: {}", server.patterns),
            cfg.colors.base03,
        ));
        if server.total > server.channels.len() {
            lines.push(Line::styled(
                format!(" showing {} of {}", server.channels.len(), server.total),
                cfg.colors.base03,
            ));
        }
    }

    let height = usize::from(area.height).max(1);
    let offset = selected_line.map_or(0, |line| line.saturating_sub(height - 1));
    lines.into_iter().skip(offset).take(height).collect()
}

/// `marker name  suffix`, the name cut so the suffix stays right-aligned.
fn list_row(marker: Span<'static>, name: &str, suffix: String, width: usize) -> Line<'static> {
    let cfg = config::get();
    let name_width = width.saturating_sub(marker.width() + suffix.width());
    let name = truncate_to_width(name, name_width);
    let pad = name_width.saturating_sub(name.width());
    Line::from(vec![
        marker,
        Span::raw(format!("{name}{}", " ".repeat(pad))),
        Span::styled(suffix, cfg.colors.base04),
    ])
}

struct LogView {
    title: String,
    footer: Option<String>,
    lines: Vec<Line<'static>>,
    placeholder: String,
}

/// The log of the selected subscription, following the newest message.
fn log_view(store: &PubSubStore, row: Option<&Row>, area: Rect, now: SystemTime) -> LogView {
    if let Some(Row::Server(name)) = row {
        let subs = store
            .server_only_channels()
            .find(|(n, _)| n == name)
            .map_or(0, |(_, subs)| *subs);
        return LogView {
            title: format!(" {name} "),
            footer: None,
            lines: Vec::new(),
            placeholder: format!(
                "{subs} subscriber{} · Enter to subscribe",
                if subs == 1 { "" } else { "s" }
            ),
        };
    }
    let Some(sub) = row.and_then(|row| match row {
        Row::Sub(i) => store.subs.get(*i),
        Row::Server(_) => None,
    }) else {
        return LogView {
            title: " Messages ".into(),
            footer: None,
            lines: Vec::new(),
            placeholder: "No subscription selected · a add one".into(),
        };
    };
    let width = usize::from(area.width);
    let lines = if sub.buffer.is_empty() {
        Vec::new()
    } else {
        sub.buffer
            .last_n(usize::from(area.height))
            .map(|msg| log_line(msg, width))
            .collect()
    };
    LogView {
        title: format!(
            " {} ── {} msg/s ── following ",
            sub.target,
            sub.stats.rate_1s(now)
        ),
        footer: Some(format!(
            " {} kept ({}) · {} received · {:.1}/s 1 min ",
            sub.buffer.len(),
            format_bytes(sub.buffer.bytes()),
            sub.stats.total,
            sub.stats.rate_60s(now),
        )),
        lines,
        placeholder: "Waiting for messages…".into(),
    }
}

/// `HH:MM:SS.mmm channel payload`, cut to `width` columns. A reconnect
/// marker is a centered note.
fn log_line(msg: &PubSubMessage, width: usize) -> Line<'static> {
    let cfg = config::get();
    if msg.kind == EntryKind::Reconnected {
        return Line::styled(RECONNECT_MARKER, cfg.colors.base03).centered();
    }
    let time = format!("{} ", clock(msg.received_at));
    let channel = format!("{} ", msg.channel);
    let budget = width.saturating_sub(time.width() + channel.width());
    let preview = payload_preview(&msg.payload, msg.size);
    Line::from(vec![
        Span::styled(time, cfg.colors.base03),
        Span::styled(channel, cfg.colors.base0d),
        Span::styled(
            truncate_to_width(&preview, budget).into_owned(),
            cfg.colors.base05,
        ),
    ])
}

/// One-line preview: control characters become spaces; binary payloads show
/// their size and the first bytes in hex.
fn payload_preview(payload: &Payload, size: usize) -> String {
    match payload {
        Payload::Text(text) => text
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect(),
        Payload::Binary(bytes) => {
            let mut out = format!("[binary {size} B]");
            for b in bytes.iter().take(HEX_PREVIEW) {
                let _ = write!(out, " {b:02x}");
            }
            if bytes.len() > HEX_PREVIEW {
                out.push_str(" …");
            }
            out
        }
    }
}

/// Local wall-clock time `HH:MM:SS.mmm`.
fn clock(t: SystemTime) -> String {
    let since = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let (h, m, s) = local_hms(since.as_secs());
    format!("{h:02}:{m:02}:{s:02}.{:03}", since.subsec_millis())
}

#[cfg(unix)]
fn local_hms(secs: u64) -> (u64, u64, u64) {
    let Ok(t) = libc::time_t::try_from(secs) else {
        return utc_hms(secs);
    };
    // SAFETY: `tm` is plain data; `localtime_r` only writes into it and is
    // thread-safe (unlike `localtime`).
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&raw const t, &raw mut tm) }.is_null() {
        return utc_hms(secs);
    }
    let part = |v: libc::c_int| u64::try_from(v).unwrap_or(0);
    (part(tm.tm_hour), part(tm.tm_min), part(tm.tm_sec))
}

#[cfg(not(unix))]
fn local_hms(secs: u64) -> (u64, u64, u64) {
    utc_hms(secs)
}

fn utc_hms(secs: u64) -> (u64, u64, u64) {
    let day = secs % 86_400;
    (day / 3600, day % 3600 / 60, day % 60)
}

fn render_placeholder(text: &str, area: Rect, buf: &mut Buffer) {
    let [centered] = Layout::vertical([Constraint::Length(1)])
        .flex(Flex::Center)
        .areas(area);
    Paragraph::new(text)
        .fg(config::get().colors.base03)
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .render(centered, buf);
}

fn render_add_popup(popup: &AddPopup, area: Rect, buf: &mut Buffer) {
    let cfg = config::get();
    let [_, popup_area, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(5),
        Constraint::Fill(1),
    ])
    .areas(area);
    let [_, popup_area, _] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(56.min(area.width)),
        Constraint::Fill(1),
    ])
    .areas(popup_area);
    Clear.render(popup_area, buf);
    Block::default()
        .bg(cfg.colors.base00)
        .render(popup_area, buf);

    let [input_area, kind_area, hint_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(popup_area);
    popup.input.render(input_area, buf);

    let (channel, pattern) = kind_labels(popup.kind());
    let on = Style::new().fg(cfg.colors.base0a).bold();
    let off = Style::new().fg(cfg.colors.base03);
    let (channel_style, pattern_style) = match popup.kind() {
        TargetKind::Channel => (on, off),
        TargetKind::Pattern => (off, on),
    };
    Paragraph::new(Line::from(vec![
        Span::styled(channel, channel_style),
        Span::styled(" / ", off),
        Span::styled(pattern, pattern_style),
    ]))
    .alignment(Alignment::Center)
    .render(kind_area, buf);
    Paragraph::new(Line::styled(
        "Tab kind · Enter subscribe · Esc cancel",
        cfg.colors.base03,
    ))
    .alignment(Alignment::Center)
    .render(hint_area, buf);
}

fn render_publish_popup(popup: &PublishPopup, area: Rect, buf: &mut Buffer) {
    let cfg = config::get();
    let [_, popup_area, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(14.min(area.height)),
        Constraint::Fill(1),
    ])
    .areas(area);
    let [_, popup_area, _] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(64.min(area.width)),
        Constraint::Fill(1),
    ])
    .areas(popup_area);
    Clear.render(popup_area, buf);
    let block = Block::default()
        .title(" Publish ")
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(cfg.colors.base04)
        .bg(cfg.colors.base00);
    let inner = block.inner(popup_area);
    block.render(popup_area, buf);

    let [channel_area, payload_area, notice_area, hint_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    popup.channel.render(channel_area, buf);
    popup.payload.render(payload_area, buf);
    let hint = if popup.read_only {
        Paragraph::new(Line::styled(READ_ONLY_PUBLISH, cfg.colors.base08))
            .alignment(Alignment::Center)
            .render(notice_area, buf);
        "Tab field · Esc cancel"
    } else {
        "Tab field · Ctrl-s send · Esc cancel"
    };
    Paragraph::new(Line::styled(hint, cfg.colors.base03))
        .alignment(Alignment::Center)
        .render(hint_area, buf);
}

/// `[channel] / pattern`: the selected kind is bracketed.
fn kind_labels(kind: TargetKind) -> (&'static str, &'static str) {
    match kind {
        TargetKind::Channel => ("[channel]", "pattern"),
        TargetKind::Pattern => ("channel", "[pattern]"),
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::redis_client::pubsub_store::{BufferLimits, ServerChannels};

    fn widget() -> PubSub {
        PubSub::new(Arc::new(Mutex::new(PubSubStore::new(BufferLimits {
            messages: 10,
            bytes: 1024,
            payload: 1024,
        }))))
    }

    fn type_text(pubsub: &mut PubSub, text: &str) {
        for c in text.chars() {
            pubsub.handle_popup_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty()));
        }
    }

    #[test]
    fn add_popup_detects_kind_and_tab_flips_it() {
        let mut p = widget();
        p.open_add();
        type_text(&mut p, "orders:*");
        assert_eq!(
            match p.popup.as_ref() {
                Some(Popup::Add(a)) => a.kind(),
                _ => panic!("no add popup"),
            },
            TargetKind::Pattern
        );
        p.handle_popup_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        assert_eq!(
            p.take_add_request(),
            Some(Target::Channel("orders:*".into()))
        );
        assert!(!p.is_popup());

        p.open_add();
        type_text(&mut p, " chat:lobby ");
        assert_eq!(
            p.take_add_request(),
            Some(Target::Channel("chat:lobby".into()))
        );

        p.open_add();
        type_text(&mut p, "   ");
        assert_eq!(p.take_add_request(), None);
    }

    #[test]
    fn selection_stays_in_range() {
        let mut p = widget();
        p.select_next();
        assert_eq!(p.selected, 0);
        {
            let mut store = p.store.lock();
            store.add(Target::Channel("a".into()));
            store.add(Target::Channel("b".into()));
        }
        p.select_next();
        p.select_next();
        assert_eq!(p.selected, 1);
        p.store.lock().remove(&Target::Channel("b".into()));
        p.clamp_selection();
        assert_eq!(p.selected, 0);
        assert_eq!(p.selected_target(), Some(Target::Channel("a".into())));
    }

    #[test]
    fn preview_is_one_line() {
        let text = Payload::Text("a\nb\tc".into());
        assert_eq!(payload_preview(&text, 5), "a b c");
        let bin = Payload::Binary((0u8..20).collect());
        assert_eq!(
            payload_preview(&bin, 312),
            "[binary 312 B] 00 01 02 03 04 05 06 07 08 09 0a 0b 0c 0d 0e 0f …"
        );
    }

    fn screen(buf: &Buffer) -> String {
        let area = buf.area;
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_subscriptions_and_follows_the_log() {
        let mut p = widget();
        {
            let mut store = p.store.lock();
            store.add(Target::Pattern("orders:*".into()));
            store.add(Target::Channel("chat:lobby".into()));
            for i in 0..30 {
                store.push(
                    "orders:new",
                    Some("orders:*"),
                    format!("m{i}").as_bytes(),
                    SystemTime::now(),
                );
            }
            store.status = ConnStatus::Connected;
        }
        let area = Rect::new(0, 0, 100, 12);
        let mut buf = Buffer::empty(area);
        PubSubWidget.render(area, &mut buf, &mut p);
        let text = screen(&buf);
        assert!(text.contains("Pub/Sub ── connected"), "{text}");
        assert!(text.contains("● orders:*"), "{text}");
        assert!(text.contains("● chat:lobby"), "{text}");
        assert!(text.contains("orders:* (pattern)"), "{text}");
        // Follow mode: the newest message is on screen, the oldest isn't.
        assert!(text.contains("orders:new m29"), "{text}");
        assert!(!text.contains("orders:new m0 "), "{text}");
        assert!(text.contains("10 kept"), "{text}");

        p.open_add();
        PubSubWidget.render(area, &mut buf, &mut p);
        assert!(screen(&buf).contains("[channel] / pattern"));
    }

    fn server(channels: &[(&str, u64)]) -> ServerChannels {
        ServerChannels {
            channels: channels.iter().map(|(n, c)| ((*n).into(), *c)).collect(),
            total: channels.len(),
            patterns: 3,
        }
    }

    #[test]
    fn publish_prefill_rules() {
        let mut store = PubSubStore::new(BufferLimits {
            messages: 10,
            bytes: 1024,
            payload: 1024,
        });
        store.add(Target::Channel("chat".into()));
        store.add(Target::Pattern("orders:*".into()));
        store.set_server_channels(server(&[("jobs", 1)]));

        assert_eq!(publish_prefill(&store, None), "");
        assert_eq!(publish_prefill(&store, Some(&Row::Sub(0))), "chat");
        assert_eq!(
            publish_prefill(&store, Some(&Row::Server("jobs".into()))),
            "jobs"
        );
        // A pattern without messages has no concrete channel yet.
        assert_eq!(publish_prefill(&store, Some(&Row::Sub(1))), "");
        let now = SystemTime::now();
        store.push("orders:new", Some("orders:*"), b"1", now);
        store.push("orders:paid", Some("orders:*"), b"2", now);
        // A marker isn't a message: the newest message's channel wins.
        store.mark_reconnected(&[Target::Pattern("orders:*".into())], now);
        assert_eq!(publish_prefill(&store, Some(&Row::Sub(1))), "orders:paid");
    }

    #[test]
    fn rows_cover_subscriptions_then_server_channels() {
        let mut p = widget();
        {
            let mut store = p.store.lock();
            store.add(Target::Channel("chat".into()));
            store.set_server_channels(server(&[("chat", 2), ("events", 2), ("jobs", 1)]));
        }
        for _ in 0..5 {
            p.select_next();
        }
        // chat is listed once (as ours): rows are chat, events, jobs.
        assert_eq!(p.selected_row(), Some(Row::Server("jobs".into())));
        assert_eq!(p.selected_target(), None);
        assert_eq!(p.selected_sub(), None);
        p.select(0);
        assert_eq!(p.selected_sub(), Some(0));
    }

    #[test]
    fn publish_popup_request() {
        let mut p = widget();
        p.open_publish(false);
        // Nothing selected: the channel starts empty and must be typed.
        assert_eq!(p.take_publish_request(), None);
        assert!(p.is_publish_popup());
        type_text(&mut p, "orders:new");
        p.handle_popup_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
        type_text(&mut p, "a");
        p.handle_popup_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
        type_text(&mut p, "b");
        assert_eq!(
            p.take_publish_request(),
            Some(("orders:new".into(), "a\nb".into()))
        );
        assert!(!p.is_popup());

        p.store.lock().add(Target::Channel("chat".into()));
        p.open_publish(true);
        type_text(&mut p, "hi");
        assert_eq!(p.take_publish_request(), None, "read-only refuses");
        assert!(p.is_publish_popup());
        assert_eq!(p.take_add_request(), None);
        assert!(p.is_publish_popup(), "take_add_request keeps other popups");
    }

    #[test]
    fn renders_server_channels_status_and_marker() {
        let mut p = widget();
        {
            let mut store = p.store.lock();
            store.add(Target::Channel("chat".into()));
            store.push("chat", None, b"hello", SystemTime::now());
            store.mark_reconnected(&[Target::Channel("chat".into())], SystemTime::now());
            store.set_server_channels(server(&[("chat", 2), ("events", 1)]));
            store.status = ConnStatus::Reconnecting {
                attempt: 2,
                next_at: std::time::Instant::now() + std::time::Duration::from_millis(1_500),
            };
        }
        p.cluster_mode = true;
        let area = Rect::new(0, 0, 120, 14);
        let mut buf = Buffer::empty(area);
        PubSubWidget.render(area, &mut buf, &mut p);
        let text = screen(&buf);
        assert!(text.contains("reconnecting in 2 s"), "{text}");
        assert!(text.contains(CLUSTER_NOTE), "{text}");
        assert!(text.contains("─ Server channels"), "{text}");
        assert!(text.contains("○ events"), "{text}");
        assert!(text.contains("1 sub"), "{text}");
        assert!(!text.contains("○ chat"), "{text}");
        assert!(text.contains("patterns on server: 3"), "{text}");
        assert!(text.contains(RECONNECT_MARKER), "{text}");

        p.select_next();
        PubSubWidget.render(area, &mut buf, &mut p);
        assert!(
            screen(&buf).contains("1 subscriber · Enter to subscribe"),
            "{}",
            screen(&buf)
        );

        p.open_publish(true);
        PubSubWidget.render(area, &mut buf, &mut p);
        let text = screen(&buf);
        assert!(text.contains(READ_ONLY_PUBLISH), "{text}");
        assert!(text.contains("events"), "prefilled: {text}");
    }

    #[test]
    fn kind_label_brackets_the_selected_kind() {
        assert_eq!(kind_labels(TargetKind::Channel), ("[channel]", "pattern"));
        assert_eq!(kind_labels(TargetKind::Pattern), ("channel", "[pattern]"));
    }

    #[test]
    fn utc_clock_parts() {
        assert_eq!(utc_hms(0), (0, 0, 0));
        assert_eq!(utc_hms(86_400 + 3_723), (1, 2, 3));
        let t = UNIX_EPOCH + std::time::Duration::from_millis(5_042);
        assert!(clock(t).ends_with(".042"));
    }
}
