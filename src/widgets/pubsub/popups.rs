use super::READ_ONLY_PUBLISH;

use ratatui::{
    buffer::Buffer,
    crossterm::event::{KeyCode, KeyEvent},
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget},
};
use ratatui_textarea::{CursorMove, TextArea};

use crate::{
    config,
    redis_client::pubsub_store::{detect_kind, TargetKind},
};

/// State of the add-subscription popup.
pub(super) struct AddPopup {
    pub(super) input: TextArea<'static>,
    /// Set once the user flips the kind with Tab; otherwise the kind follows
    /// the text (glob characters mean pattern).
    pub(super) kind_override: Option<TargetKind>,
}

impl AddPopup {
    pub(super) fn new() -> Self {
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

    pub(super) fn text(&self) -> String {
        self.input.lines().first().cloned().unwrap_or_default()
    }

    pub(super) fn kind(&self) -> TargetKind {
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
pub(super) struct PublishPopup {
    channel: TextArea<'static>,
    pub(super) payload: TextArea<'static>,
    focus: PublishField,
    /// `--read-only`: shown with a notice, sending is refused.
    pub(super) read_only: bool,
}

impl PublishPopup {
    pub(super) fn new(prefill: String, read_only: bool) -> Self {
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

    pub(super) fn channel(&self) -> String {
        self.channel.lines().first().cloned().unwrap_or_default()
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) {
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

pub(super) enum Popup {
    Add(Box<AddPopup>),
    Publish(Box<PublishPopup>),
}

pub(super) fn render_add_popup(popup: &AddPopup, area: Rect, buf: &mut Buffer) {
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

pub(super) fn render_publish_popup(popup: &PublishPopup, area: Rect, buf: &mut Buffer) {
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
    use super::*;
    use crossterm::event::KeyModifiers;
    use pretty_assertions::assert_eq;

    use ratatui::crossterm::event::{KeyCode, KeyEvent};

    use crate::redis_client::pubsub_store::{Target, TargetKind};
    use crate::widgets::pubsub::tests::{type_text, widget};

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
    fn kind_label_brackets_the_selected_kind() {
        assert_eq!(kind_labels(TargetKind::Channel), ("[channel]", "pattern"));
        assert_eq!(kind_labels(TargetKind::Pattern), ("channel", "[pattern]"));
    }
}
