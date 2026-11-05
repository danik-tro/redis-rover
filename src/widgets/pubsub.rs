//! The Pub/Sub tab: own subscriptions and the server's channels on the left,
//! the selected subscription's live log on the right, and the add / publish
//! popups.

use std::{
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
    pubsub_notify::NotifyState,
    redis_client::pubsub_store::{
        detect_kind, ConnStatus, EntryKind, MessageBuffer, Payload, PubSubMessage, PubSubStore,
        Subscription, Target, TargetKind,
    },
    utils::{fmt_bytes, group_thousands},
    widgets::{
        keyspace::truncate_to_width,
        text_view::{self, TextCache, TextLabels, TextView},
    },
};

/// Width of the subscriptions pane.
const LIST_WIDTH: u16 = 32;
/// Log row standing for a reconnect.
const RECONNECT_MARKER: &str = "— reconnected, messages may be missing —";
/// Header note in cluster mode.
const CLUSTER_NOTE: &str = "⚠ PUBSUB CHANNELS: this node only";
/// Publish popup notice under `--read-only`.
const READ_ONLY_PUBLISH: &str = "Read-only mode: publishing is disabled";
/// Rows of the stats panel, borders included.
const STATS_HEIGHT: u16 = 5;
/// The stats panel only shows when the right pane has this many rows.
const STATS_MIN_PANE: u16 = 12;
/// Concrete channels listed in the stats panel of a pattern.
const TOP_CHANNELS: usize = 5;

/// Pane that `j` / `k` drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pane {
    #[default]
    List,
    Log,
}

/// A paused log: what the user looks at, by sequence number so evictions
/// don't shift it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Paused {
    /// Selected entry.
    selected: u64,
    /// First entry shown.
    top: u64,
    /// Newest entry when the log was paused; what came after is `+N new`.
    newest_at_pause: u64,
    /// The paused rows were evicted and the view jumped to the oldest one.
    evicted: bool,
}

/// Position in a subscription's log: following the newest entry (default)
/// or paused on one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LogCursor {
    paused: Option<Paused>,
    /// Rows the log showed on the last render.
    height: usize,
}

impl LogCursor {
    pub fn is_paused(&self) -> bool {
        self.paused.is_some()
    }

    pub fn follow(&mut self) {
        self.paused = None;
    }

    /// Freeze the view as it is (the newest `height` rows), selecting the
    /// newest entry. Nothing to pause on an empty buffer.
    fn pause(&mut self, buf: &MessageBuffer) -> bool {
        let Some(last) = buf.last_seq() else {
            return false;
        };
        let top = buf
            .get(buf.len().saturating_sub(self.height.max(1)))
            .map_or(last, |m| m.seq);
        self.paused = Some(Paused {
            selected: last,
            top,
            newest_at_pause: last,
            evicted: false,
        });
        true
    }

    /// Space: pause, or resume following.
    pub fn toggle_pause(&mut self, buf: &MessageBuffer) {
        if self.is_paused() {
            self.follow();
        } else {
            self.pause(buf);
        }
    }

    /// Index of the selected entry (the oldest kept one if it was evicted).
    fn selected_index(paused: &Paused, buf: &MessageBuffer) -> usize {
        buf.index_of(paused.selected)
            .unwrap_or_else(|i| i)
            .min(buf.len().saturating_sub(1))
    }

    /// `k`: select the previous entry. Pauses a following log first.
    pub fn up(&mut self, buf: &MessageBuffer) {
        if !self.is_paused() && !self.pause(buf) {
            return;
        }
        let Some(paused) = self.paused.as_mut() else {
            return;
        };
        let i = Self::selected_index(paused, buf);
        if let Some(prev) = i.checked_sub(1).and_then(|i| buf.get(i)) {
            paused.selected = prev.seq;
            paused.evicted = false;
        }
    }

    /// `j`: select the next entry; past the newest one, follow again.
    pub fn down(&mut self, buf: &MessageBuffer) {
        let Some(paused) = self.paused.as_mut() else {
            return;
        };
        let i = Self::selected_index(paused, buf);
        match buf.get(i + 1) {
            Some(next) => {
                paused.selected = next.seq;
                paused.evicted = false;
            }
            None => self.follow(),
        }
    }

