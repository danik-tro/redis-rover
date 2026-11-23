use super::App;

use std::{
    sync::atomic::Ordering,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::{config, hit_map::HitTarget, mode::PopupMode};
use color_eyre::eyre::Result;
use ratatui::prelude::Rect;
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Flex, Layout},
    style::Stylize,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, StatefulWidget, Widget},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    command::Command,
    mode::Mode,
    tui,
    widgets::{
        dashboard::DashboardWidget,
        db_picker::DbPickerWidget,
        info::{build_hint, InfoWidget},
        keyspace::KeySpaceWidget,
        monitor::MonitorWidget,
        pubsub::{PubSubView, PubSubWidget},
        slowlog::SlowlogWidget,
        toast::ToastWidget,
    },
};

pub struct AppWidget;

/// How often `frames_drawn` is logged.
const FRAMES_LOG_INTERVAL: Duration = Duration::from_mins(1);
/// Least time between frames drawn only because the footer text changed.
const FOOTER_REDRAW_INTERVAL: Duration = Duration::from_secs(10);

/// Whole seconds since the Unix epoch (0 before it).
fn wall_sec(wall: SystemTime) -> u64 {
    wall.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Keyspace hint entries that write; hidden in `--read-only` mode.
const WRITE_COMMANDS: &[Command] = &[
    Command::AddKey,
    Command::AddItem,
    Command::EditValue,
    Command::DeleteKey,
    Command::SetTtl,
    Command::Rename,
    Command::CopyKey,
    Command::EditExternal,
    Command::PubSubPublish,
    Command::SlowlogReset,
];

/// Footer hint entries for the keyspace view, in display order.
const KEYSPACE_HINT_ENTRIES: &[(Command, &str)] = &[
    (Command::AddKey, "add"),
    (Command::AddItem, "add-item"),
    (Command::EditValue, "edit"),
    (Command::DeleteKey, "del"),
    (Command::SetTtl, "ttl"),
    (Command::EnterValue, "open"),
    (Command::YankValue, "yank"),
    (Command::SetPattern, "filter"),
    (Command::SelectDb, "db"),
    (Command::ToggleHelp, "help"),
];

/// Footer hint entries for the dashboard view, in display order.
const DASHBOARD_HINT_ENTRIES: &[(Command, &str)] = &[
    (Command::ToggleDashboard, "back"),
    (Command::ToggleMetricsPause, "pause"),
    (Command::ResetMetrics, "reset"),
    (Command::NextChart, "chart"),
    (Command::ToggleAllCharts, "all"),
    (Command::TogglePubSub, "pubsub"),
    (Command::ToggleHelp, "help"),
];

/// Footer hint entries for the Pub/Sub tab, in display order. Kept short:
/// remove, clear and refresh are in the `?` help.
const PUBSUB_HINT_ENTRIES: &[(Command, &str)] = &[
    (Command::TogglePubSub, "back"),
    (Command::PubSubAdd, "add"),
    (Command::PubSubPublish, "publish"),
    (Command::PubSubToggleNotify, "notify"),
    (Command::PubSubFocus, "focus"),
    (Command::PubSubTogglePause, "pause"),
    (Command::PubSubEnter, "detail"),
    (Command::YankValue, "yank"),
    (Command::YankKey, "channel"),
    (Command::ToggleHelp, "help"),
];

/// Footer hint entries for the MONITOR screen, in display order.
const MONITOR_HINT_ENTRIES: &[(Command, &str)] = &[
    (Command::LeaveScreen, "back"),
    (Command::MonitorToggle, "start/stop"),
    (Command::MonitorFilter, "filter"),
    (Command::MonitorTogglePause, "pause"),
    (Command::MonitorClear, "clear"),
    (Command::MonitorEnter, "detail"),
    (Command::ToggleHelp, "help"),
];

/// Footer hint entries for the Slowlog screen, in display order.
const SLOWLOG_HINT_ENTRIES: &[(Command, &str)] = &[
    (Command::LeaveScreen, "back"),
    (Command::SlowlogEnter, "detail"),
    (Command::SlowlogSort, "sort"),
    (Command::SlowlogRefresh, "refresh"),
    (Command::SlowlogReset, "reset"),
    (Command::ToggleHelp, "help"),
];

/// `?` help rows for the `g` jumps, shown on every top-level screen.
const GO_TO_HELP_ENTRIES: &[(Command, &str)] = &[
    (Command::GoToMonitor, "Go to MONITOR"),
    (Command::GoToSlowlog, "Go to Slowlog"),
    (Command::GoToPubSub, "Go to Pub/Sub"),
    (Command::GoToDashboard, "Go to metrics dashboard"),
];

/// `?` help rows for the MONITOR screen, in display order.
const MONITOR_HELP_ENTRIES: &[(Command, &str)] = &[
    (Command::LeaveScreen, "Back (Esc closes the detail first)"),
    (Command::MonitorToggle, "Start / stop MONITOR"),
    (Command::MonitorFilter, "Filter by command or key glob"),
    (Command::MonitorTogglePause, "Pause / follow the log"),
    (Command::MonitorClear, "Clear the log"),
    (Command::MonitorEnter, "Open the selected line"),
    (Command::NextNode, "Next cluster node"),
    (Command::PrevNode, "Previous cluster node"),
    (Command::ScrollDown, "Down (log or detail)"),
    (Command::ScrollUp, "Up (log or detail; pauses the log)"),
    (Command::ToggleWrap, "Wrap the detail on / off"),
    (Command::ScrollLeft, "Pan detail left (wrap off)"),
    (Command::ScrollRight, "Pan detail right (wrap off)"),
    (Command::ToggleReadOnly, "Toggle read-only"),
    (Command::ToggleHelp, "Toggle this help"),
    (Command::Quit, "Quit"),
];

/// `?` help rows for the Slowlog screen, in display order.
const SLOWLOG_HELP_ENTRIES: &[(Command, &str)] = &[
    (Command::LeaveScreen, "Back (Esc closes the detail first)"),
    (Command::SlowlogEnter, "Open the entry's full arguments"),
    (Command::SlowlogSort, "Sort: newest / slowest first"),
    (Command::SlowlogRefresh, "Load it again"),
    (Command::SlowlogReset, "SLOWLOG RESET (shows a preview)"),
    (Command::NextNode, "Next cluster node"),
    (Command::PrevNode, "Previous cluster node"),
    (Command::ScrollDown, "Down (table or detail)"),
    (Command::ScrollUp, "Up (table or detail)"),
    (Command::ToggleWrap, "Wrap the detail on / off"),
    (Command::ScrollLeft, "Pan detail left (wrap off)"),
    (Command::ScrollRight, "Pan detail right (wrap off)"),
    (Command::ToggleHelp, "Toggle this help"),
    (Command::Quit, "Quit"),
];

/// `?` help rows for the Pub/Sub tab, in display order.
const PUBSUB_HELP_ENTRIES: &[(Command, &str)] = &[
    (Command::TogglePubSub, "Back (Esc closes the detail first)"),
    (Command::ScrollDown, "Down (list, log or detail)"),
    (
        Command::ScrollUp,
        "Up (list, log or detail; pauses the log)",
    ),
    (Command::PubSubFocus, "Switch focus: list / log"),
    (Command::PubSubTogglePause, "Pause / follow the log"),
    (
        Command::PubSubEnter,
        "Subscribe to server channel / open message",
    ),
    (Command::YankValue, "Copy message payload"),
    (Command::YankKey, "Copy message channel"),
    (Command::ToggleWrap, "Wrap message detail on / off"),
    (Command::ScrollLeft, "Pan detail left (wrap off)"),
    (Command::ScrollRight, "Pan detail right (wrap off)"),
    (Command::PubSubAdd, "Add subscription (Tab: kind)"),
    (Command::PubSubRemove, "Unsubscribe and drop messages"),
    (Command::PubSubClear, "Clear messages and stats"),
    (Command::PubSubPublish, "Publish (Ctrl-t: SPUBLISH)"),
    (
        Command::PubSubToggleNotify,
        "Toggle notifications (also un-mutes)",
    ),
    (Command::PubSubRefresh, "Refresh server channels"),
    (Command::ToggleReadOnly, "Toggle read-only"),
    (Command::ToggleHelp, "Toggle this help"),
    (Command::Quit, "Quit"),
];

/// `?` help rows for the dashboard, in display order.
const DASHBOARD_HELP_ENTRIES: &[(Command, &str)] = &[
    (Command::ToggleDashboard, "Back to keyspace"),
    (Command::ToggleMetricsPause, "Pause / resume view"),
    (Command::ResetMetrics, "Reset history"),
    (Command::NextChart, "Next chart"),
    (Command::PrevChart, "Previous chart"),
    (Command::NextNode, "Next node (one server: next chart)"),
    (
        Command::PrevNode,
        "Previous node (one server: previous chart)",
    ),
    (Command::ToggleAllCharts, "Toggle all charts"),
    (Command::TogglePubSub, "Open Pub/Sub"),
    (Command::ToggleReadOnly, "Toggle read-only"),
    (Command::ToggleHelp, "Toggle this help"),
    (Command::Quit, "Quit"),
];

/// `?` help rows for the keyspace view, in display order.
const KEYSPACE_HELP_ENTRIES: &[(Command, &str)] = &[
    (Command::ScrollDown, "Scroll down"),
    (Command::ScrollUp, "Scroll up"),
    (Command::LoadNextPage, "Next page (keys or open value)"),
    (
        Command::LoadPreviousPage,
        "Previous page (keys or open value)",
    ),
    (Command::RefreshSpace, "Refresh keyspace"),
    (Command::SetPattern, "Set filter pattern"),
    (Command::DeletePattern, "Delete filter pattern"),
    (Command::OpenSortMenu, "Sort by key / type / TTL / size"),
    (Command::TypeFilter, "Show only some types"),
    (Command::ToggleTree, "Tree view (←/→ fold, Enter opens)"),
    (Command::LoadAll, "Load all keys (Esc stops)"),
    (Command::GoToOpenKey, "Go to the open key"),
    (Command::EnterValue, "Enter value / JSON field / confirm"),
    (Command::StreamTab, "Stream: entries / groups"),
    (Command::AddKey, "Add new key"),
    (Command::AddItem, "Add item (in collection)"),
    (Command::EditValue, "Edit value / item (drill in first)"),
    (Command::EditExternal, "Edit value / item in $EDITOR"),
    (Command::DeleteKey, "Delete key / item (in value)"),
    (Command::SetTtl, "Set TTL"),
    (Command::Rename, "Rename key / field / member"),
    (Command::CopyKey, "Copy key (Tab: target DB)"),
    (Command::ToggleMark, "Mark / unmark key (d deletes marked)"),
    (Command::MarkAllVisible, "Mark all keys on the page"),
    (Command::ClearMarks, "Clear marks"),
    (Command::YankValue, "Copy value / item"),
    (Command::YankKey, "Copy key name"),
    (Command::ToggleWrap, "Wrap text value on / off"),
    (Command::ScrollLeft, "Pan text left (wrap off)"),
    (Command::ScrollRight, "Pan text right (wrap off)"),
    (Command::SelectDb, "Switch database"),
    (Command::ToggleDashboard, "Open metrics dashboard"),
    (Command::TogglePubSub, "Open Pub/Sub"),
    (Command::ClosePopup, "Close popup / overlay"),
    (Command::ToggleReadOnly, "Toggle read-only"),
    (Command::ToggleHelp, "Toggle this help"),
    (Command::Quit, "Quit"),
];

/// Keys every text input takes (ratatui-textarea's own, not rebindable).
const TEXT_INPUT_HELP: &[(&str, &str)] = &[("Undo / redo in a text input", "Ctrl-U / Ctrl-R")];

/// The main area and, below it, the five-row footer.
fn layout(area: Rect) -> [Rect; 2] {
    Layout::vertical([Constraint::Percentage(100), Constraint::Length(5)])
        .flex(Flex::Center)
        .margin(1)
        .areas(area)
}

/// Columns between a help label and its keys.
const HELP_GAP: usize = 2;

/// Widths of the `?` help rows: the label column (labels are padded to the
/// longest one) and a whole row, label column + gap + the widest keys.
fn help_columns(rows: &[(String, String)]) -> (usize, usize) {
    let label_width = rows
        .iter()
        .map(|(label, _)| label.width())
        .max()
        .unwrap_or(0);
    let keys_width = rows.iter().map(|(_, keys)| keys.width()).max().unwrap_or(0);
    (label_width, label_width + HELP_GAP + keys_width)
}

/// Columns between two columns of the `?` help.
const HELP_COLUMN_GAP: usize = 3;

/// How the `?` help's rows are laid out in the area: as many columns as the
/// height needs (each `rows` tall, balanced), but no more than fit across.
/// What still doesn't fit is counted on the bottom border.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HelpGrid {
    columns: usize,
    rows: usize,
}

