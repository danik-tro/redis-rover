//! The Pub/Sub tab: own subscriptions and the server's channels on the left,
//! the selected subscription's live log on the right, and the add / publish
//! popups.

mod cursor;
mod popups;
mod render;
mod view;

use cursor::{Detail, LogCursor};
use popups::{render_add_popup, render_publish_popup, AddPopup, Popup, PublishPopup};
pub use render::PubSubWidget;
use view::{clock, list_lines, log_view, stats_lines};

use std::sync::Arc;

use parking_lot::Mutex;
use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::redis_client::pubsub_store::{
    EntryKind, MessageBuffer, PubSubMessage, PubSubStore, Target,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use pretty_assertions::assert_eq;
    use std::{sync::Arc, time::SystemTime};

    use parking_lot::Mutex;
    use ratatui::{
        buffer::Buffer,
        crossterm::event::{KeyCode, KeyEvent},
        layout::Rect,
        widgets::StatefulWidget,
    };

    use crate::redis_client::pubsub_store::{BufferLimits, ServerChannels};
    use crate::redis_client::pubsub_store::{ConnStatus, MessageBuffer, PubSubStore, Target};

    pub(super) fn widget() -> PubSub {
        PubSub::new(Arc::new(Mutex::new(PubSubStore::new(BufferLimits {
            messages: 10,
            bytes: 1024,
            payload: 1024,
        }))))
    }

    pub(super) fn type_text(pubsub: &mut PubSub, text: &str) {
        for c in text.chars() {
            pubsub.handle_popup_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty()));
        }
    }

    pub(super) fn screen(buf: &Buffer) -> String {
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

    pub(super) fn server(channels: &[(&str, u64)]) -> ServerChannels {
        ServerChannels {
            channels: channels.iter().map(|(n, c)| ((*n).into(), *c)).collect(),
            total: channels.len(),
            patterns: 3,
        }
    }

    pub(super) fn limits(messages: usize) -> BufferLimits {
        BufferLimits {
            messages,
            bytes: 1 << 20,
            payload: 64,
        }
    }

    pub(super) fn fill(buf: &mut MessageBuffer, from: usize, to: usize) {
        for i in from..to {
            buf.push("c", format!("m{i}").as_bytes(), SystemTime::now());
        }
    }

    pub(super) fn shown(
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

    pub(super) fn pattern_widget() -> PubSub {
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

    pub(super) fn render(p: &mut PubSub) -> (Buffer, String) {
        let area = Rect::new(0, 0, 110, 24);
        let mut buf = Buffer::empty(area);
        PubSubWidget.render(area, &mut buf, p);
        let text = screen(&buf);
        (buf, text)
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
