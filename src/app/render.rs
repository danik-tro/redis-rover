use super::App;

use crate::{config, mode::PopupMode};
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
        pubsub::PubSubWidget,
        toast::ToastWidget,
    },
};

pub struct AppWidget;

/// Keyspace hint entries that write; hidden in `--read-only` mode.
const WRITE_COMMANDS: &[Command] = &[
    Command::AddKey,
    Command::AddItem,
    Command::EditValue,
    Command::DeleteKey,
    Command::SetTtl,
    Command::PubSubPublish,
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
    (Command::PubSubAdd, "Add subscription (channel or pattern)"),
    (Command::PubSubRemove, "Unsubscribe and drop messages"),
    (Command::PubSubClear, "Clear messages and stats"),
    (Command::PubSubPublish, "Publish a message"),
    (
        Command::PubSubToggleNotify,
        "Toggle notifications (also un-mutes)",
    ),
    (Command::PubSubRefresh, "Refresh server channels"),
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
    (Command::ToggleAllCharts, "Toggle all charts"),
    (Command::TogglePubSub, "Open Pub/Sub"),
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
    (Command::EnterValue, "Enter value / confirm"),
    (Command::AddKey, "Add new key"),
    (Command::AddItem, "Add item (in collection)"),
    (Command::EditValue, "Edit value / item (drill in first)"),
    (Command::DeleteKey, "Delete key / item (in value)"),
    (Command::SetTtl, "Set TTL"),
    (Command::YankValue, "Copy value / item"),
    (Command::YankKey, "Copy key name"),
    (Command::ToggleWrap, "Wrap text value on / off"),
    (Command::ScrollLeft, "Pan text left (wrap off)"),
    (Command::ScrollRight, "Pan text right (wrap off)"),
    (Command::SelectDb, "Switch database"),
    (Command::ToggleDashboard, "Open metrics dashboard"),
    (Command::TogglePubSub, "Open Pub/Sub"),
    (Command::ClosePopup, "Close popup / overlay"),
    (Command::ToggleHelp, "Toggle this help"),
    (Command::Quit, "Quit"),
];

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
    pub(super) fn build_frame(&mut self, area: Rect) {
        let [main, _] = layout(area);
        self.frame.cluster = self.cluster_enabled();
        self.frame.refresh_footer(&self.state);
        let mode = self.view_mode();
        self.frame.metrics = if mode == Mode::Dashboard {
            self.dashboard.live_window(main.width)
        } else {
            None
        };
        self.frame.pubsub = (mode == Mode::PubSub).then(|| self.pubsub.build_view(main));
    }

    /// The mode drawn under a popup: the one it was opened from.
    fn view_mode(&self) -> Mode {
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
            _ => (Mode::KeySpace, KEYSPACE_HINT_ENTRIES),
        };
        let entries: Vec<(Command, &str)> = entries
            .iter()
            .filter(|(cmd, _)| !(self.read_only && WRITE_COMMANDS.contains(cmd)))
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

        StatefulWidget::render(
            InfoWidget {
                frame: &state.frame,
            },
            footer,
            buf,
            &mut state.summary,
        );
        state.render_main_block(main, buf);
        StatefulWidget::render(DbPickerWidget, main, buf, &mut state.db_picker);

        if state.help_is_open() {
            let mode = match state.mode {
                Mode::Popup(_) => state.previous_mode.unwrap_or(Mode::KeySpace),
                m => m,
            };
            App::render_help_overlay(area, buf, mode);
        }
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
            Mode::Common | Mode::Popup(_) => {}
        }
        if self.mode == Mode::Popup(PopupMode::Error) {
            self.render_error_popup(area, buf);
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
        self.keyspace.cluster_mode = self.frame.cluster;
        StatefulWidget::render(KeySpaceWidget, area, buf, &mut self.keyspace);
    }

    /// Render the `?` help overlay: a centered popup listing every command and
    /// the key sequence(s) bound to it. The box grows ("extends") to fit its
    /// content — both width and height are derived from the rows — and is
    /// clamped to the available area.
    fn render_help_overlay(area: Rect, buf: &mut Buffer, mode: Mode) {
        let cfg = config::get();
        let keybindings = &cfg.keybindings;

        // Commands shown in the help, paired with a human-readable label. Each
        // is resolved to its configured key sequences via
        // `KeyBindings::get_config_for_command`.
        let (lookup_mode, entries) = match mode {
            Mode::Dashboard => (Mode::Dashboard, DASHBOARD_HELP_ENTRIES),
            Mode::PubSub => (Mode::PubSub, PUBSUB_HELP_ENTRIES),
            _ => (Mode::KeySpace, KEYSPACE_HELP_ENTRIES),
        };

        let rows: Vec<(String, String)> = entries
            .iter()
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

        // "Extend" the box to its content, clamped to the available area:
        // borders (2) and one column of padding on each side (2).
        let title = " Help ";
        let (label_width, content_width) = help_columns(&rows);
        let inner_width = content_width.max(title.width());
        let popup_w = u16::try_from(inner_width + 4)
            .unwrap_or(u16::MAX)
            .min(area.width);
        let popup_h = u16::try_from(rows.len() + 2)
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

        let block = Block::default()
            .title(title)
            .title_alignment(Alignment::Center)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(cfg.colors.base04)
            .bg(cfg.colors.base00)
            .fg(cfg.colors.base05)
            .padding(ratatui::widgets::Padding::horizontal(1));

        let lines: Vec<Line> = rows
            .iter()
            .map(|(label, keys)| {
                let pad = label_width - label.width() + HELP_GAP;
                Line::from(vec![
                    Span::raw(format!("{label}{}", " ".repeat(pad))),
                    Span::styled(keys.clone(), cfg.colors.base04),
                ])
            })
            .collect();

        Clear.render(popup_area, buf);
        Paragraph::new(lines).block(block).render(popup_area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ratatui::buffer::Buffer;
    use ratatui::prelude::Rect;

    use crate::app::tests::{app, render_screen};
    use crate::mode::Mode;

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
            app.toast.show("hello toast", false);
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
        app.toggle_help();
        check(&mut app, "help overlay");
    }
}