    /// Fit the view to `buf` and `height` rows: returns the index of the
    /// first row shown and of the selected one (`None` when empty). A
    /// paused view whose rows were evicted moves to the oldest entry still
    /// kept; an empty buffer (cleared) follows again.
    pub fn sync(&mut self, buf: &MessageBuffer, height: usize) -> (usize, Option<usize>) {
        self.height = height;
        let height = height.max(1);
        let (Some(first), len) = (buf.first_seq(), buf.len()) else {
            self.follow();
            return (0, None);
        };
        let Some(paused) = self.paused.as_mut() else {
            return (len.saturating_sub(height), Some(len - 1));
        };
        if paused.selected < first {
            paused.selected = first;
            paused.evicted = true;
        }
        let selected = Self::selected_index(paused, buf);
        let mut top = buf.index_of(paused.top.max(first)).unwrap_or_else(|i| i);
        if selected < top {
            top = selected;
        } else if selected >= top + height {
            top = selected + 1 - height;
        }
        paused.top = buf.get(top).map_or(first, |m| m.seq);
        (top, Some(selected))
    }

    /// Sequence number of the selected entry: the newest one while following.
    pub fn selected_seq(&self, buf: &MessageBuffer) -> Option<u64> {
        match self.paused {
            Some(paused) => buf
                .first_seq()
                .map(|first| paused.selected.max(first))
                .filter(|_| !buf.is_empty()),
            None => buf.last_seq(),
        }
    }

    /// Entries that arrived since the log was paused, counted by `seq`.
    pub fn new_count(&self, buf: &MessageBuffer) -> u64 {
        match (self.paused, buf.last_seq()) {
            (Some(paused), Some(last)) => last.saturating_sub(paused.newest_at_pause),
            _ => 0,
        }
    }

    pub fn evicted(&self) -> bool {
        self.paused.is_some_and(|p| p.evicted)
    }
}

/// The message detail pane: one message, its full payload in the text viewer.
struct Detail {
    msg: PubSubMessage,
    /// The pattern of a pattern subscription.
    pattern: Option<String>,
    text: TextCache,
    view: TextView,
}

impl Detail {
    fn new(msg: PubSubMessage, pattern: Option<String>) -> Self {
        let text = match &msg.payload {
            Payload::Text(text) => TextCache::from_text(text, !msg.truncated),
            binary @ Payload::Binary(_) => TextCache::from_lines(binary.hex_dump()),
        };
        Self {
            msg,
            pattern,
            text,
            view: TextView::default(),
        }
    }
}

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
    focus: Pane,
    /// Log position of the selected subscription.
    log: LogCursor,
    detail: Option<Detail>,
}

impl PubSub {
    pub fn new(store: Arc<Mutex<PubSubStore>>) -> Self {
        Self {
            store,
            selected: 0,
            popup: None,
            cluster_mode: false,
            focus: Pane::List,
            log: LogCursor::default(),
            detail: None,
        }
    }

    pub fn select_next(&mut self) {
        let len = row_count(&self.store.lock());
        if self.selected + 1 < len {
            self.select(self.selected + 1);
        }
    }

    pub fn select_prev(&mut self) {
        self.select(self.selected.saturating_sub(1));
    }

    /// Select row `index` (subscriptions come first in the list). Another
    /// row starts its log following.
    pub fn select(&mut self, index: usize) {
        if index != self.selected {
            self.reset_log();
        }
        self.selected = index;
    }

    /// Follow the log again and close the detail (`c`, `d`, another row).
    pub fn reset_log(&mut self) {
        self.log = LogCursor::default();
        self.detail = None;
    }

    pub fn focus(&self) -> Pane {
        self.focus
    }

    /// Tab: list ↔ log. A server channel has no log, so the list keeps it.
    pub fn toggle_focus(&mut self) {
        self.focus = if self.focus == Pane::List && self.selected_sub().is_some() {
            Pane::Log
        } else {
            Pane::List
        };
    }

