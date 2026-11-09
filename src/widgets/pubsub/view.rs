use super::render::{pane_areas, PaneAreas};
use super::{row_at, LogCursor, Pane, PubSub, Row, RECONNECT_MARKER, TOP_CHANNELS};

use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    config,
    pubsub_notify::NotifyState,
    redis_client::pubsub_store::{
        ConnStatus, EntryKind, MessageBuffer, PubSubMessage, PubSubStore, Subscription,
    },
    utils::{fmt_bytes, group_thousands},
    widgets::keyspace::truncate_to_width,
};

/// One frame of the tab, built under one short lock of the store; part of
/// [`crate::state::FrameData`].
pub struct PubSubView {
    pub(super) has_sub: bool,
    pub(super) status: ConnStatus,
    pub(super) list: Vec<Line<'static>>,
    pub(super) log: LogView,
    pub(super) stats: Option<Vec<Line<'static>>>,
}

impl PubSub {
    /// Build this frame's rows for the tab drawn in `area`. Keeps the
    /// selection in range and moves the log cursor, as drawing did.
    pub fn build_view(&mut self, area: Rect) -> PubSubView {
        self.clamp_selection();
        let has_sub = self.selected_sub().is_some();
        let PaneAreas {
            list_inner,
            log_inner,
            stats_inner,
            ..
        } = pane_areas(area, has_sub);
        let log_focused = self.focus == Pane::Log;

        // Build everything under one short lock, render after releasing it.
        let now = SystemTime::now();
        let (status, list, log, stats_view) = {
            let store = self.store.lock();
            let row = row_at(&store, self.selected);
            let sub = match row {
                Some(Row::Sub(i)) => store.subs.get(i),
                _ => None,
            };
            (
                store.status.clone(),
                list_lines(&store, self.selected, !log_focused, list_inner, now),
                log_view(
                    &store,
                    row.as_ref(),
                    &mut self.log,
                    log_focused,
                    log_inner,
                    now,
                ),
                sub.map(|sub| stats_lines(sub, usize::from(stats_inner.width), now)),
            )
        };
        PubSubView {
            has_sub,
            status,
            list,
            log,
            stats: stats_view,
        }
    }
}

/// Lines of the left pane: own subscriptions (`● name  N/s`), then the
/// server's other channels (`○ name  N subs`), scrolled to keep the
/// selection visible.
pub(super) fn list_lines(
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

pub(super) struct LogView {
    pub(super) title: String,
    pub(super) lines: Vec<Line<'static>>,
    pub(super) placeholder: String,
}

/// The log of the selected subscription: the newest rows while following,
/// a frozen window while paused. The selected row is highlighted when the
/// log is focused or paused.
pub(super) fn log_view(
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
pub(super) fn log_state(cursor: &LogCursor, buf: &MessageBuffer) -> String {
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
pub(super) fn stats_lines(sub: &Subscription, width: usize, now: SystemTime) -> Vec<Line<'static>> {
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
pub(super) fn clock(t: SystemTime) -> String {
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

#[cfg(test)]
mod tests {

    use super::*;

    use pretty_assertions::assert_eq;
    use std::time::UNIX_EPOCH;

    use ratatui::{buffer::Buffer, layout::Rect};

    use crate::widgets::pubsub::tests::{draw, pattern_widget, render, screen, widget};
    use crate::{pubsub_notify::NotifyState, redis_client::pubsub_store::Target};

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
        draw(&mut p, area, &mut buf);
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
    fn utc_clock_parts() {
        assert_eq!(utc_hms(0), (0, 0, 0));
        assert_eq!(utc_hms(86_400 + 3_723), (1, 2, 3));
        let t = UNIX_EPOCH + std::time::Duration::from_millis(5_042);
        assert!(clock(t).ends_with(".042"));
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
}
