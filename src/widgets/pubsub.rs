//! The Pub/Sub tab: own subscriptions on the left, the selected one's live log
//! on the right, and the add-subscription popup.

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
use ratatui_textarea::TextArea;
use unicode_width::UnicodeWidthStr;

use crate::{
    config,
    redis_client::pubsub_store::{
        detect_kind, ConnStatus, Payload, PubSubMessage, PubSubStore, Target, TargetKind,
    },
    widgets::keyspace::{format_bytes, truncate_to_width},
};

/// Bytes shown in a binary payload's hex preview.
const HEX_PREVIEW: usize = 16;
/// Width of the subscriptions pane.
const LIST_WIDTH: u16 = 32;

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

pub struct PubSub {
    store: Arc<Mutex<PubSubStore>>,
    /// Index into `PubSubStore::subs`.
    selected: usize,
    add_popup: Option<AddPopup>,
}

impl PubSub {
    pub fn new(store: Arc<Mutex<PubSubStore>>) -> Self {
        Self {
            store,
            selected: 0,
            add_popup: None,
        }
    }

    pub fn select_next(&mut self) {
        let len = self.store.lock().subs.len();
        if self.selected + 1 < len {
            self.selected += 1;
        }
    }

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn select(&mut self, index: usize) {
        self.selected = index;
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Target of the selected subscription.
    pub fn selected_target(&self) -> Option<Target> {
        self.store
            .lock()
            .subs
            .get(self.selected)
            .map(|s| s.target.clone())
    }

    /// Keep the selection on an existing row after a removal.
    pub fn clamp_selection(&mut self) {
        let len = self.store.lock().subs.len();
        self.selected = self.selected.min(len.saturating_sub(1));
    }

    pub fn open_add(&mut self) {
        self.add_popup = Some(AddPopup::new());
    }

    pub fn is_popup(&self) -> bool {
        self.add_popup.is_some()
    }

    pub fn close_popup(&mut self) {
        self.add_popup = None;
    }

    /// A key typed into the add popup (Enter / Esc are handled by the App).
    /// Tab flips the channel / pattern kind.
    pub fn handle_popup_key(&mut self, key: KeyEvent) {
        let Some(popup) = self.add_popup.as_mut() else {
            return;
        };
        if key.code == KeyCode::Tab || key.code == KeyCode::BackTab {
            popup.kind_override = Some(popup.kind().flipped());
            return;
        }
        popup.input.input(key);
    }

    /// Close the add popup and return the target typed, if any. Surrounding
    /// whitespace is dropped.
    pub fn take_add_request(&mut self) -> Option<Target> {
        let popup = self.add_popup.take()?;
        let kind = popup.kind();
        let name = popup.text().trim().to_owned();
        (!name.is_empty()).then(|| Target::new(kind, name))
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
            (
                store.status.clone(),
                list_lines(&store, state.selected, list_inner, now),
                log_view(&store, state.selected, log_inner, now),
            )
        };

        outer
            .title(Line::from(vec![
                Span::raw(" Pub/Sub ── "),
                Span::styled(status.to_string(), status_style(&status)),
                Span::raw(" "),
            ]))
            .render(area, buf);

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

        if let Some(popup) = state.add_popup.as_ref() {
            render_add_popup(popup, area, buf);
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
        ConnStatus::Disconnected(_) => Style::new().fg(cfg.colors.base08),
        ConnStatus::NotStarted => Style::new().fg(cfg.colors.base03),
    }
}

/// Rows of the subscriptions pane: `● name  N/s`, scrolled to keep the
/// selection visible.
fn list_lines(
    store: &PubSubStore,
    selected: usize,
    area: Rect,
    now: SystemTime,
) -> Vec<Line<'static>> {
    let cfg = config::get();
    if store.subs.is_empty() {
        return vec![Line::styled(" a add subscription", cfg.colors.base03)];
    }
    let height = usize::from(area.height).max(1);
    let width = usize::from(area.width);
    let offset = selected.saturating_sub(height - 1);
    store
        .subs
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(i, sub)| {
            let rate = format!(" {}/s", sub.stats.rate_1s(now));
            let name_width = width.saturating_sub(3 + rate.width());
            let name = truncate_to_width(sub.target.name(), name_width);
            let pad = name_width.saturating_sub(name.width());
            let dot = if sub.target.is_pattern() {
                cfg.colors.base0e
            } else {
                cfg.colors.base0d
            };
            let mut line = Line::from(vec![
                Span::styled(" ● ", dot),
                Span::raw(format!("{name}{}", " ".repeat(pad))),
                Span::styled(rate, cfg.colors.base04),
            ]);
            if i == selected {
                line = line.style(
                    Style::new()
                        .bg(cfg.colors.base02)
                        .add_modifier(Modifier::BOLD),
                );
            }
            line
        })
        .collect()
}

struct LogView {
    title: String,
    footer: Option<String>,
    lines: Vec<Line<'static>>,
    placeholder: String,
}

/// The log of the selected subscription, following the newest message.
fn log_view(store: &PubSubStore, selected: usize, area: Rect, now: SystemTime) -> LogView {
    let Some(sub) = store.subs.get(selected) else {
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

/// `HH:MM:SS.mmm channel payload`, cut to `width` columns.
fn log_line(msg: &PubSubMessage, width: usize) -> Line<'static> {
    let cfg = config::get();
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
    use crate::redis_client::pubsub_store::BufferLimits;

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
        assert_eq!(p.add_popup.as_ref().unwrap().kind(), TargetKind::Pattern);
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
        assert_eq!(p.selected(), 0);
        {
            let mut store = p.store.lock();
            store.add(Target::Channel("a".into()));
            store.add(Target::Channel("b".into()));
        }
        p.select_next();
        p.select_next();
        assert_eq!(p.selected(), 1);
        p.store.lock().remove(&Target::Channel("b".into()));
        p.clamp_selection();
        assert_eq!(p.selected(), 0);
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
