use std::time::{Duration, Instant};

use crate::{
    config, mode::PopupMode, redis_client::event::RedisEvent, redis_client::types::RedisType,
    state::SharedState,
};
use color_eyre::eyre::Result;
use crossterm::event::KeyCode;
use ratatui::prelude::Rect;
use ratatui::{
    buffer::Buffer,
    crossterm::event::KeyEvent,
    layout::{Alignment, Constraint, Flex, Layout},
    style::Stylize,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, StatefulWidget, Widget},
};
use tokio::sync::{
    broadcast,
    mpsc::{self, Receiver, Sender},
};
use tokio_util::sync::CancellationToken;

use crate::{
    action::{self, Action},
    command::Command,
    keybindings::KeyResolution,
    mode::Mode,
    tui,
    widgets::{
        dashboard::{Dashboard, DashboardWidget},
        info::{build_hint, Info, InfoWidget},
        keyspace::{KeySpace, KeySpaceWidget, OverlayRequest, PageInfo},
    },
};

pub struct AppWidget;

/// How long the `?` help overlay stays up before auto-dismissing.
const HELP_TIMEOUT: Duration = Duration::from_secs(7);
/// How long a partially typed key chord waits for its next key.
const CHORD_TIMEOUT: Duration = Duration::from_secs(1);

pub struct App {
    state: SharedState,

    tick_rate: f64,
    frame_rate: f64,

    mode: Mode,
    previous_mode: Option<Mode>,
    error_message: Option<String>,
    /// When the `?` help overlay was opened. `None` means it's hidden. The
    /// overlay auto-dismisses after [`HELP_TIMEOUT`] or on the next command.
    help_opened_at: Option<Instant>,
    /// After a non-destructive edit (value/TTL), the name of the key to keep
    /// selected once the post-write keyspace refresh lands, so the cursor stays
    /// put instead of snapping back to the top.
    reselect_key: Option<String>,
    /// After an in-collection add-item, the value-table row to restore focus to
    /// once the (now larger) collection value reloads, so the user stays inside
    /// the collection instead of bouncing back to the key list.
    restore_value_row: Option<usize>,
    /// Armed by `load_new_keys` after the post-add-item refresh re-requests the
    /// value, so the *final* value load (not an earlier racing one) restores
    /// value focus.
    restore_value_pending: bool,

    should_quit: bool,
    /// Keys of a partially typed chord (a strict prefix of some binding).
    pending_keys: Vec<KeyEvent>,
    /// When the last key went into `pending_keys`, for [`CHORD_TIMEOUT`].
    last_key_at: Option<Instant>,
    /// `FetchKeys` requests sent but not yet answered; drives the loading
    /// indicator.
    keys_in_flight: usize,

    tx: mpsc::Sender<Action>,
    rx: mpsc::Receiver<Action>,

    redis_tx: broadcast::Sender<RedisEvent>,
    summary: Info,
    keyspace: KeySpace,
    dashboard: Dashboard,
}

/// Footer hint entries for the keyspace view, in display order.
const KEYSPACE_HINT_ENTRIES: &[(Command, &str)] = &[
    (Command::AddKey, "add"),
    (Command::AddItem, "add-item"),
    (Command::EditValue, "edit"),
    (Command::DeleteKey, "del"),
    (Command::SetTtl, "ttl"),
    (Command::EnterValue, "open"),
    (Command::SetPattern, "filter"),
    (Command::ToggleHelp, "help"),
];

/// Footer hint entries for the dashboard view, in display order.
const DASHBOARD_HINT_ENTRIES: &[(Command, &str)] = &[
    (Command::ToggleDashboard, "back"),
    (Command::ToggleMetricsPause, "pause"),
    (Command::ResetMetrics, "reset"),
    (Command::NextChart, "chart"),
    (Command::ToggleAllCharts, "all"),
    (Command::ToggleHelp, "help"),
];