impl HelpGrid {
    fn fit(count: usize, row_width: usize, area: Rect) -> Self {
        let max_rows = usize::from(area.height.saturating_sub(2)).max(1);
        let needed = count.div_ceil(max_rows).max(1);
        // Borders and padding take 4 columns.
        let room = usize::from(area.width).saturating_sub(4);
        let across = ((room + HELP_COLUMN_GAP) / (row_width + HELP_COLUMN_GAP)).max(1);
        let columns = needed.min(across);
        let rows = if columns == needed {
            count.div_ceil(columns)
        } else {
            max_rows
        };
        Self { columns, rows }
    }

    fn width(self, row_width: usize) -> usize {
        self.columns * row_width + (self.columns - 1) * HELP_COLUMN_GAP
    }
}

impl App {
    pub(super) fn draw(&mut self, tui: &mut tui::Tui) -> Result<()> {
        tui.draw(|frame| {
            self.build_frame(frame.area());
            frame.render_stateful_widget(AppWidget, frame.area(), self);
        })?;
        Ok(())
    }

    /// Copy what this frame draws out of the shared state, each lock held
    /// only for its copy. Widgets then render from `self.frame` alone.
    /// Records in `self.redraw` which data the frame shows.
    pub(super) fn build_frame(&mut self, area: Rect) {
        let [main, _] = layout(area);
        let (now, wall) = (self.clock.now(), self.clock.wall());
        self.frame.cluster = self.cluster_enabled();
        self.frame.whole_cluster = self
            .state
            .info
            .lock()
            .as_ref()
            .is_some_and(|info| info.whole_cluster);
        self.frame.refresh_footer(&self.state);
        self.redraw.info_seq = self.frame.footer.key().map_or(0, |(seq, _)| seq);
        let mode = self.view_mode();
        self.frame.metrics = if mode == Mode::Dashboard {
            self.dashboard.live_window(main.width)
        } else {
            None
        };
        self.frame.cluster_view = if mode == Mode::Dashboard {
            self.dashboard.cluster_view()
        } else {
            None
        };
        self.frame.pubsub = (mode == Mode::PubSub).then(|| {
            // Read before the view: a message landing in between is drawn
            // now and again on the next frame, never skipped.
            self.redraw.pubsub = self.state.pubsub.lock().version();
            self.redraw.wall_sec = wall_sec(wall);
            self.pubsub.build_view(main, now, wall)
        });
        self.frame.node_strip = self.node_strip(mode);
        // The node row takes the screen's first line.
        let body = if self.frame.node_strip.is_some() {
            Rect {
                y: main.y + 1,
                height: main.height.saturating_sub(1),
                ..main
            }
        } else {
            main
        };
        self.frame.monitor = (mode == Mode::Monitor).then(|| {
            // Read before the view, like the Pub/Sub tab.
            self.redraw.monitor = self.state.monitor.lock().version();
            self.redraw.wall_sec = wall_sec(wall);
            self.monitor.build_view(body, wall)
        });
    }