    #[cfg(test)]
    pub fn log_cursor(&self) -> LogCursor {
        self.log
    }

    /// Run `f` on the log cursor and the selected subscription's buffer.
    fn with_log(&mut self, f: impl FnOnce(&mut LogCursor, &MessageBuffer)) {
        let store = self.store.lock();
        if let Some(Row::Sub(i)) = row_at(&store, self.selected) {
            if let Some(sub) = store.subs.get(i) {
                f(&mut self.log, &sub.buffer);
            }
        }
    }

    /// Space: pause / follow the log.
    pub fn toggle_pause(&mut self) {
        self.with_log(LogCursor::toggle_pause);
    }

    pub fn log_up(&mut self) {
        self.with_log(LogCursor::up);
    }

    pub fn log_down(&mut self) {
        self.with_log(LogCursor::down);
    }

    /// The selected log entry with the pattern of its subscription. Marker
    /// rows aren't messages.
    fn selected_log_message(&self) -> Option<(PubSubMessage, Option<String>)> {
        let store = self.store.lock();
        let Some(Row::Sub(i)) = row_at(&store, self.selected) else {
            return None;
        };
        let sub = store.subs.get(i)?;
        let seq = self.log.selected_seq(&sub.buffer)?;
        let index = sub.buffer.index_of(seq).ok()?;
        let msg = sub.buffer.get(index)?;
        (msg.kind == EntryKind::Message).then(|| {
            let pattern = sub
                .target
                .is_pattern()
                .then(|| sub.target.name().to_owned());
            (msg.clone(), pattern)
        })
    }

    /// Message `y` / `Shift-y` copy: the one in the detail, else the
    /// selected log entry (the newest while following).
    pub fn selected_message(&self) -> Option<PubSubMessage> {
        match self.detail.as_ref() {
            Some(detail) => Some(detail.msg.clone()),
            None => self.selected_log_message().map(|(msg, _)| msg),
        }
    }

    /// Enter in the log: open the selected message. Returns whether it opened.
    pub fn open_detail(&mut self) -> bool {
        let Some((msg, pattern)) = self.selected_log_message() else {
            return false;
        };
        self.detail = Some(Detail::new(msg, pattern));
        true
    }

    pub fn is_detail(&self) -> bool {
        self.detail.is_some()
    }

    pub fn close_detail(&mut self) {
        self.detail = None;
    }

    pub fn scroll_detail(&mut self, down: bool) {
        if let Some(detail) = self.detail.as_mut() {
            detail.view.scroll(down, 1);
        }
    }

    pub fn pan_detail(&mut self, right: bool) {
        if let Some(detail) = self.detail.as_mut() {
            detail.view.pan(right);
        }
    }