impl App {
    pub fn new(
        state: SharedState,
        tx: Sender<Action>,
        rx: Receiver<Action>,
        redis_tx: broadcast::Sender<RedisEvent>,
        target: String,
        tick_rate: f64,
        frame_rate: f64,
    ) -> Self {
        let _ = config::get();

        let mode = Mode::KeySpace;

        let summary = Info::new(state.info.clone(), state.metrics.clone(), target);
        let keyspace = KeySpace::new(Vec::new());
        let dashboard = Dashboard::new(state.metrics.clone());

        let mut app = Self {
            state,
            summary,
            keyspace,
            dashboard,
            tick_rate,
            frame_rate,
            should_quit: false,
            mode,
            previous_mode: None,
            error_message: None,
            help_opened_at: None,
            reselect_key: None,
            restore_value_row: None,
            restore_value_pending: false,
            pending_keys: Vec::new(),
            last_key_at: None,
            keys_in_flight: 0,
            tx,
            rx,
            redis_tx,
        };
        app.sync_hint();
        app
    }

    /// Run the main event loop until quit.
    ///
    /// # Errors
    ///
    /// Returns an error if the TUI fails to enter or if any action
    /// dispatch returns an error.
    pub async fn run(&mut self, cancellation_token: CancellationToken) -> Result<()> {
        let mut tui = tui::Tui::new()?
            .tick_rate(self.tick_rate)
            .frame_rate(self.frame_rate)
            .cancellation_token(cancellation_token.clone());

        // tui.mouse(true);
        // `Tui` has no `Drop` teardown: always shut down explicitly, even when
        // entering or the loop fails, so the terminal is restored.
        let result = match tui.enter() {
            Ok(()) => self.event_loop(&mut tui).await,
            Err(err) => Err(err),
        };
        let shutdown = tui.shutdown().await;
        result.and(shutdown)
    }

    async fn event_loop(&mut self, tui: &mut tui::Tui) -> Result<()> {
        loop {
            // TODO: refactor with async_channel crate
            // replace with select multiplex
            if let Some(e) = tui.next().await {
                if let Some(action) = self.handle_event(&e) {
                    action::try_send_action(&self.tx, action);
                }
            }

            while let Ok(action) = self.rx.try_recv() {
                if let Some(next) = self.handle_action(&action, tui)? {
                    action::try_send_action(&self.tx, next);
                }
            }
            if self.should_quit {
                return Ok(());
            }
        }
    }

    fn resize(&mut self, tui: &mut tui::Tui, (w, h): (u16, u16)) -> Result<()> {
        tui.resize(Rect::new(0, 0, w, h))?;
        Ok(())
    }