    /// Whether `Render` at `now` should draw (#50): something changed
    /// (`dirty`), or data that changes on its own moved since the last
    /// frame. A new `INFO` sample redraws the live dashboard. Elsewhere it
    /// rebuilds the footer here, and a changed footer (or cluster flag)
    /// draws at most once per [`FOOTER_REDRAW_INTERVAL`]: an idle server's
    /// `CPU: x.x %` flickers on most samples. The Pub/Sub tab redraws on a
    /// new message and once per wall-clock second, for its rates and ages.
    pub(super) fn should_draw(&mut self, now: Instant) -> bool {
        if self.redraw.dirty {
            return true;
        }
        let mode = self.view_mode();
        let seq = self.state.info_seq.load(Ordering::Acquire);
        if seq != self.redraw.info_seq {
            if mode == Mode::Dashboard && !self.dashboard.is_paused() {
                return true;
            }
            let footer = self.frame.refresh_footer(&self.state);
            let cluster = self.cluster_enabled() != self.frame.cluster;
            self.redraw.footer_stale |= footer || cluster;
            self.redraw.info_seq = seq;
        }
        if self.redraw.footer_stale
            && self
                .redraw
                .drawn_at
                .is_none_or(|at| now.duration_since(at) >= FOOTER_REDRAW_INTERVAL)
        {
            return true;
        }
        if mode == Mode::PubSub {
            return self.state.pubsub.lock().version() != self.redraw.pubsub
                || wall_sec(self.clock.wall()) != self.redraw.wall_sec;
        }
        if mode == Mode::Monitor {
            // The rate in the title moves each second while it runs.
            return self.state.monitor.lock().version() != self.redraw.monitor
                || (self.monitor_active() && wall_sec(self.clock.wall()) != self.redraw.wall_sec);
        }
        false
    }

