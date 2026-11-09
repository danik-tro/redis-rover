use ratatui::buffer::Buffer;
use ratatui::layout::{self, Alignment, Constraint, Layout, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, StatefulWidget, Widget};

use crate::{
    command::Command,
    config,
    keybindings::KeyBindings,
    mode::Mode,
    redis_client::{metrics::MetricsHistory, types::RedisInfo},
    state::FrameData,
};

pub struct Info {
    /// ` Info · redis://localhost:6379 `: the credential-free target.
    title: String,
    /// `--read-only`: show the `RO` badge.
    pub read_only: bool,
    /// One-line key-hint legend; follows the current mode.
    pub hint: String,
    /// Unread Pub/Sub messages over all subscriptions; badge when > 0.
    /// The App keeps it at 0 while the Pub/Sub tab is open.
    pub pubsub_unread: u64,
}

/// The footer's text, built once per `INFO` sample (#52) rather than on
/// every frame. Lives in [`FrameData`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FooterText {
    /// `(info_seq, db)` this was built for; `None` until the first build.
    key: Option<(u64, i64)>,
    version: String,
    os: String,
    memory: String,
    clients: String,
    db_status: String,
    cpu: String,
}

impl FooterText {
    pub fn build(key: (u64, i64), info: Option<&RedisInfo>, metrics: &MetricsHistory) -> Self {
        let cpu_pct = metrics.latest.as_ref().and_then(|s| s.cpu_pct);
        let cluster = metrics
            .latest_snapshot
            .as_ref()
            .is_some_and(|s| s.cluster_enabled);
        let (version, os, memory, clients) = info.map_or_else(
            || ("-".into(), "-".into(), "-".into(), "-".into()),
            |info| {
                (
                    info.redis_version(),
                    info.os(),
                    info.memory(),
                    info.clients(),
                )
            },
        );
        Self {
            key: Some(key),
            version,
            os,
            memory,
            clients,
            db_status: db_status(key.1, info, cluster),
            cpu: cpu_pct.map_or_else(|| "CPU: -".to_string(), |v| format!("CPU: {v:.1} %")),
        }
    }

    /// `(info_seq, db)` this was built for; `None` until the first build.
    pub fn key(&self) -> Option<(u64, i64)> {
        self.key
    }
}

/// Build the footer legend from the live keymap: `"{key} {label}"` per entry,
/// using the shortest binding of `mode` (falling back to [`Mode::Common`]) and
/// skipping unbound commands.
pub fn build_hint(keybindings: &KeyBindings, mode: Mode, entries: &[(Command, &str)]) -> String {
    entries
        .iter()
        .filter_map(|(cmd, label)| {
            [mode, Mode::Common]
                .into_iter()
                .find_map(|m| {
                    keybindings
                        .get_config_for_command(m, *cmd)
                        .into_iter()
                        .min_by(|a, b| a.chars().count().cmp(&b.chars().count()).then(a.cmp(b)))
                })
                .map(|key| format!("{key} {label}"))
        })
        .collect::<Vec<_>>()
        .join("  ")
}

/// `db 3 · 120 keys`, or `db 3` before the first `INFO` arrives. In cluster
/// mode `INFO` only counts the connected node's keys, so that is spelled out.
fn db_status(db: i64, info: Option<&RedisInfo>, cluster: bool) -> String {
    match info {
        Some(info) => {
            let keys = info.db_keys(db);
            let noun = if keys == 1 { "key" } else { "keys" };
            let scope = if cluster { " (this node)" } else { "" };
            format!("db {db} · {keys} {noun}{scope}")
        }
        None => format!("db {db}"),
    }
}

impl Info {
    /// `target` is the credential-free connection target, e.g.
    /// `redis://localhost:6379`.
    #[must_use]
    pub fn new(target: &str) -> Self {
        Self {
            title: format!(" Info · {target} "),
            read_only: false,
            hint: String::new(),
            pubsub_unread: 0,
        }
    }
}

/// Draws the footer from the frame's [`FooterText`].
pub struct InfoWidget<'a> {
    pub frame: &'a FrameData,
}