    fn handle_event(&mut self, e: &tui::Event) -> Option<Action> {
        match e {
            // Emitted once when the event task starts: trigger the initial
            // keyspace load so the screen is populated without a manual action.
            tui::Event::Init => Some(Action::LoadKeySpace),
            tui::Event::Quit => Some(Action::Quit),
            tui::Event::Tick => Some(Action::Tick),
            tui::Event::Render => Some(Action::Render),
            tui::Event::Resize(x, y) => Some(Action::Resize(*x, *y)),
            tui::Event::Key(key) => self.handle_key_event(*key),
            _ => None,
        }
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> Option<Action> {
        let is_confirm = key.code == KeyCode::Enter;
        let is_cancel = key.code == KeyCode::Esc;

        // The error popup is modal: Esc/Enter dismiss it, and only global
        // bindings (e.g. Ctrl-c) pass through.
        if self.mode == Mode::Popup(PopupMode::Error) {
            if is_confirm || is_cancel {
                return Some(Action::PreviousMode);
            }
            return self.handle_keybindings(key);
        }

        // Keep a half-open keyspace overlay from swallowing dashboard keys.
        if self.mode == Mode::Dashboard {
            // Esc backs out of the help overlay first; any other key (`m`)
            // goes through as a real command.
            if is_cancel && self.help_is_open() {
                self.help_opened_at = None;
                return None;
            }
            return self.handle_keybindings(key);
        }

        // While the filter popup is open, raw keys feed its text-area; only
        // Enter (confirm) and Esc (cancel) escape to the dispatcher.
        if self.keyspace.is_popup() && !is_confirm && !is_cancel {
            self.keyspace.handle_key(key);
            return None;
        }

        // While a text-input action overlay (edit / TTL) is open, raw keys feed
        // it; Enter confirms, Esc cancels.
        if self.keyspace.is_input_active() && !is_confirm && !is_cancel {
            self.keyspace.handle_overlay_key(key);
            return None;
        }

        // The add-key wizard owns ALL keys except Esc (which cancels the whole
        // flow). Enter/Tab/j/k and text drive its multi-step machine internally;
        // after each key we check whether it asked to finalize.
        if self.keyspace.is_wizard_active() {
            if is_cancel {
                return Some(Action::DiscardKeyspacePopup);
            }
            self.keyspace.handle_wizard_key(key);
            if self.keyspace.wizard_wants_finalize() {
                return Some(Action::ConfirmKeyspacePopup);
            }
            return None;
        }

        // The in-collection add-item overlay: Enter confirms, Esc cancels, the
        // rest is input (incl. Tab to switch fields).
        if self.keyspace.is_add_item_active() {
            if is_confirm {
                return Some(Action::ConfirmKeyspacePopup);
            }
            if is_cancel {
                return Some(Action::DiscardKeyspacePopup);
            }
            self.keyspace.handle_add_item_key(key);
            return None;
        }

        // The in-collection edit-item overlay: Enter saves, Esc cancels, the
        // rest is input.
        if self.keyspace.is_edit_item_active() {
            if is_confirm {
                return Some(Action::ConfirmKeyspacePopup);
            }
            if is_cancel {
                return Some(Action::DiscardKeyspacePopup);
            }
            self.keyspace.handle_edit_item_key(key);
            return None;
        }

        // Enter/Esc act on whatever modal layer is open before falling back to
        // the configured bindings (where Enter is `EnterValue`).
        if self.keyspace.is_popup() || self.keyspace.is_overlay_open() {
            if is_confirm {
                return Some(Action::ConfirmKeyspacePopup);
            }
            if is_cancel {
                return Some(Action::DiscardKeyspacePopup);
            }
        }

        self.handle_keybindings(key)
    }

    fn handle_keybindings(&mut self, mut key: KeyEvent) -> Option<Action> {
        // Symbol keys like `?` arrive with `SHIFT` set on most terminals, but
        // the config writes them literally (no modifier). Drop a lone `SHIFT`
        // on punctuation/symbol chars so those bindings match.
        if let KeyCode::Char(c) = key.code {
            if !c.is_alphanumeric() && key.modifiers == crossterm::event::KeyModifiers::SHIFT {
                key.modifiers = crossterm::event::KeyModifiers::empty();
            }
        }

        self.pending_keys.push(key);
        self.last_key_at = Some(Instant::now());

        let keybindings = &config::get().keybindings;
        let mut resolution = keybindings.resolve(self.mode, &self.pending_keys);
        if resolution == KeyResolution::NoMatch && self.pending_keys.len() > 1 {
            // The new key broke a pending chord: start over from that key alone.
            self.pending_keys = vec![key];
            resolution = keybindings.resolve(self.mode, &self.pending_keys);
        }

        match resolution {
            KeyResolution::Command(command) => {
                self.pending_keys.clear();
                Some(command.into())
            }
            KeyResolution::Pending => None,
            KeyResolution::NoMatch => {
                self.pending_keys.clear();
                None
            }
        }
    }

    /// Drop a partially typed chord once it has waited [`CHORD_TIMEOUT`].
    fn expire_pending_chord(&mut self) {
        if self
            .last_key_at
            .is_some_and(|at| at.elapsed() >= CHORD_TIMEOUT)
        {
            self.pending_keys.clear();
            self.last_key_at = None;
        }
    }

    fn handle_action(&mut self, action: &Action, tui: &mut tui::Tui) -> Result<Option<Action>> {
        if action != &Action::Tick && action != &Action::Render {
            log::debug!("{action:?}");
        }
        self.maybe_dismiss_help(action);
        match *action {
            Action::Tick => self.expire_pending_chord(),
            Action::Quit => self.should_quit = true,
            Action::Resize(w, h) => self.resize(tui, (w, h))?,
            Action::Render => self.draw(tui)?,
            Action::PreviousMode => self.switch_to_previous_mode(),
            Action::ToggleDashboard => self.toggle_dashboard(),
            Action::NextChart => self.dashboard.next_chart(),
            Action::PrevChart => self.dashboard.prev_chart(),
            Action::ToggleAllCharts => self.dashboard.toggle_all(),
            Action::ToggleMetricsPause => self.dashboard.toggle_pause(),
            Action::ResetMetrics => {
                self.state.metrics.lock().reset();
                self.dashboard.unpause();
            }
            Action::LoadKeySpace => self.load_keyspace(),
            Action::RefreshSpace => self.refresh_space(),
            Action::LoadKeysIntoKeySpace => {
                self.finish_keys_fetch();
                self.load_new_keys();
            }
            Action::KeysLoadAborted => self.finish_keys_fetch(),
            Action::RequestSelectedValue => self.request_selected_value(),
            Action::LoadSelectedValueIntoView => self.load_selected_value(),
            Action::ScrollDown => self.scroll_down(),
            Action::ScrollUp => self.scroll_up(),
            Action::LoadNextPage => self.load_next_page(),
            Action::LoadPreviousPage => self.load_previous_page(),
            Action::SetKeyspaceFilter => self.enter_filter_popup(),
            Action::DiscardKeyspacePopup => self.close_popup(),
            Action::ConfirmKeyspacePopup => self.confirm_popup(),
            Action::DeleteKeyspaceFilter => self.delete_keyspace_filter(),
            Action::EnterValue => self.enter_value(),
            Action::RequestDeleteKey => self.request_delete_key(),
            Action::RequestSetTtl => self.request_set_ttl(),
            Action::RequestEditValue => self.request_edit_value(),
            Action::RequestAddKey => self.request_add_key(),
            Action::RequestAddItem => self.request_add_item(),
            Action::Help => self.toggle_help(),
            Action::Error(ref msg) => self.show_error_popup(msg),
            Action::Refresh => {}
        }

        Ok(None)
    }

    fn draw(&mut self, tui: &mut tui::Tui) -> Result<()> {
        tui.draw(|frame| {
            frame.render_stateful_widget(AppWidget, frame.area(), self);
        })?;
        Ok(())
    }
}

impl StatefulWidget for AppWidget {
    type State = App;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let cfg = config::get();
        Block::default().bg(cfg.colors.base00).render(area, buf);