    /// After a `Render` at `now`: note the frame if it drew, and log the
    /// frames drawn once a minute.
    pub(super) fn count_frame(&mut self, drew: bool, now: Instant) {
        if drew {
            self.redraw.dirty = false;
            self.redraw.footer_stale = false;
            self.redraw.drawn_at = Some(now);
            self.redraw.frames += 1;
        }
        let since = *self.redraw.since.get_or_insert(now);
        if now.duration_since(since) >= FRAMES_LOG_INTERVAL {
            log::debug!("frames_drawn: {} in the last minute", self.redraw.frames);
            self.redraw.frames = 0;
            self.redraw.since = Some(now);
        }
    }

    /// The mode drawn under a popup: the one it was opened from.
    pub(super) fn view_mode(&self) -> Mode {
        match self.mode {
            Mode::Popup(_) => self.previous_mode.unwrap_or(Mode::KeySpace),
            m => m,
        }
    }

    /// Point the footer hint at the current mode.
    pub(super) fn sync_hint(&mut self) {
        let (mode, entries) = match self.mode {
            Mode::Dashboard => (Mode::Dashboard, DASHBOARD_HINT_ENTRIES),
            Mode::PubSub => (Mode::PubSub, PUBSUB_HINT_ENTRIES),
            Mode::Monitor => (Mode::Monitor, MONITOR_HINT_ENTRIES),
            Mode::Slowlog => (Mode::Slowlog, SLOWLOG_HINT_ENTRIES),
            _ => (Mode::KeySpace, KEYSPACE_HINT_ENTRIES),
        };
        let read_only = self.is_read_only();
        let entries: Vec<(Command, &str)> = entries
            .iter()
            .filter(|(cmd, _)| !(read_only && WRITE_COMMANDS.contains(cmd)))
            .copied()
            .collect();
        self.summary.hint = build_hint(&config::get().keybindings, mode, &entries);
    }
}