    pub fn toggle_detail_wrap(&mut self) {
        if let Some(detail) = self.detail.as_mut() {
            detail.view.toggle_wrap();
        }
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

    /// Keep the selection on an existing row after the list changed. The
    /// log focus goes back to the list when no subscription is selected.
    pub fn clamp_selection(&mut self) {
        let len = row_count(&self.store.lock());
        self.select(self.selected.min(len.saturating_sub(1)));
        if self.selected_sub().is_none() {
            self.focus = Pane::List;
            self.detail = None;
        }
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

        let [list_area, right_area] =
            Layout::horizontal([Constraint::Length(LIST_WIDTH), Constraint::Fill(1)]).areas(inner);
        let list_inner = pane_block("", false).inner(list_area);
        let has_sub = state.selected_sub().is_some();
        let [log_area, stats_area] = if has_sub && right_area.height >= STATS_MIN_PANE {
            Layout::vertical([Constraint::Fill(1), Constraint::Length(STATS_HEIGHT)])
                .areas(right_area)
        } else {
            [right_area, Rect::default()]
        };
        let log_inner = pane_block("", false).inner(log_area);
        let stats_inner = pane_block("", false).inner(stats_area);
        let log_focused = state.focus == Pane::Log;

        // Build everything under one short lock, render after releasing it.
        let now = SystemTime::now();
        let (status, list, log, stats_view) = {
            let store = state.store.lock();
            let row = row_at(&store, state.selected);
            let sub = match row {
                Some(Row::Sub(i)) => store.subs.get(i),
                _ => None,
            };
            (
                store.status.clone(),
                list_lines(&store, state.selected, !log_focused, list_inner, now),
                log_view(
                    &store,
                    row.as_ref(),
                    &mut state.log,
                    log_focused,
                    log_inner,
                    now,
                ),
                sub.map(|sub| stats_lines(sub, usize::from(stats_inner.width), now)),
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
            .block(pane_block(" Subscriptions ", !log_focused))
            .render(list_area, buf);

        if let Some(detail) = state.detail.as_mut() {
            render_detail(detail, right_area, buf);
        } else {
            let block = pane_block(&log.title, log_focused);
            if log.lines.is_empty() {
                block.render(log_area, buf);
                render_placeholder(&log.placeholder, log_inner, buf);
            } else {
                Paragraph::new(log.lines).block(block).render(log_area, buf);
            }
            if let Some(stats) = stats_view.filter(|_| !stats_area.is_empty()) {
                Paragraph::new(stats)
                    .block(pane_block(" Stats ", false))
                    .render(stats_area, buf);
            }
        }

        match state.popup.as_ref() {
            Some(Popup::Add(popup)) => render_add_popup(popup, area, buf),
            Some(Popup::Publish(popup)) => render_publish_popup(popup, area, buf),
            None => {}
        }
    }
}

/// A pane with `title`; the focused one has a highlighted border and a bold
/// title.
fn pane_block(title: &str, focused: bool) -> Block<'_> {
    let cfg = config::get();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded);
    if focused {
        block
            .border_style(cfg.colors.base0d)
            .title(Span::styled(title, Style::new().bold()))
    } else {
        block.border_style(cfg.colors.base03).title(title)
    }
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
    focused: bool,
    area: Rect,
    now: SystemTime,
) -> Vec<Line<'static>> {
    let cfg = config::get();
    let width = usize::from(area.width);
    // Unfocused, the selection stays visible but quieter.
    let selected_style = if focused {
        Style::new()
            .bg(cfg.colors.base02)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().bg(cfg.colors.base01)
    };
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
        push_row(
            &mut lines,
            list_row(
                Span::styled(" ● ", dot),
                sub.target.name(),
                sub_suffix(sub, now),
                width,
            ),
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

/// ` 12/s (37) 🔔`: rate, unread badge when there is one, and the notify
/// marker (`🔔` on, `🔕` auto-muted, nothing when off).
fn sub_suffix(sub: &Subscription, now: SystemTime) -> String {
    let unread = if sub.unread > 0 {
        format!(" ({})", sub.unread)
    } else {
        String::new()
    };
    let marker = match sub.notify {
        NotifyState::On { .. } => " 🔔",
        NotifyState::AutoMuted => " 🔕",
        NotifyState::Off => "",
    };
    format!(" {}/s{unread}{marker}", sub.stats.rate_1s(now))
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
    lines: Vec<Line<'static>>,
    placeholder: String,
}

/// The log of the selected subscription: the newest rows while following,
/// a frozen window while paused. The selected row is highlighted when the
/// log is focused or paused.
fn log_view(
    store: &PubSubStore,
    row: Option<&Row>,
    cursor: &mut LogCursor,
    focused: bool,
    area: Rect,
    now: SystemTime,
) -> LogView {
    if let Some(Row::Server(name)) = row {
        let subs = store
            .server_only_channels()
            .find(|(n, _)| n == name)
            .map_or(0, |(_, subs)| *subs);
        return LogView {
            title: format!(" {name} "),
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
            lines: Vec::new(),
            placeholder: "No subscription selected · a add one".into(),
        };
    };
    let width = usize::from(area.width);
    let height = usize::from(area.height);
    let (start, selected) = cursor.sync(&sub.buffer, height);
    let highlight = focused || cursor.is_paused();
    let selected_style = Style::new()
        .bg(config::get().colors.base02)
        .add_modifier(Modifier::BOLD);
    let lines = sub
        .buffer
        .range(start, height)
        .enumerate()
        .map(|(i, msg)| {
            let line = log_line(msg, width);
            if highlight && selected == Some(start + i) {
                line.style(selected_style)
            } else {
                line
            }
        })
        .collect();
    LogView {
        title: format!(
            " {} ── {} msg/s ── {} ",
            sub.target,
            sub.stats.rate_1s(now),
            log_state(cursor, &sub.buffer)
        ),
        lines,
        placeholder: "Waiting for messages…".into(),
    }
}

/// `following`, or `paused · +128 new` (and `· older messages evicted`).
fn log_state(cursor: &LogCursor, buf: &MessageBuffer) -> String {
    if !cursor.is_paused() {
        return "following".into();
    }
    let mut state = format!(
        "paused · +{} new",
        group_thousands_u64(cursor.new_count(buf))
    );
    if cursor.evicted() {
        state.push_str(" · older messages evicted");
    }
    state
}

fn group_thousands_u64(n: u64) -> String {
    group_thousands(usize::try_from(n).unwrap_or(usize::MAX))
}

/// The stats panel: totals and sizes, rates and buffer use, and for a
/// pattern its busiest concrete channels.
fn stats_lines(sub: &Subscription, width: usize, now: SystemTime) -> Vec<Line<'static>> {
    let cfg = config::get();
    let stats = &sub.stats;
    let totals = format!(
        " {} msgs · {} · avg {} · max {}",
        group_thousands_u64(stats.total),
        fmt_bytes(stats.bytes),
        fmt_bytes(stats.avg_size()),
        fmt_bytes(stats.max_size as u64),
    );
    let last = stats.last_at.map_or_else(|| "—".to_owned(), clock);
    let rates = format!(
        " {}/s now · {:.1}/s 1 min · last {last} · kept {} ({})",
        stats.rate_1s(now),
        stats.rate_60s(now),
        group_thousands(sub.buffer.len()),
        fmt_bytes(sub.buffer.bytes() as u64),
    );
    let mut lines = vec![totals, rates];
    if sub.target.is_pattern() {
        let mut top: Vec<String> = stats
            .top_channels(TOP_CHANNELS)
            .into_iter()
            .map(|(name, count)| format!("{name} {}", group_thousands_u64(count)))
            .collect();
        if stats.per_channel.len() > TOP_CHANNELS {
            top.push("…".into());
        }
        if stats.other > 0 {
            top.push(format!("other {}", group_thousands_u64(stats.other)));
        }
        if top.is_empty() {
            top.push("—".into());
        }
        lines.push(format!(" top: {}", top.join(" · ")));
    }
    lines
        .into_iter()
        .map(|l| Line::styled(truncate_to_width(&l, width).into_owned(), cfg.colors.base05))
        .collect()
}

/// The message detail over the right pane: metadata, then the payload in
/// the text viewer (JSON pretty-printed, binary as a hex dump).
fn render_detail(detail: &mut Detail, area: Rect, buf: &mut Buffer) {
    let cfg = config::get();
    let block = pane_block(" Message ", true);
    let inner = block.inner(area);
    Clear.render(area, buf);
    block.render(area, buf);

    let msg = &detail.msg;
    let size = if msg.truncated {
        format!(
            "{} stored · truncated (original {})",
            fmt_bytes(msg.payload.len() as u64),
            fmt_bytes(msg.size as u64)
        )
    } else {
        fmt_bytes(msg.size as u64)
    };
    let mut meta = vec![("channel", msg.channel.clone())];
    if let Some(pattern) = detail.pattern.as_ref() {
        meta.push(("pattern", pattern.clone()));
    }
    meta.push(("received", clock(msg.received_at)));
    meta.push(("size", size));
    let meta: Vec<Line<'static>> = meta
        .into_iter()
        .map(|(label, value)| {
            Line::from(vec![
                Span::styled(format!(" {label:<9}"), cfg.colors.base03),
                Span::styled(value, cfg.colors.base05),
            ])
        })
        .collect();

    let meta_height = u16::try_from(meta.len()).unwrap_or(u16::MAX);
    let [meta_area, rule_area, text_area] = Layout::vertical([
        Constraint::Length(meta_height),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(inner);
    Paragraph::new(meta).render(meta_area, buf);
    Paragraph::new(Line::styled(
        "─".repeat(usize::from(rule_area.width)),
        cfg.colors.base03,
    ))
    .render(rule_area, buf);

    let header = match msg.payload {
        Payload::Text(_) => "Payload",
        Payload::Binary(_) => "Payload · binary (hex)",
    };
    text_view::render(
        &detail.text,
        &TextLabels {
            header,
            paging: false,
            close: Some("y copy · Esc close"),
        },
        None,
        true,
        text_area,
        buf,
        &mut detail.view,
    );
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
    let preview = msg.payload.preview(msg.size);
    Line::from(vec![
        Span::styled(time, cfg.colors.base03),
        Span::styled(channel, cfg.colors.base0d),
        Span::styled(
            truncate_to_width(&preview, budget).into_owned(),
            cfg.colors.base05,
        ),
    ])
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
    use crate::redis_client::pubsub_store::{BufferLimits, MessageBuffer, ServerChannels};

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
        let area = Rect::new(0, 0, 100, 16);
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
        assert!(text.contains("kept 10"), "{text}");

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
    fn list_shows_unread_and_notify_markers() {
        let mut p = widget();
        {
            let mut store = p.store.lock();
            store.add(Target::Channel("chat".into()));
            store.add(Target::Channel("noisy".into()));
            store.add(Target::Channel("quiet".into()));
            store.subs[0].unread = 37;
            store.subs[0].notify = NotifyState::Off.toggled();
            store.subs[1].notify = NotifyState::AutoMuted;
        }
        let area = Rect::new(0, 0, 100, 10);
        let mut buf = Buffer::empty(area);
        PubSubWidget.render(area, &mut buf, &mut p);
        let lines: Vec<String> = screen(&buf).lines().map(str::to_owned).collect();
        let row = |name: &str| {
            lines
                .iter()
                .find(|l| l.contains(&format!("● {name}")))
                .cloned()
                .unwrap_or_default()
        };
        assert!(row("chat").contains("0/s (37) 🔔"), "{}", row("chat"));
        assert!(row("noisy").contains("0/s 🔕"), "{}", row("noisy"));
        assert!(!row("quiet").contains('('), "{}", row("quiet"));
        assert!(!row("quiet").contains('🔔'), "{}", row("quiet"));
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

    fn limits(messages: usize) -> BufferLimits {
        BufferLimits {
            messages,
            bytes: 1 << 20,
            payload: 64,
        }
    }

    fn fill(buf: &mut MessageBuffer, from: usize, to: usize) {
        for i in from..to {
            buf.push("c", format!("m{i}").as_bytes(), SystemTime::now());
        }
    }

    fn shown(
        cursor: &mut LogCursor,
        buf: &MessageBuffer,
        height: usize,
    ) -> (Vec<u64>, Option<u64>) {
        let (start, selected) = cursor.sync(buf, height);
        (
            buf.range(start, height).map(|m| m.seq).collect(),
            selected.and_then(|i| buf.get(i)).map(|m| m.seq),
        )
    }

    #[test]
    fn paused_view_keeps_its_rows_while_messages_arrive() {
        let mut buf = MessageBuffer::new(limits(1000));
        fill(&mut buf, 0, 10);
        let mut cursor = LogCursor::default();
        assert_eq!(shown(&mut cursor, &buf, 4), (vec![7, 8, 9, 10], Some(10)));

        cursor.toggle_pause(&buf);
        assert!(cursor.is_paused());
        fill(&mut buf, 10, 138);
        assert_eq!(shown(&mut cursor, &buf, 4), (vec![7, 8, 9, 10], Some(10)));
        assert_eq!(cursor.new_count(&buf), 128);
        assert_eq!(log_state(&cursor, &buf), "paused · +128 new");

        // Up moves the selection, the window follows it.
        for _ in 0..5 {
            cursor.up(&buf);
        }
        assert_eq!(shown(&mut cursor, &buf, 4), (vec![5, 6, 7, 8], Some(5)));

        cursor.toggle_pause(&buf);
        assert_eq!(log_state(&cursor, &buf), "following");
        assert_eq!(shown(&mut cursor, &buf, 2), (vec![137, 138], Some(138)));
    }

    #[test]
    fn up_pauses_and_down_past_the_end_follows() {
        let mut buf = MessageBuffer::new(limits(100));
        fill(&mut buf, 0, 5);
        let mut cursor = LogCursor::default();
        cursor.down(&buf);
        assert!(!cursor.is_paused(), "down while following does nothing");
        cursor.up(&buf);
        assert!(cursor.is_paused());
        assert_eq!(cursor.selected_seq(&buf), Some(4));
        cursor.down(&buf);
        assert_eq!(cursor.selected_seq(&buf), Some(5));
        assert!(cursor.is_paused());
        fill(&mut buf, 5, 6);
        cursor.down(&buf);
        assert_eq!(cursor.selected_seq(&buf), Some(6));
        cursor.down(&buf);
        assert!(!cursor.is_paused(), "past the newest row: following");

        // Nothing to pause on.
        let empty = MessageBuffer::new(limits(10));
        let mut cursor = LogCursor::default();
        cursor.toggle_pause(&empty);
        cursor.up(&empty);
        assert!(!cursor.is_paused());
    }

    #[test]
    fn eviction_moves_a_paused_view_to_the_oldest_row() {
        let mut buf = MessageBuffer::new(limits(5));
        fill(&mut buf, 0, 5);
        let mut cursor = LogCursor::default();
        cursor.sync(&buf, 3);
        cursor.up(&buf);
        assert_eq!(cursor.selected_seq(&buf), Some(4));
        fill(&mut buf, 5, 12);
        // Seqs 1..=7 are gone: the oldest kept is 8.
        assert_eq!(shown(&mut cursor, &buf, 3), (vec![8, 9, 10], Some(8)));
        assert!(cursor.evicted());
        assert_eq!(
            log_state(&cursor, &buf),
            "paused · +7 new · older messages evicted"
        );
        cursor.down(&buf);
        assert!(!cursor.evicted(), "moving clears the note");

        // Cleared (`c`): following again.
        buf.clear();
        assert_eq!(shown(&mut cursor, &buf, 3), (vec![], None));
        assert!(!cursor.is_paused());
    }

    fn pattern_widget() -> PubSub {
        let p = widget();
        {
            let mut store = p.store.lock();
            store.add(Target::Pattern("orders:*".into()));
            store.status = ConnStatus::Connected;
            for i in 0..12 {
                let channel = if i % 3 == 0 {
                    "orders:paid"
                } else {
                    "orders:new"
                };
                store.push(
                    channel,
                    Some("orders:*"),
                    format!(r#"{{"id":{i}}}"#).as_bytes(),
                    SystemTime::now(),
                );
            }
        }
        p
    }

    fn render(p: &mut PubSub) -> (Buffer, String) {
        let area = Rect::new(0, 0, 110, 24);
        let mut buf = Buffer::empty(area);
        PubSubWidget.render(area, &mut buf, p);
        let text = screen(&buf);
        (buf, text)
    }

    #[test]
    fn tab_moves_the_focus_style_to_the_log() {
        let mut p = pattern_widget();
        let colors = &config::get().colors;
        let (buf, _) = render(&mut p);
        // Top-left corners of the list and the log panes.
        assert_eq!(buf[(1, 1)].fg, colors.base0d);
        assert_eq!(buf[(1 + LIST_WIDTH, 1)].fg, colors.base03);

        p.toggle_focus();
        assert_eq!(p.focus(), Pane::Log);
        let (buf, _) = render(&mut p);
        assert_eq!(buf[(1, 1)].fg, colors.base03);
        assert_eq!(buf[(1 + LIST_WIDTH, 1)].fg, colors.base0d);
        p.toggle_focus();
        assert_eq!(p.focus(), Pane::List);

        // A server channel has no log to focus.
        p.store.lock().set_server_channels(server(&[("jobs", 1)]));
        p.select_next();
        p.toggle_focus();
        assert_eq!(p.focus(), Pane::List);
    }

    #[test]
    fn stats_panel_shows_totals_rates_and_top_channels() {
        let mut p = pattern_widget();
        let (_, text) = render(&mut p);
        assert!(text.contains("Stats"), "{text}");
        assert!(
            text.contains("12 msgs · 98 B · avg 8 B · max 9 B"),
            "{text}"
        );
        assert!(text.contains("/s 1 min · last "), "{text}");
        assert!(text.contains("kept 10 ("), "{text}");
        assert!(text.contains("top: orders:new 8 · orders:paid 4"), "{text}");

        // A channel subscription has no top line.
        let mut p = widget();
        p.store.lock().add(Target::Channel("chat".into()));
        let (_, text) = render(&mut p);
        assert!(text.contains("0 msgs"), "{text}");
        assert!(!text.contains("top:"), "{text}");
    }

    #[test]
    fn paused_title_counts_new_messages() {
        let mut p = pattern_widget();
        render(&mut p);
        p.toggle_pause();
        p.store
            .lock()
            .push("orders:new", Some("orders:*"), b"x", SystemTime::now());
        let (_, text) = render(&mut p);
        assert!(text.contains("── paused · +1 new"), "{text}");
    }

    #[test]
    fn detail_shows_metadata_and_pretty_json() {
        let mut p = pattern_widget();
        p.toggle_focus();
        render(&mut p);
        assert!(p.open_detail());
        let (_, text) = render(&mut p);
        for want in [
            "channel  orders:new",
            "pattern  orders:*",
            "received ",
            "size     9 B",
            "Payload · JSON",
            "\"id\": 11",
            "Esc close",
        ] {
            assert!(text.contains(want), "{want} missing:\n{text}");
        }
        assert!(!text.contains("truncated"), "{text}");
        assert_eq!(
            p.selected_message().map(|m| m.channel),
            Some("orders:new".into())
        );
        p.close_detail();
        assert!(!p.is_detail());
    }

    #[test]
    fn detail_of_a_cut_binary_payload() {
        let mut p = widget();
        {
            let mut store = p.store.lock();
            store.add(Target::Channel("bin".into()));
            // Over the 1024-byte payload limit of `widget()`.
            let mut bytes: Vec<u8> = (0..3000)
                .map(|i: u16| u8::try_from(i % 7).unwrap())
                .collect();
            bytes[0] = 0xff;
            store.push("bin", None, &bytes, SystemTime::now());
        }
        let (_, text) = render(&mut p);
        assert!(text.contains("[binary 3000 B] ff 01 02"), "{text}");
        p.toggle_focus();
        assert!(p.open_detail());
        let (_, text) = render(&mut p);
        assert!(!text.contains("pattern "), "{text}");
        assert!(
            text.contains("1.0 KiB stored · truncated (original 2.9 KiB)"),
            "{text}"
        );
        assert!(text.contains("Payload · binary (hex)"), "{text}");
        assert!(text.contains("00000000  ff 01 02 03 04 05 06 00"), "{text}");
    }

    #[test]
    fn markers_and_server_rows_open_no_detail() {
        let mut p = widget();
        {
            let mut store = p.store.lock();
            store.add(Target::Channel("chat".into()));
            store.mark_reconnected(&[Target::Channel("chat".into())], SystemTime::now());
        }
        assert!(!p.open_detail());
        assert_eq!(p.selected_message(), None);
    }

    #[test]
    fn another_row_or_clear_resets_the_log() {
        let mut p = pattern_widget();
        p.store.lock().add(Target::Channel("chat".into()));
        p.toggle_focus();
        p.toggle_pause();
        assert!(p.open_detail());
        p.select(1);
        assert!(!p.log_cursor().is_paused());
        assert!(!p.is_detail());
        p.select(0);
        p.toggle_pause();
        p.reset_log();
        assert!(!p.log_cursor().is_paused());
        // Removing the last subscription sends the focus back to the list.
        let mut store = p.store.lock();
        store.subs.clear();
        drop(store);
        p.clamp_selection();
        assert_eq!(p.focus(), Pane::List);
    }
}