        let [main, footer] = Layout::vertical([Constraint::Percentage(100), Constraint::Length(5)])
            .flex(Flex::Center)
            .margin(1)
            .areas(area);

        StatefulWidget::render(InfoWidget, footer, buf, &mut state.summary);
        state.render_main_block(main, buf);

        if state.help_is_open() {
            let mode = match state.mode {
                Mode::Popup(_) => state.previous_mode.unwrap_or(Mode::KeySpace),
                m => m,
            };
            App::render_help_overlay(area, buf, mode);
        }
    }
}

/// Render logic
impl App {
    fn render_main_block(&mut self, area: Rect, buf: &mut Buffer) {
        let mode = match self.mode {
            Mode::Popup(_) => self.previous_mode.unwrap_or(Mode::KeySpace),
            m => m,
        };
        match mode {
            Mode::KeySpace => self.render_key_space(area, buf),
            Mode::Dashboard => {
                StatefulWidget::render(DashboardWidget, area, buf, &mut self.dashboard);
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
        let entries: &[(Command, &str)] = if mode == Mode::Dashboard {
            &[
                (Command::ToggleDashboard, "Back to keyspace"),
                (Command::ToggleMetricsPause, "Pause / resume view"),
                (Command::ResetMetrics, "Reset history"),
                (Command::NextChart, "Next chart"),
                (Command::PrevChart, "Previous chart"),
                (Command::ToggleAllCharts, "Toggle all charts"),
                (Command::ToggleHelp, "Toggle this help"),
                (Command::Quit, "Quit"),
            ]
        } else {
            &[
                (Command::ScrollDown, "Scroll down"),
                (Command::ScrollUp, "Scroll up"),
                (Command::LoadNextPage, "Next page"),
                (Command::LoadPreviousPage, "Previous page"),
                (Command::RefreshSpace, "Refresh keyspace"),
                (Command::SetPattern, "Set filter pattern"),
                (Command::DeletePattern, "Delete filter pattern"),
                (Command::EnterValue, "Enter value / confirm"),
                (Command::AddKey, "Add new key"),
                (Command::AddItem, "Add item (in collection)"),
                (Command::EditValue, "Edit value / item (drill in first)"),
                (Command::DeleteKey, "Delete key / item (in value)"),
                (Command::SetTtl, "Set TTL"),
                (Command::ToggleDashboard, "Open metrics dashboard"),
                (Command::ClosePopup, "Close popup / overlay"),
                (Command::ToggleHelp, "Toggle this help"),
                (Command::Quit, "Quit"),
            ]
        };
        let lookup_mode = if mode == Mode::Dashboard {
            Mode::Dashboard
        } else {
            Mode::KeySpace
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

        // "Extend" the box to its content: longest "label    keys" line plus
        // borders and padding, clamped to the available area.
        let content_width = rows
            .iter()
            .map(|(label, keys)| label.len() + keys.len() + 4)
            .max()
            .unwrap_or(0);
        let title = " Help ";
        let inner_width = content_width.max(title.len());
        #[allow(clippy::cast_possible_truncation)]
        let popup_w = (inner_width as u16 + 4).min(area.width);
        #[allow(clippy::cast_possible_truncation)]
        let popup_h = (rows.len() as u16 + 3).min(area.height);

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
            .fg(cfg.colors.base05);

        let label_width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
        let lines: Vec<Line> = rows
            .iter()
            .map(|(label, keys)| {
                Line::from(vec![
                    Span::raw(format!("{label:<label_width$}  ")),
                    Span::styled(keys.clone(), cfg.colors.base04),
                ])
            })
            .collect();

        Clear.render(popup_area, buf);
        Paragraph::new(lines).block(block).render(popup_area, buf);
    }
}

/// Handling events logic
impl App {
    fn switch_to_previous_mode(&mut self) {
        self.mode = self.previous_mode.take().unwrap_or(Mode::KeySpace);
        self.sync_hint();
        if !matches!(self.mode, Mode::Popup(_)) {
            self.error_message = None;
        }
    }

    fn toggle_dashboard(&mut self) {
        match self.mode {
            Mode::Dashboard => self.switch_to_previous_mode(),
            Mode::KeySpace => {
                self.previous_mode = Some(self.mode);
                self.mode = Mode::Dashboard;
                self.sync_hint();
            }
            _ => {}
        }
    }

    /// Point the footer hint at the current mode.
    fn sync_hint(&mut self) {
        let (mode, entries) = if self.mode == Mode::Dashboard {
            (Mode::Dashboard, DASHBOARD_HINT_ENTRIES)
        } else {
            (Mode::KeySpace, KEYSPACE_HINT_ENTRIES)
        };
        self.summary.hint = build_hint(&config::get().keybindings, mode, entries);
    }

    fn show_error_popup(&mut self, msg: &str) {
        self.error_message = Some(msg.to_owned());
        if !matches!(self.mode, Mode::Popup(_)) {
            self.previous_mode = Some(self.mode);
            self.mode = Mode::Popup(PopupMode::Error);
        }
    }

    fn enter_filter_popup(&mut self) {
        self.keyspace.enter_filter_pattern();
    }

    fn toggle_help(&mut self) {
        self.help_opened_at = if self.help_opened_at.is_some() {
            None
        } else {
            Some(Instant::now())
        };
    }

    fn help_is_open(&self) -> bool {
        self.help_opened_at.is_some()
    }

    /// Hide the help overlay when it has been up longer than [`HELP_TIMEOUT`]
    /// (checked on `Tick`) or as soon as the user issues any real command —
    /// `Help` itself toggles it, and `Tick`/`Render` are background noise.
    fn maybe_dismiss_help(&mut self, action: &Action) {
        let Some(opened_at) = self.help_opened_at else {
            return;
        };

        match action {
            // `Help` toggles the overlay; `Tick`/`Render` are background noise.
            // `DiscardKeyspacePopup` (Esc) is handled by `close_popup`, which
            // already gives the help overlay precedence — leave it alone here so
            // a single Esc doesn't also dismiss an underlying keyspace popup.
            Action::Help
            | Action::Render
            | Action::DiscardKeyspacePopup
            | Action::KeysLoadAborted => {}
            Action::Tick => {
                if opened_at.elapsed() >= HELP_TIMEOUT {
                    self.help_opened_at = None;
                }
            }
            _ => self.help_opened_at = None,
        }
    }

    /// Esc backs out one modal layer at a time, in precedence order:
    /// help overlay → action overlay (delete/edit/TTL/notice) → value focus →
    /// filter popup.
    fn close_popup(&mut self) {
        if self.help_is_open() {
            self.help_opened_at = None;
            return;
        }

        if self.keyspace.is_overlay_open() {
            self.keyspace.close_overlay();
            return;
        }

        // The add-key wizard / add-item overlays cancel as a whole on Esc. (Their
        // internal multi-step back-out is handled inside the widget for non-Esc
        // keys; one Esc dismisses the whole flow.)
        if self.keyspace.is_wizard_active() {
            self.keyspace.close_new_key();
            return;
        }

        if self.keyspace.is_add_item_active() {
            self.keyspace.close_add_item();
            return;
        }

        if self.keyspace.is_edit_item_active() {
            self.keyspace.close_edit_item();
            return;
        }

        if self.keyspace.is_value_focused() {
            self.keyspace.exit_value_focus();
            return;
        }

        if !self.keyspace.is_popup() {
            return;
        }

        self.keyspace.exit_popup();
    }

    /// Enter confirms whichever modal is open. Precedence: add-key wizard
    /// finalize → add-item overlay → action overlay (delete/edit/TTL) → filter
    /// popup.
    fn confirm_popup(&mut self) {
        if self.keyspace.is_wizard_active() {
            self.confirm_new_key();
            return;
        }
        if self.keyspace.is_add_item_active() {
            self.confirm_add_item();
            return;
        }
        if self.keyspace.is_edit_item_active() {
            self.confirm_edit_item();
            return;
        }
        if self.keyspace.is_overlay_open() {
            self.confirm_action_overlay();
            return;
        }
        self.set_keyspace_filter();
    }

    /// Finalize the add-key wizard into a `CreateKey` event. Keeps the cursor on
    /// the new key via `reselect_key`. Existence is validated in the storage
    /// layer (surfacing as an `Action::Error` if the key already exists).
    fn confirm_new_key(&mut self) {
        let Some((key, spec)) = self.keyspace.take_new_key_request() else {
            // Nothing valid supplied; leave the wizard closed.
            self.keyspace.close_new_key();
            return;
        };
        self.reselect_key = Some(key.clone());
        self.send_redis_event(RedisEvent::CreateKey { key, spec });
    }

    /// Finalize the in-collection add-item overlay into an `AddItem` event. On
    /// invalid input the widget keeps the overlay open with a notice, so a
    /// `None` here simply means "stay open".
    ///
    /// To keep the user inside the collection after the post-write refresh,
    /// remember the focused key (so the cursor stays put) and the value-table
    /// row (so focus is restored once the larger value reloads).
    fn confirm_add_item(&mut self) {
        // Capture focus state *before* taking the request (which closes the
        // overlay but leaves value focus intact).
        let row = self.keyspace.value_selected_row().unwrap_or(0);
        if let Some((key, item)) = self.keyspace.take_add_item_request() {
            self.reselect_key = Some(key.clone());
            self.restore_value_row = Some(row);
            self.send_redis_event(RedisEvent::AddItem { key, item });
        }
    }

    /// Finalize the in-collection edit-item overlay into an `EditItem` event.
    /// On invalid input the widget keeps the overlay open with a notice, so a
    /// `None` here simply means "stay open". Keeps the user inside the collection
    /// on the same row after the post-write refresh, like [`Self::confirm_add_item`].
    fn confirm_edit_item(&mut self) {
        let row = self.keyspace.value_selected_row().unwrap_or(0);
        if let Some((key, edit)) = self.keyspace.take_edit_item_request() {
            self.reselect_key = Some(key.clone());
            self.restore_value_row = Some(row);
            self.send_redis_event(RedisEvent::EditItem { key, edit });
        }
    }

    /// Translate the confirmed action overlay into a Redis write event.
    fn confirm_action_overlay(&mut self) {
        // Capture the value-table row before taking the request (delete-item
        // leaves value focus intact so the user can be restored to it).
        let row = self.keyspace.value_selected_row().unwrap_or(0);
        let Some(request) = self.keyspace.take_overlay_request() else {
            // Notice-only overlay: nothing to do, it's already been dismissed.
            return;
        };
        let Some((key, _)) = self.keyspace.selected_key() else {
            return;
        };

        let event = match request {
            OverlayRequest::Delete => {
                // The key is going away; let the refresh land on whatever fills
                // its place (default top-of-page) rather than reselecting it.
                self.reselect_key = None;
                RedisEvent::DeleteKey { key }
            }
            OverlayRequest::DeleteItem(delete) => {
                // The key survives; keep the user inside the collection on the
                // same (clamped) row after the post-delete refresh.
                self.reselect_key = Some(key.clone());
                self.restore_value_row = Some(row);
                RedisEvent::DeleteItem { key, delete }
            }
            OverlayRequest::SetString(value) => {
                // Non-destructive: keep the cursor on this key after the refresh.
                self.reselect_key = Some(key.clone());
                RedisEvent::SetString { key, value }
            }
            OverlayRequest::SetTtl(secs) => {
                self.reselect_key = Some(key.clone());
                RedisEvent::SetTtl { key, secs }
            }
        };
        self.send_redis_event(event);
    }

    fn enter_value(&mut self) {
        self.keyspace.enter_value();
    }

    /// `d`: delete. From the key list this deletes the whole selected key; while
    /// drilled into a collection value it deletes only the selected element.
    fn request_delete_key(&mut self) {
        if self.keyspace.selected_key().is_none() {
            return;
        }
        if self.keyspace.is_value_focused() {
            self.keyspace.open_delete_item_confirm();
        } else {
            self.keyspace.open_delete_confirm();
        }
    }

    fn request_set_ttl(&mut self) {
        if self.keyspace.selected_key().is_some() {
            self.keyspace.open_set_ttl();
        }
    }

    /// `e`: edit. STRING opens the whole-value editor. For a collection, the
    /// first `e` drills into the value (so the user sees the elements) and the
    /// second `e` — now value-focused — edits the selected element.
    fn request_edit_value(&mut self) {
        let Some((key, r_type)) = self.keyspace.selected_key() else {
            return;
        };
        match r_type {
            RedisType::List | RedisType::Set | RedisType::Hash | RedisType::Zset => {
                if self.keyspace.is_value_focused() {
                    self.keyspace.open_edit_item(key);
                } else {
                    // Drill in first; a second `e` edits the selected element.
                    self.keyspace.enter_value();
                }
            }
            // STRING edits the whole value; JSON/Unknown fall through to the
            // editor (which shows a notice for non-STRING scalars).
            RedisType::String | RedisType::Json | RedisType::Unknown => {
                self.keyspace.open_edit_value();
            }
        }
    }

    /// `a`: open the add-key wizard. Available from the key list at any time.
    fn request_add_key(&mut self) {
        self.keyspace.open_new_key();
    }

    /// `i`: append an item to the focused collection. No-op unless the user has
    /// drilled into a collection value (`is_value_focused()`), so it can't fire
    /// from the key list.
    fn request_add_item(&mut self) {
        if !self.keyspace.is_value_focused() {
            return;
        }
        if let Some((key, r_type)) = self.keyspace.selected_key() {
            self.keyspace.open_add_item(key, r_type);
        }
    }

    fn send_redis_event(&self, event: RedisEvent) {
        if let Err(err) = self.redis_tx.send(event) {
            log::error!("Failed to send redis event: {err:?}");
        }
    }

    fn set_keyspace_filter(&mut self) {
        if !self.keyspace.is_popup() {
            return;
        }

        let pattern = self.keyspace.confirm_filter_pattern();
        self.state.keyspace_state.lock().set_pattern(pattern);
        self.fetch_keys();
    }

    fn delete_keyspace_filter(&mut self) {
        self.state.keyspace_state.lock().delete_pattern();
        self.fetch_keys();
    }

    fn load_next_page(&mut self) {
        // Paging reuses the sweep: no rescan, only the new page's metadata.
        if self.state.keyspace_state.lock().next_page() {
            self.fetch_keys();
        }
    }

    fn load_previous_page(&mut self) {
        if self.state.keyspace_state.lock().previous_page() {
            self.fetch_keys();
        }
    }

    fn load_new_keys(&mut self) {
        let page_info = PageInfo::from(&*self.state.keyspace_state.lock());
        self.keyspace.set_page_info(page_info);
        self.keyspace.set_keys(self.state.keys.lock().clone());
        self.keyspace.clear_selected_value();

        // After a non-destructive edit, keep the cursor on the edited key and
        // reload its (now updated) value instead of snapping back to the top.
        if let Some(key) = self.reselect_key.take() {
            if self.keyspace.select_key_by_name(&key) {
                // If an add-item is in flight, arm the focus restore for the
                // value load this re-request triggers (the final one).
                if self.restore_value_row.is_some() {
                    self.restore_value_pending = true;
                }
                action::try_send_action(&self.tx, Action::RequestSelectedValue);
            } else {
                // Key vanished (e.g. filtered out); drop the pending restore.
                self.restore_value_row = None;
            }
        }
    }

    fn request_selected_value(&self) {
        let Some((key, r_type)) = self.keyspace.selected_key() else {
            return;
        };
        if let Err(err) = self.redis_tx.send(RedisEvent::FetchValue { key, r_type }) {
            log::error!("Failed to send FetchValue: {err:?}");
        }
    }

    fn load_selected_value(&mut self) {
        let value = self.state.selected_value.lock().clone();
        self.keyspace.set_selected_value(value);

        // After an add-item, re-enter the collection at the row the user was on
        // (clamped to the new length) so adding doesn't bounce focus to the key
        // list. `restore_value_row` is armed by `load_new_keys` only once the
        // post-write keyspace refresh (which clears focus) has already run, so
        // this fires on the *final* value load and isn't undone afterwards.
        if self.restore_value_pending {
            self.restore_value_pending = false;
            if let Some(row) = self.restore_value_row.take() {
                self.keyspace.restore_value_focus(row);
            }
        }
    }

    fn load_keyspace(&mut self) {
        self.fetch_keys();
    }

    /// Re-run the SCAN sweep (picking up writes) while staying on the current
    /// page.
    fn refresh_space(&mut self) {
        self.state.keyspace_state.lock().rescan();
        self.fetch_keys();
    }

    fn fetch_keys(&mut self) {
        self.keyspace.refresh();
        if let Err(err) = self.redis_tx.send(RedisEvent::FetchKeys) {
            log::error!("Failed to send redis event: {err:?}");
            return;
        }
        self.keys_in_flight += 1;
        self.keyspace.set_loading(true);
    }

    fn finish_keys_fetch(&mut self) {
        self.keys_in_flight = self.keys_in_flight.saturating_sub(1);
        self.keyspace.set_loading(self.keys_in_flight > 0);
    }

    fn scroll_down(&mut self) {
        if self.mode != Mode::KeySpace {
            return;
        }
        // When focus is in the value pane, `j`/`k` scroll the value's rows in
        // place — the selected key (and its loaded value) must not change.
        if self.keyspace.is_value_focused() {
            self.keyspace.scroll_next();
            return;
        }
        self.keyspace.scroll_next();
        self.keyspace.clear_selected_value();
        action::try_send_action(&self.tx, Action::RequestSelectedValue);
    }

    fn scroll_up(&mut self) {
        if self.mode != Mode::KeySpace {
            return;
        }
        if self.keyspace.is_value_focused() {
            self.keyspace.scroll_previous();
            return;
        }
        self.keyspace.scroll_previous();
        self.keyspace.clear_selected_value();
        action::try_send_action(&self.tx, Action::RequestSelectedValue);
    }
}