impl StatefulWidget for AppWidget {
    type State = App;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let cfg = config::get();
        Block::default().bg(cfg.colors.base00).render(area, buf);

        let [main, footer] = layout(area);
        state.frame.hits.clear();
        state.frame.hits.push(main, HitTarget::Main);
        state.frame.hits.push(footer, HitTarget::Footer);
        // The environment's colour as a line across the top margin row.
        Block::new()
            .borders(Borders::TOP)
            .border_style(cfg.colors.env(state.environment))
            .render(Rect { height: 1, ..area }, buf);

        StatefulWidget::render(
            InfoWidget {
                frame: &state.frame,
            },
            footer,
            buf,
            &mut state.summary,
        );
        let pubsub_panes = state.frame.pubsub.as_ref().map(PubSubView::pane_rects);
        state.render_main_block(main, buf);
        match (state.view_mode(), pubsub_panes) {
            (Mode::KeySpace, _) => {
                let panes = state.keyspace.pane_areas();
                state.frame.hits.push(panes.keys, HitTarget::KeysPane);
                state.frame.hits.push(panes.value, HitTarget::ValuePane);
            }
            (Mode::PubSub, Some((list, log))) => {
                state.frame.hits.push(list, HitTarget::PubSubList);
                state.frame.hits.push(log, HitTarget::PubSubLog);
            }
            _ => {}
        }
        StatefulWidget::render(DbPickerWidget, main, buf, &mut state.db_picker);

        if state.help_is_open() {
            let mode = match state.mode {
                Mode::Popup(_) => state.previous_mode.unwrap_or(Mode::KeySpace),
                m => m,
            };
            state.help_pages = App::render_help_page(area, buf, mode, state.help_page);
            state.help_page = state.help_page.min(state.help_pages.saturating_sub(1));
        }
        state.render_confirm(main, buf);
        // Last: above every mode, popup and overlay.
        StatefulWidget::render(ToastWidget, main, buf, &mut state.toast);
    }
}

/// Render logic
impl App {
    fn render_main_block(&mut self, area: Rect, buf: &mut Buffer) {
        match self.view_mode() {
            Mode::KeySpace => self.render_key_space(area, buf),
            Mode::Dashboard => {
                let widget = DashboardWidget { frame: &self.frame };
                StatefulWidget::render(widget, area, buf, &mut self.dashboard);
            }
            Mode::PubSub => {
                self.pubsub.cluster_mode = self.frame.cluster;
                if let Some(view) = self.frame.pubsub.take() {
                    StatefulWidget::render(PubSubWidget { view }, area, buf, &mut self.pubsub);
                }
            }
            Mode::Monitor => {
                let body = self.render_node_strip(area, buf);
                if let Some(view) = self.frame.monitor.take() {
                    StatefulWidget::render(MonitorWidget { view }, body, buf, &mut *self.monitor);
                }
            }
            Mode::Slowlog => {
                let body = self.render_node_strip(area, buf);
                let wall = self.clock.wall();
                StatefulWidget::render(SlowlogWidget { wall }, body, buf, &mut self.slowlog);
            }
            Mode::Common | Mode::Popup(_) => {}
        }
        if self.mode == Mode::Popup(PopupMode::Error) {
            self.render_error_popup(area, buf);
        }
    }

    /// The cluster's node row, if any; returns the area left for the screen.
    fn render_node_strip(&self, area: Rect, buf: &mut Buffer) -> Rect {
        match &self.frame.node_strip {
            Some(strip) => strip.render_above(area, buf),
            None => area,
        }
    }

