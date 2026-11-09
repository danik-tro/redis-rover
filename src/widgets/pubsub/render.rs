use super::{
    clock, render_add_popup, render_publish_popup, Detail, Pane, Popup, PubSub, PubSubView,
    CLUSTER_NOTE, LIST_WIDTH, STATS_HEIGHT, STATS_MIN_PANE,
};

use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Flex, Layout, Rect},
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, StatefulWidget, Widget, Wrap},
};

use crate::{
    config,
    redis_client::pubsub_store::{ConnStatus, Payload},
    utils::fmt_bytes,
    widgets::text_view::{self, TextLabels},
};

/// Draws the Pub/Sub tab from the frame's [`PubSubView`], built by
/// [`PubSub::build_view`] for the same area.
pub struct PubSubWidget {
    pub view: PubSubView,
}

/// The tab's panes: the list, the right side (log and stats, or the message
/// detail over both), and the inner areas their rows are built for.
pub(super) struct PaneAreas {
    pub(super) list_area: Rect,
    pub(super) right_area: Rect,
    pub(super) log_area: Rect,
    pub(super) stats_area: Rect,
    pub(super) list_inner: Rect,
    pub(super) log_inner: Rect,
    pub(super) stats_inner: Rect,
}

fn outer_border() -> Block<'static> {
    let cfg = config::get();
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(cfg.colors.base04)
        .bg(cfg.colors.base00)
        .fg(cfg.colors.base05)
}

/// Pane layout of `area`; the stats panel only shows for a subscription.
pub(super) fn pane_areas(area: Rect, has_sub: bool) -> PaneAreas {
    let inner = outer_border().inner(area);

    let [list_area, right_area] =
        Layout::horizontal([Constraint::Length(LIST_WIDTH), Constraint::Fill(1)]).areas(inner);
    let list_inner = pane_block("", false).inner(list_area);
    let [log_area, stats_area] = if has_sub && right_area.height >= STATS_MIN_PANE {
        Layout::vertical([Constraint::Fill(1), Constraint::Length(STATS_HEIGHT)]).areas(right_area)
    } else {
        [right_area, Rect::default()]
    };
    let log_inner = pane_block("", false).inner(log_area);
    let stats_inner = pane_block("", false).inner(stats_area);
    PaneAreas {
        list_area,
        right_area,
        log_area,
        stats_area,
        list_inner,
        log_inner,
        stats_inner,
    }
}

impl StatefulWidget for PubSubWidget {
    type State = PubSub;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let cfg = config::get();
        let PubSubView {
            has_sub,
            status,
            list,
            log,
            stats: stats_view,
        } = self.view;

        let outer = outer_border();
        let PaneAreas {
            list_area,
            right_area,
            log_area,
            stats_area,
            log_inner,
            ..
        } = pane_areas(area, has_sub);
        let log_focused = state.focus == Pane::Log;

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

#[cfg(test)]
mod tests {

    use super::*;
    use crate::widgets::pubsub::{READ_ONLY_PUBLISH, RECONNECT_MARKER};
    use pretty_assertions::assert_eq;
    use std::time::SystemTime;

    use ratatui::{buffer::Buffer, layout::Rect};

    use crate::widgets::pubsub::tests::{draw, pattern_widget, render, screen, server, widget};
    use crate::{
        config,
        redis_client::pubsub_store::{ConnStatus, Target},
    };

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
        draw(&mut p, area, &mut buf);
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
        draw(&mut p, area, &mut buf);
        assert!(screen(&buf).contains("[channel] / pattern"));
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
        draw(&mut p, area, &mut buf);
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
        draw(&mut p, area, &mut buf);
        assert!(
            screen(&buf).contains("1 subscriber · Enter to subscribe"),
            "{}",
            screen(&buf)
        );

        p.open_publish(true);
        draw(&mut p, area, &mut buf);
        let text = screen(&buf);
        assert!(text.contains(READ_ONLY_PUBLISH), "{text}");
        assert!(text.contains("events"), "prefilled: {text}");
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
}