impl StatefulWidget for InfoWidget<'_> {
    type State = Info;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let cfg = config::get();
        let mut title = vec![Span::raw(state.title.as_str())];
        if state.read_only {
            title.push(
                Span::raw(" RO ")
                    .fg(cfg.colors.base00)
                    .bg(cfg.colors.base08)
                    .bold(),
            );
            title.push(Span::raw(" "));
        }
        if state.pubsub_unread > 0 {
            title.push(
                Span::raw(format!(" PubSub {} ", state.pubsub_unread))
                    .fg(cfg.colors.base00)
                    .bg(cfg.colors.base0e)
                    .bold(),
            );
            title.push(Span::raw(" "));
        }
        Block::new()
            .fg(cfg.colors.base04)
            .bg(cfg.colors.base00)
            .borders(Borders::ALL)
            .title(Line::from(title))
            .title_alignment(Alignment::Left)
            .title_top(Line::from(" ? Help ").right_aligned())
            .border_type(BorderType::Rounded)
            .render(area, buf);

        let text = &self.frame.footer;

        // Reserve the last inner row for the always-on key-hint legend (#41).
        let [top, bottom, hint] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .flex(layout::Flex::Center)
        .areas(area);

        let [version_area, db_area, cpu_area] =
            Layout::horizontal([Constraint::Min(0), Constraint::Fill(1), Constraint::Min(0)])
                .flex(layout::Flex::Center)
                .horizontal_margin(3)
                .areas(top);

        let [os_area, clients_area, memory_area] =
            Layout::horizontal([Constraint::Min(0), Constraint::Fill(1), Constraint::Min(0)])
                .flex(layout::Flex::Center)
                .horizontal_margin(3)
                .areas(bottom);

        Paragraph::new(text.os.as_str())
            .alignment(Alignment::Left)
            .render(os_area, buf);
        Paragraph::new(text.version.as_str())
            .alignment(Alignment::Left)
            .render(version_area, buf);

        Paragraph::new(text.memory.as_str())
            .alignment(Alignment::Right)
            .render(memory_area, buf);

        Paragraph::new(text.cpu.as_str())
            .alignment(Alignment::Right)
            .render(cpu_area, buf);

        Paragraph::new(text.clients.as_str())
            .alignment(Alignment::Center)
            .render(clients_area, buf);

        Paragraph::new(Line::from(text.db_status.as_str()).fg(cfg.colors.base05))
            .alignment(Alignment::Center)
            .render(db_area, buf);

        // One-line key-hint legend (#41) so the common actions are discoverable
        // without opening the full `?` help overlay.
        Paragraph::new(Line::styled(state.hint.as_str(), cfg.colors.base04))
            .alignment(Alignment::Center)
            .render(hint, buf);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::state::SharedState;

    #[test]
    fn db_status_shows_key_count() {
        let mut map = HashMap::new();
        map.insert("db3", "keys=1,expires=0,avg_ttl=0");
        map.insert("db0", "keys=40,expires=0,avg_ttl=0");
        let info = RedisInfo::from(map);
        assert_eq!(db_status(3, Some(&info), false), "db 3 · 1 key");
        assert_eq!(db_status(0, Some(&info), false), "db 0 · 40 keys");
        assert_eq!(db_status(5, Some(&info), false), "db 5 · 0 keys");
        assert_eq!(db_status(5, None, false), "db 5");
        assert_eq!(
            db_status(0, Some(&info), true),
            "db 0 · 40 keys (this node)"
        );
    }

    fn sample_info() -> RedisInfo {
        RedisInfo::from(HashMap::from([
            ("redis_version", "7.2.4"),
            ("os", "Linux 6.8.0-1012-aws x86_64"),
            ("used_memory_human", "1.53M"),
            ("total_system_memory_human", "15.47G"),
            ("connected_clients", "3"),
            ("maxclients", "10000"),
            ("db0", "keys=1234,expires=0,avg_ttl=0"),
        ]))
    }

    fn footer(info: Option<RedisInfo>, width: u16) -> Vec<String> {
        let shared = SharedState::default();
        *shared.info.lock() = info;
        let mut state = Info::new("redis://localhost:6379");
        state.read_only = true;
        state.pubsub_unread = 3;
        state.hint = "j/k move  / filter  ? help".into();
        draw(&mut state, &mut FrameData::default(), &shared, width)
    }

    /// One frame: refresh the footer text as `App::draw` does, then render.
    fn draw(
        state: &mut Info,
        frame: &mut FrameData,
        shared: &SharedState,
        width: u16,
    ) -> Vec<String> {
        frame.refresh_footer(shared);
        let area = Rect::new(0, 0, width, 5);
        let mut buf = Buffer::empty(area);
        InfoWidget { frame }.render(area, &mut buf, state);
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    /// Pins the footer text, so caching it per INFO sample (#52) can't change
    /// what is drawn.
    #[test]
    fn footer_renders_same_text() {
        let cases: [(u16, bool, [&str; 5]); 4] = [
            (
                80,
                true,
            [
                "╭ Info · redis://localhost:6379  RO   PubSub 3  ─────────────────────── ? Help ╮",
                "│  Redis version: 7.2.4         db 0 · 1234 keys                       CPU: -  │",
                "│  OS: Linux 6.8.0-1012-aws Connected clients: 3/100        RAM: 1.53M/15.47G  │",
                "│                          j/k move  / filter  ? help                          │",
                "╰──────────────────────────────────────────────────────────────────────────────╯",
            ],
            ),
            (
                120,
                true,
            [
                "╭ Info · redis://localhost:6379  RO   PubSub 3  ─────────────────────────────────────────────────────────────── ? Help ╮",
                "│  Redis version: 7.2.4                             db 0 · 1234 keys                                           CPU: -  │",
                "│  OS: Linux 6.8.0-1012-aws x86_64             Connected clients: 3/10000                           RAM: 1.53M/15.47G  │",
                "│                                              j/k move  / filter  ? help                                              │",
                "╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯",
            ],
            ),
            (
                80,
                false,
            [
                "╭ Info · redis://localhost:6379  RO   PubSub 3  ─────────────────────── ? Help ╮",
                "│  -                                  db 0                             CPU: -  │",
                "│  -                                    -                                   -  │",
                "│                          j/k move  / filter  ? help                          │",
                "╰──────────────────────────────────────────────────────────────────────────────╯",
            ],
            ),
            (
                120,
                false,
            [
                "╭ Info · redis://localhost:6379  RO   PubSub 3  ─────────────────────────────────────────────────────────────── ? Help ╮",
                "│  -                                                      db 0                                                 CPU: -  │",
                "│  -                                                        -                                                       -  │",
                "│                                              j/k move  / filter  ? help                                              │",
                "╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯",
            ],
            ),
        ];
        for (width, info, expected) in cases {
            assert_eq!(
                footer(info.then(sample_info), width),
                expected,
                "{width} columns, info: {info}"
            );
        }
    }

    #[test]
    fn footer_text_rebuilds_only_on_new_sample_or_db() {
        let shared = SharedState::default();
        *shared.info.lock() = Some(sample_info());
        let mut state = Info::new("redis://localhost:6379");
        let mut frame = FrameData::default();
        draw(&mut state, &mut frame, &shared, 120);
        let first = frame.footer.version.clone();
        assert_eq!(first, "Redis version: 7.2.4");

        // Info changed but no new sample was announced: the cached text stays.
        let mut newer = sample_info();
        newer.common.version = "7.4.0".into();
        *shared.info.lock() = Some(newer);
        draw(&mut state, &mut frame, &shared, 120);
        assert_eq!(frame.footer.version, first);

        shared.info_seq.fetch_add(1, Ordering::Release);
        let lines = draw(&mut state, &mut frame, &shared, 120);
        assert_eq!(frame.footer.version, "Redis version: 7.4.0");
        assert!(lines[1].contains("Redis version: 7.4.0"));

        // A DB switch rebuilds it too, without a new sample.
        *shared.db.lock() = 3;
        let lines = draw(&mut state, &mut frame, &shared, 120);
        assert_eq!(frame.footer.db_status, "db 3 · 0 keys");
        assert!(lines[1].contains("db 3 · 0 keys"));
    }

    #[test]
    fn hint_follows_rebound_key_and_skips_unbound() {
        let key = |c| vec![KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)];
        let mut dash = HashMap::new();
        dash.insert(key('x'), Command::ResetMetrics);
        dash.insert(key('m'), Command::ToggleDashboard);
        let mut common = HashMap::new();
        common.insert(key('?'), Command::ToggleHelp);
        let kb = KeyBindings(HashMap::from([
            (Mode::Dashboard, dash),
            (Mode::Common, common),
        ]));
        let entries = [
            (Command::ToggleDashboard, "back"),
            (Command::ResetMetrics, "reset"),
            (Command::NextChart, "chart"),
            (Command::ToggleHelp, "help"),
        ];
        assert_eq!(
            build_hint(&kb, Mode::Dashboard, &entries),
            "m back  x reset  ? help"
        );
    }
}