    /// Centered bordered box with the current error message.
    fn render_error_popup(&self, area: Rect, buf: &mut Buffer) {
        let cfg = config::get();
        let msg = self.error_message.as_deref().unwrap_or_default();
        let footer = "Esc/Enter to close";

        let text_w = msg
            .lines()
            .map(str::len)
            .max()
            .unwrap_or(0)
            .max(footer.len());
        #[allow(clippy::cast_possible_truncation)]
        let popup_w = ((text_w as u16).saturating_add(4)).max(20).min(area.width);
        #[allow(clippy::cast_possible_truncation)]
        let popup_h = ((msg.lines().count() as u16).saturating_add(4)).min(area.height);

        let [_, popup_area, _] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(popup_h),
            Constraint::Fill(1),
        ])
        .flex(Flex::Center)
        .areas(area);
        let [_, popup_area, _] = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(popup_w),
            Constraint::Fill(1),
        ])
        .flex(Flex::Center)
        .areas(popup_area);

        let block = Block::default()
            .title(" Error ")
            .title_alignment(Alignment::Center)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(cfg.colors.base08)
            .bg(cfg.colors.base00)
            .fg(cfg.colors.base05);

        let mut lines: Vec<Line> = msg.lines().map(Line::from).collect();
        lines.push(Line::from(""));
        lines.push(Line::styled(footer, cfg.colors.base04));

        Clear.render(popup_area, buf);
        Paragraph::new(lines)
            .wrap(ratatui::widgets::Wrap { trim: true })
            .block(block)
            .render(popup_area, buf);
    }

    fn render_key_space(&mut self, area: Rect, buf: &mut Buffer) {
        // Only a session stuck on one node of a cluster has a partial list.
        self.keyspace.cluster_mode = self.frame.cluster && !self.frame.whole_cluster;
        StatefulWidget::render(KeySpaceWidget, area, buf, &mut self.keyspace);
    }

    /// Render the `?` help overlay: a centered popup listing every command and
    /// the key sequence(s) bound to it. The box grows ("extends") to fit its
    /// content — both width and height are derived from the rows — and is
    /// clamped to the available area.
    pub(crate) fn render_help_overlay(area: Rect, buf: &mut Buffer, mode: Mode) {
        Self::render_help_page(area, buf, mode, 0);
    }

    /// The `?` help, page `page` of what fits in `area`. Returns how many
    /// pages there are (1 when everything fits).
    pub(crate) fn render_help_page(area: Rect, buf: &mut Buffer, mode: Mode, page: usize) -> usize {
        let cfg = config::get();
        let keybindings = &cfg.keybindings;

        // Commands shown in the help, paired with a human-readable label. Each
        // is resolved to its configured key sequences via
        // `KeyBindings::get_config_for_command`.
        let (lookup_mode, entries) = match mode {
            Mode::Dashboard => (Mode::Dashboard, DASHBOARD_HELP_ENTRIES),
            Mode::PubSub => (Mode::PubSub, PUBSUB_HELP_ENTRIES),
            Mode::Monitor => (Mode::Monitor, MONITOR_HELP_ENTRIES),
            Mode::Slowlog => (Mode::Slowlog, SLOWLOG_HELP_ENTRIES),
            _ => (Mode::KeySpace, KEYSPACE_HELP_ENTRIES),
        };

        let mut rows: Vec<(String, String)> = entries
            .iter()
            .chain(GO_TO_HELP_ENTRIES)
            .map(|(command, label)| {
                let mut binds = keybindings.get_config_for_command(lookup_mode, *command);
                binds.extend(keybindings.get_config_for_command(Mode::Common, *command));
                let keys = if binds.is_empty() {
                    "—".to_string()
                } else {
                    binds.join(", ")
                };
                ((*label).to_string(), keys)
            })
            .collect();
        // Fixed by ratatui-textarea, so not in the keybindings: listed for
        // the screens that have text inputs.
        if !matches!(mode, Mode::Dashboard | Mode::Slowlog) {
            rows.extend(
                TEXT_INPUT_HELP
                    .iter()
                    .map(|(label, keys)| ((*label).to_owned(), (*keys).to_owned())),
            );
        }

        let (label_width, row_width) = help_columns(&rows);
        let grid = HelpGrid::fit(rows.len(), row_width, area);
        let per_page = (grid.columns * grid.rows).max(1);
        let pages = rows.len().div_ceil(per_page).max(1);
        let page = page.min(pages - 1);
        let rows = &rows[page * per_page..rows.len().min((page + 1) * per_page)];
        let title = " Help ";
        let inner_width = grid.width(row_width).max(title.width());
        // Borders (2) and one column of padding on each side (2).
        let popup_w = u16::try_from(inner_width + 4)
            .unwrap_or(u16::MAX)
            .min(area.width);
        let popup_h = u16::try_from(grid.rows + 2)
            .unwrap_or(u16::MAX)
            .min(area.height);

        let [_, popup_area, _] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(popup_h),
            Constraint::Fill(1),
        ])
        .flex(Flex::Center)
        .areas(area);
        let [_, popup_area, _] = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(popup_w),
            Constraint::Fill(1),
        ])
        .flex(Flex::Center)
        .areas(popup_area);

        let mut block = Block::default()
            .title(title)
            .title_alignment(Alignment::Center)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(cfg.colors.base04)
            .bg(cfg.colors.base00)
            .fg(cfg.colors.base05)
            .padding(ratatui::widgets::Padding::horizontal(1));
        if pages > 1 {
            block = block.title_bottom(
                Line::from(format!(" {}/{pages} · n / p: page ", page + 1))
                    .fg(cfg.colors.base0a)
                    .centered(),
            );
        }
        let inner = block.inner(popup_area);
        Clear.render(popup_area, buf);
        block.render(popup_area, buf);

        let line = |(label, keys): &(String, String)| {
            let pad = label_width - label.width() + HELP_GAP;
            Line::from(vec![
                Span::raw(format!("{label}{}", " ".repeat(pad))),
                Span::styled(keys.clone(), cfg.colors.base04),
            ])
        };
        // Column-major: read down the first column, then the next.
        let step = u16::try_from(row_width + HELP_COLUMN_GAP).unwrap_or(u16::MAX);
        for (i, chunk) in rows.chunks(grid.rows.max(1)).enumerate() {
            let x = inner
                .x
                .saturating_add(step.saturating_mul(u16::try_from(i).unwrap_or(u16::MAX)));
            if x >= inner.right() {
                break;
            }
            let column = Rect {
                x,
                width: (inner.right() - x).min(u16::try_from(row_width).unwrap_or(u16::MAX)),
                ..inner
            };
            Paragraph::new(chunk.iter().map(line).collect::<Vec<_>>()).render(column, buf);
        }
        pages
    }
}

