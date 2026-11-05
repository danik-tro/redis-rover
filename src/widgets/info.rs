use std::sync::Arc;

use parking_lot::Mutex;

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
};

#[allow(clippy::struct_field_names)]
pub struct Info {
    info: Arc<Mutex<Option<RedisInfo>>>,
    /// Credential-free connection target, e.g. `redis://localhost:6379`.
    target: String,
    /// Current DB of the keyspace browser.
    db: Arc<Mutex<i64>>,
    /// `--read-only`: show the `RO` badge.
    pub read_only: bool,
    /// Source of the CPU figure: a rate over the last sampling interval.
    metrics: Arc<Mutex<MetricsHistory>>,
    /// One-line key-hint legend; follows the current mode.
    pub hint: String,
    /// Unread Pub/Sub messages over all subscriptions; badge when > 0.
    /// The App keeps it at 0 while the Pub/Sub tab is open.
    pub pubsub_unread: u64,
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
    pub fn new(
        info: Arc<Mutex<Option<RedisInfo>>>,
        metrics: Arc<Mutex<MetricsHistory>>,
        db: Arc<Mutex<i64>>,
        target: String,
    ) -> Self {
        Self {
            info,
            target,
            db,
            read_only: false,
            metrics,
            hint: String::new(),
            pubsub_unread: 0,
        }
    }
}

pub struct InfoWidget;

impl StatefulWidget for InfoWidget {
    type State = Info;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let cfg = config::get();
        let mut title = vec![Span::raw(format!(" Info · {} ", state.target))];
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

        let common_info = { state.info.lock().as_ref().map(Clone::clone) };
        let db = *state.db.lock();
        let (cpu_pct, cluster) = {
            let metrics = state.metrics.lock();
            (
                metrics.latest.as_ref().and_then(|s| s.cpu_pct),
                metrics
                    .latest_snapshot
                    .as_ref()
                    .is_some_and(|s| s.cluster_enabled),
            )
        };
        let db_status = db_status(db, common_info.as_ref(), cluster);
        let cpu = cpu_pct.map_or_else(|| "CPU: -".to_string(), |v| format!("CPU: {v:.1} %"));

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

        // TODO: handle failed case
        let (os_info, memory, redis_info, clients) = if let Some(info) = common_info {
            (
                info.os(),
                info.memory(),
                info.redis_version(),
                info.clients(),
            )
        } else {
            ("-".into(), "-".into(), "-".into(), "-".into())
        };

        Paragraph::new(os_info)
            .alignment(Alignment::Left)
            .render(os_area, buf);
        Paragraph::new(redis_info)
            .alignment(Alignment::Left)
            .render(version_area, buf);

        Paragraph::new(memory)
            .alignment(Alignment::Right)
            .render(memory_area, buf);

        Paragraph::new(cpu)
            .alignment(Alignment::Right)
            .render(cpu_area, buf);

        Paragraph::new(clients)
            .alignment(Alignment::Center)
            .render(clients_area, buf);

        Paragraph::new(Line::from(db_status).fg(cfg.colors.base05))
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

    use super::*;

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