#[cfg(test)]
mod help_grid_tests {
    use super::HelpGrid;
    use ratatui::layout::Rect;

    #[test]
    fn rows_flow_into_as_many_columns_as_fit() {
        let area = |w, h| Rect::new(0, 0, w, h);
        // Tall enough: one column.
        assert_eq!(
            HelpGrid::fit(37, 50, area(120, 40)),
            HelpGrid {
                columns: 1,
                rows: 37
            }
        );
        // 22 rows of room: two balanced columns of 19.
        assert_eq!(
            HelpGrid::fit(37, 50, area(120, 24)),
            HelpGrid {
                columns: 2,
                rows: 19
            }
        );
        // Two don't fit across 80: one full column, the rest counted.
        assert_eq!(
            HelpGrid::fit(37, 50, area(80, 24)),
            HelpGrid {
                columns: 1,
                rows: 22
            }
        );
        // Very wide and short: three columns.
        assert_eq!(
            HelpGrid::fit(37, 50, area(200, 16)),
            HelpGrid {
                columns: 3,
                rows: 13
            }
        );
    }
}

#[cfg(test)]
mod env_line_tests {
    use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};

    use super::AppWidget;
    use crate::{app::tests::app, config, target::Environment};

    #[test]
    fn top_row_is_a_line_in_the_environment_colour() {
        for env in [Environment::Local, Environment::Prod] {
            let mut app = app().environment(env);
            let area = Rect::new(0, 0, 80, 24);
            let mut buf = Buffer::empty(area);
            app.build_frame(area);
            AppWidget.render(area, &mut buf, &mut app);
            for x in [0, 40, 79] {
                assert_eq!(buf[(x, 0)].symbol(), "─", "{env:?} x={x}");
                assert_eq!(buf[(x, 0)].fg, config::get().colors.env(env), "{env:?}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ratatui::buffer::Buffer;
    use ratatui::prelude::Rect;

    use crate::action::Action;
    use crate::app::tests::{app, app_on_orders, render_screen};
    use crate::clock::{Clock, FixedClock};
    use crate::mode::Mode;
    use std::sync::Arc;

    /// An app on a fixed clock, with its first frame drawn.
    fn drawn_app(mut app: super::App) -> (super::App, Arc<FixedClock>) {
        let clock = Arc::new(FixedClock::new(UNIX_EPOCH + Duration::from_secs(1000)));
        app.clock = clock.clone();
        let now = clock.now();
        assert!(app.should_draw(now), "the first frame always draws");
        draw(&mut app);
        (app, clock)
    }

    /// What `Render` does, on a test screen.
    fn draw(app: &mut super::App) {
        render_screen(app);
        let now = app.clock.now();
        app.count_frame(true, now);
    }

    /// A new `INFO` sample landed.
    fn bump_info(app: &super::App) {
        app.state.info_seq.fetch_add(1, Ordering::Release);
    }

    /// A minute of 100 ms ticks on the key list with one `INFO` sample per
    /// second; `flicker` changes the footer text on every sample, as an
    /// idle server's CPU % does. Returns the frames drawn.
    fn idle_minute(flicker: bool) -> usize {
        let (mut app, clock) = drawn_app(app());
        let mut frames = 0;
        for tick in 1..=600 {
            clock.advance(Duration::from_millis(100));
            if tick % 10 == 0 {
                if flicker {
                    *app.state.db.lock() = tick / 10 % 2;
                }
                bump_info(&app);
            }
            app.update(&Action::Tick);
            app.update(&Action::Render);
            if app.should_draw(clock.now()) {
                draw(&mut app);
                frames += 1;
            }
        }
        frames
    }

    /// #50 AC5: an idle key list draws nothing while the footer stays the
    /// same, and under 10 frames a minute while it changes every second.
    #[test]
    fn idle_key_list_draws_almost_nothing() {
        assert_eq!(idle_minute(false), 0);
        assert_eq!(idle_minute(true), 6, "one per FOOTER_REDRAW_INTERVAL");
    }

    /// #50 AC6: the live dashboard draws once per `INFO` sample, and not
    /// while paused.
    #[test]
    fn dashboard_draws_once_per_sample() {
        let mut app = app();
        app.toggle_dashboard();
        let (mut app, _clock) = drawn_app(app);
        assert!(!app.should_draw(app.clock.now()));
        bump_info(&app);
        assert!(app.should_draw(app.clock.now()));
        draw(&mut app);
        assert!(!app.should_draw(app.clock.now()));

        app.dashboard.toggle_pause();
        draw(&mut app);
        bump_info(&app);
        assert!(!app.should_draw(app.clock.now()), "paused, same footer");
    }

    /// #50 AC6: the Pub/Sub tab draws a new message on the next frame, and
    /// once per wall-clock second for its rates and ages.
    #[test]
    fn pubsub_draws_new_messages_and_each_second() {
        let (mut app, clock) = drawn_app(app_on_orders());
        assert!(!app.should_draw(app.clock.now()));
        let wall = app.clock.wall();
        app.state
            .pubsub
            .lock()
            .push("orders:new", Some("orders:*"), b"x", wall);
        assert!(app.should_draw(app.clock.now()));
        draw(&mut app);
        assert!(!app.should_draw(app.clock.now()));
        clock.advance(Duration::from_secs(1));
        assert!(app.should_draw(app.clock.now()));
    }

    /// A footer that shows a new value draws, on any screen, but at most
    /// once per `FOOTER_REDRAW_INTERVAL`.
    #[test]
    fn changed_footer_draws_at_most_every_interval() {
        let (mut app, clock) = drawn_app(app());
        *app.state.db.lock() = 3;
        bump_info(&app);
        assert!(!app.should_draw(clock.now()), "a frame was just drawn");
        clock.advance(FOOTER_REDRAW_INTERVAL);
        assert!(app.should_draw(clock.now()));
        draw(&mut app);
        assert!(!app.should_draw(clock.now()));
    }

    #[test]
    fn help_box_fits_short_labels_with_long_keys() {
        let rows = vec![
            ("Back".to_string(), "b, Esc".to_string()),
            (
                "Add subscription (channel or pattern)".to_string(),
                "a".to_string(),
            ),
            ("Next".to_string(), "Down, j".to_string()),
            ("Missing".to_string(), "—".to_string()),
        ];
        let (label, row) = help_columns(&rows);
        assert_eq!(label, 37);
        // The widest keys ("Down, j") sit after the longest label column.
        assert_eq!(row, 37 + HELP_GAP + 7);
    }

    #[test]
    fn help_overlay_shows_every_pubsub_binding_whole() {
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        App::render_help_overlay(area, &mut buf, Mode::PubSub);
        let text: String = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        // Key order within a row follows the keymap's hash order.
        let back = text.lines().find(|l| l.contains("Back")).unwrap();
        assert!(back.contains("Esc") && back.contains('b'), "{text}");
        let next = text.lines().find(|l| l.contains("Down (list")).unwrap();
        assert!(
            next.contains("Down, j") || next.contains("j, Down"),
            "{text}"
        );
        let row = |label: &str| {
            text.lines()
                .find(|l| l.contains(label))
                .unwrap_or_else(|| panic!("{label} missing:\n{text}"))
                .to_owned()
        };
        assert!(row("Switch focus").contains("Tab"), "{text}");
        assert!(row("Pause / follow").contains("Space"), "{text}");
        assert!(row("Copy message channel").contains("Shift-Y"), "{text}");
        assert!(row("Copy message payload").contains("  y "), "{text}");
        assert!(row("open message").contains("Enter"), "{text}");
        for whole in ["Ctrl-c", "Clear messages and stats", "Pan detail right"] {
            assert!(text.contains(whole), "{whole} missing:\n{text}");
        }
    }

    #[test]
    fn toast_renders_over_every_mode() {
        let mut app = app();
        let check = |app: &mut App, what: &str| {
            app.toast.show("hello toast", false, app.clock.now());
            let text = render_screen(app);
            assert!(text.contains("hello toast"), "{what}:\n{text}");
        };
        check(&mut app, "keyspace");
        app.toggle_dashboard();
        check(&mut app, "dashboard");
        app.toggle_dashboard();
        app.toggle_pubsub();
        check(&mut app, "pubsub");
        app.show_error_popup("boom");
        check(&mut app, "error popup");
        app.switch_to_previous_mode();
        app.toggle_help(app.clock.now());
        check(&mut app, "help overlay");
    }
}
