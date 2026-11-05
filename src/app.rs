use std::time::{Duration, Instant, SystemTime};

use crate::{
    config,
    mode::PopupMode,
    pubsub_notify::{self, Notice},
    redis_client::{event::RedisEvent, pubsub::PubSubCmd, pubsub_store::Target, types::RedisType},
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
use unicode_width::UnicodeWidthStr;

use tokio::sync::{
    broadcast,
    mpsc::{self, Receiver, Sender, UnboundedSender},
};
use tokio_util::sync::CancellationToken;

use crate::{
    action::{self, Action},
    clipboard::{Clipboard, Via},
    command::Command,
    keybindings::KeyResolution,
    mode::Mode,
    tui,
    widgets::{
        dashboard::{Dashboard, DashboardWidget},
        db_picker::{DbPicker, DbPickerWidget},
        info::{build_hint, Info, InfoWidget},
        keyspace::{
            format_bytes, KeySpace, KeySpaceWidget, OverlayRequest, PageInfo, ScrollOutcome,
        },
        pubsub::{Pane, PubSub, PubSubWidget, Row},
        toast::{Toast, ToastWidget},
    },
};

pub struct AppWidget;

/// How long the `?` help overlay stays up before auto-dismissing.
const HELP_TIMEOUT: Duration = Duration::from_secs(7);
/// Values above this size (`MEMORY USAGE`) ask before being copied whole.
const YANK_WARN_BYTES: u128 = 10 * 1024 * 1024;
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
    /// After turning a value page with `j` / `k` / `n` / `p`, the row to select
    /// once the new page loads (`usize::MAX` = last row; clamped).
    page_turn_row: Option<usize>,

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
    db_picker: DbPicker,
    pubsub: PubSub,
    /// Control channel of the Pub/Sub task.
    pubsub_tx: UnboundedSender<PubSubCmd>,
    /// Where `b` / Esc in the Pub/Sub tab goes back to. Kept apart from
    /// `previous_mode`, which an error popup inside the tab overwrites.
    pubsub_return: Mode,
    /// `--read-only`: refuse writes before any input is asked for.
    read_only: bool,
    clipboard: Clipboard,
    /// Short message drawn above every mode and popup.
    toast: Toast,
}

/// Shown when a write is attempted in `--read-only` mode.
const READ_ONLY_NOTICE: &str = "Read-only mode: writes are disabled.";

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

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        state: SharedState,
        tx: Sender<Action>,
        rx: Receiver<Action>,
        redis_tx: broadcast::Sender<RedisEvent>,
        pubsub_tx: UnboundedSender<PubSubCmd>,
        target: String,
        tick_rate: f64,
        frame_rate: f64,
    ) -> Self {
        let _ = config::get();

        let mode = Mode::KeySpace;

        let summary = Info::new(
            state.info.clone(),
            state.metrics.clone(),
            state.db.clone(),
            target,
        );
        let keyspace = KeySpace::new(Vec::new());
        let dashboard = Dashboard::new(state.metrics.clone());
        let pubsub = PubSub::new(state.pubsub.clone());

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
            page_turn_row: None,
            pending_keys: Vec::new(),
            last_key_at: None,
            keys_in_flight: 0,
            tx,
            rx,
            redis_tx,
            db_picker: DbPicker::default(),
            pubsub,
            pubsub_tx,
            pubsub_return: Mode::KeySpace,
            read_only: false,
            clipboard: Clipboard::default(),
            toast: Toast::default(),
        };
        app.sync_hint();
        app
    }

    /// Refuse every write (`--read-only`): show the `RO` badge and drop the
    /// write commands from the footer hint.
    #[must_use]
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self.summary.read_only = read_only;
        self.sync_hint();
        self
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

    fn resize(tui: &mut tui::Tui, (w, h): (u16, u16)) -> Result<()> {
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

        // The DB picker is modal: Enter switches, Esc cancels, the rest moves
        // the selection. Global bindings (Ctrl-c) still work.
        if self.db_picker.is_open() {
            if is_confirm {
                self.confirm_db_picker();
                return None;
            }
            if is_cancel {
                self.db_picker.close();
                return None;
            }
            if self.handle_keybindings(key) == Some(Action::Quit) {
                return Some(Action::Quit);
            }
            self.db_picker.handle_key(key);
            return None;
        }

        if self.mode == Mode::PubSub {
            return self.handle_pubsub_key(key);
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

    /// Keys in the Pub/Sub tab. Popups take raw keys (Esc cancels, Ctrl-c
    /// still quits): Enter subscribes in the add popup, Ctrl-s sends in the
    /// publish popup. Otherwise Esc closes the help overlay first, then keys
    /// go through the bindings.
    fn handle_pubsub_key(&mut self, key: KeyEvent) -> Option<Action> {
        if self.pubsub.is_popup() {
            let publish = self.pubsub.is_publish_popup();
            let ctrl_s = key.code == KeyCode::Char('s')
                && key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Esc => self.pubsub.close_popup(),
                _ if Self::is_quit_key(key) => return Some(Action::Quit),
                KeyCode::Enter if !publish => self.confirm_add_subscription(),
                _ if publish && ctrl_s => self.confirm_publish(),
                _ => self.pubsub.handle_popup_key(key),
            }
            return None;
        }
        if key.code == KeyCode::Esc && self.help_is_open() {
            self.help_opened_at = None;
            return None;
        }
        if key.code == KeyCode::Esc && self.pubsub.is_detail() {
            self.pubsub.close_detail();
            return None;
        }
        self.handle_keybindings(key)
    }

    /// `key` alone is the global quit binding (Ctrl-c).
    fn is_quit_key(key: KeyEvent) -> bool {
        config::get().keybindings.resolve(Mode::Common, &[key])
            == KeyResolution::Command(Command::Quit)
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
            Action::Tick => {
                self.expire_pending_chord();
                self.request_filter_count();
                let now = Instant::now();
                self.toast.expire(now);
                self.pubsub_tick(now, SystemTime::now());
            }
            Action::Quit => self.should_quit = true,
            Action::Resize(w, h) => Self::resize(tui, (w, h))?,
            Action::Render => {
                self.draw(tui)?;
                self.sync_page_size();
            }
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
            Action::ValueLoadFailed => self.keyspace.clear_value_loading(),
            Action::ScrollDown => self.scroll_down(),
            Action::ScrollUp => self.scroll_up(),
            Action::ScrollLeft | Action::ScrollRight => {
                let right = *action == Action::ScrollRight;
                if self.mode == Mode::PubSub {
                    self.pubsub.pan_detail(right);
                } else {
                    self.keyspace.pan_text(right);
                }
            }
            Action::ToggleWrap => {
                if self.mode == Mode::PubSub {
                    self.pubsub.toggle_detail_wrap();
                } else {
                    self.keyspace.toggle_wrap();
                }
            }
            Action::YankValue => self.yank_value(),
            Action::YankKey => self.yank_key(),
            Action::YankReady => self.finish_yank(),
            Action::YankFailed(ref msg) => self.toast.show(msg.clone(), false),
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
            Action::OpenDbPicker => self.open_db_picker(),
            Action::DbSwitched => {
                self.finish_keys_fetch();
                self.fetch_keys();
            }
            Action::Help => self.toggle_help(),
            Action::Error(ref msg) => self.show_error_popup(msg),
            Action::FilterMatchCount {
                ref pattern,
                count,
                complete,
            } => self
                .keyspace
                .set_filter_count(pattern.clone(), count, complete),
            Action::TogglePubSub
            | Action::PubSubAdd
            | Action::PubSubRemove
            | Action::PubSubClear
            | Action::PubSubRefresh
            | Action::PubSubPublish
            | Action::PubSubEnter
            | Action::PubSubFocus
            | Action::PubSubTogglePause
            | Action::PubSubChannelsLoaded
            | Action::PubSubToggleNotify
            | Action::Published { .. } => self.handle_pubsub_action(action),
            Action::Refresh => {}
        }

        Ok(None)
    }

    /// Actions of the Pub/Sub tab.
    fn handle_pubsub_action(&mut self, action: &Action) {
        match *action {
            Action::TogglePubSub => self.toggle_pubsub(),
            Action::PubSubAdd => self.pubsub.open_add(),
            Action::PubSubRemove => self.remove_subscription(),
            Action::PubSubClear => {
                if let Some(index) = self.pubsub.selected_sub() {
                    self.state.pubsub.lock().clear(index);
                    self.pubsub.reset_log();
                }
            }
            Action::PubSubRefresh => self.fetch_pubsub_channels(),
            Action::PubSubPublish => self.pubsub.open_publish(self.read_only),
            Action::PubSubEnter => self.pubsub_enter(),
            Action::PubSubFocus => self.pubsub.toggle_focus(),
            Action::PubSubTogglePause => {
                if !self.pubsub.is_detail() {
                    self.pubsub.toggle_pause();
                }
            }
            Action::PubSubChannelsLoaded => self.pubsub.clamp_selection(),
            Action::PubSubToggleNotify => self.toggle_notify(),
            Action::Published { receivers, .. } => self.toast.show(
                format!(
                    "delivered to {receivers} subscriber{}",
                    if receivers == 1 { "" } else { "s" }
                ),
                false,
            ),
            _ => {}
        }
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

/// Text for the clipboard, what it is (for the toast) and an optional note.
struct Copy {
    text: String,
    what: String,
    note: Option<&'static str>,
}

/// `Copied value of k (12 B)`, with `note` next to the size and how it got
/// there when not via the OS clipboard.
fn copied_toast(what: &str, len: usize, note: Option<&str>, via: Via) -> String {
    let how = if via == Via::Terminal {
        " via terminal (OSC 52)"
    } else {
        ""
    };
    let note = note.map(|n| format!(", {n}")).unwrap_or_default();
    format!("Copied {what} ({}{note}){how}", format_bytes(len))
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
            Mode::PubSub => {
                self.pubsub.cluster_mode = self.cluster_enabled();
                StatefulWidget::render(PubSubWidget, area, buf, &mut self.pubsub);
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
        self.keyspace.cluster_mode = self.cluster_enabled();
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

    /// `b`: open the Pub/Sub tab from the keyspace or dashboard; in the tab,
    /// go back to where it was opened from.
    fn toggle_pubsub(&mut self) {
        match self.mode {
            Mode::PubSub => {
                self.pubsub.close_popup();
                self.pubsub.close_detail();
                self.mode = self.pubsub_return;
                self.sync_hint();
            }
            Mode::KeySpace | Mode::Dashboard => {
                self.pubsub_return = self.mode;
                self.mode = Mode::PubSub;
                self.sync_hint();
                self.mark_selected_read();
                self.summary.pubsub_unread = 0;
                self.fetch_pubsub_channels();
            }
            Mode::Common | Mode::Popup(_) => {}
        }
    }

    /// Enter in the add popup: subscribe to the target typed.
    fn confirm_add_subscription(&mut self) {
        if let Some(target) = self.pubsub.take_add_request() {
            self.subscribe_target(target);
        }
    }

    /// Enter in the tab. Log: open the selected message. List: subscribe to
    /// a server channel (it moves to the subscriptions), or move focus into
    /// a subscription's log.
    fn pubsub_enter(&mut self) {
        if self.pubsub.is_detail() {
            return;
        }
        if self.pubsub.focus() == Pane::Log {
            self.pubsub.open_detail();
            return;
        }
        match self.pubsub.selected_row() {
            Some(Row::Server(name)) => self.subscribe_target(Target::Channel(name)),
            Some(Row::Sub(_)) => self.pubsub.toggle_focus(),
            None => {}
        }
    }

    /// Record the subscription (the UI shows it right away), select it, and
    /// ask the task to subscribe. A duplicate only selects the existing one.
    fn subscribe_target(&mut self, target: Target) {
        let (index, added) = self.state.pubsub.lock().add(target.clone());
        self.pubsub.select(index);
        self.mark_selected_read();
        if added {
            self.send_pubsub_cmd(PubSubCmd::Subscribe(target));
        }
    }

    /// Ctrl-s in the publish popup. Under `--read-only` the popup stays open
    /// with its notice (the runner would reject the write anyway).
    fn confirm_publish(&mut self) {
        if let Some((channel, payload)) = self.pubsub.take_publish_request() {
            self.send_redis_event(RedisEvent::Publish { channel, payload });
        }
    }

    /// Ask the runner for the server's channels (tab open, `r`).
    fn fetch_pubsub_channels(&self) {
        self.send_redis_event(RedisEvent::FetchPubSubChannels);
    }

    /// `d` in the tab: unsubscribe and drop the selected subscription.
    fn remove_subscription(&mut self) {
        let Some(target) = self.pubsub.selected_target() else {
            return;
        };
        self.state.pubsub.lock().remove(&target);
        self.pubsub.reset_log();
        self.pubsub.clamp_selection();
        self.send_pubsub_cmd(PubSubCmd::Unsubscribe(target));
    }

    /// The Pub/Sub tab is on screen (possibly under an error popup).
    fn pubsub_visible(&self) -> bool {
        self.mode == Mode::PubSub
            || (matches!(self.mode, Mode::Popup(_)) && self.previous_mode == Some(Mode::PubSub))
    }

    /// Subscription open in the tab: its messages are being read.
    fn open_subscription(&self) -> Option<usize> {
        self.pubsub_visible()
            .then(|| self.pubsub.selected_sub())
            .flatten()
    }

    /// The subscription selected in the tab has been read.
    fn mark_selected_read(&mut self) {
        if let Some(index) = self.open_subscription() {
            if let Some(sub) = self.state.pubsub.lock().subs.get_mut(index) {
                sub.unread = 0;
            }
        }
    }

    /// `N`: notifications on / off for the selected subscription (also
    /// un-mutes an auto-muted one).
    fn toggle_notify(&mut self) {
        let Some(index) = self.pubsub.selected_sub() else {
            return;
        };
        if let Some(sub) = self.state.pubsub.lock().subs.get_mut(index) {
            sub.notify = sub.notify.toggled();
        }
    }

    /// Notifier, on every `Tick`: count what each subscription received since
    /// the last tick into its unread badge, run its notification state
    /// machine, and toast the notices. The subscription open in the tab
    /// stays read and never toasts.
    fn pubsub_tick(&mut self, now: Instant, wall: SystemTime) {
        let policy = config::get().pubsub.notify.policy();
        let open = self.open_subscription();
        let mut notices: Vec<Notice> = Vec::new();
        let mut unread = 0;
        {
            let mut store = self.state.pubsub.lock();
            for (i, sub) in store.subs.iter_mut().enumerate() {
                let new = sub.stats.total.saturating_sub(sub.seen_total);
                sub.seen_total = sub.stats.total;
                let rate = f64::from(sub.stats.rate_1s(wall));
                let preview = (new > 0
                    && matches!(sub.notify, pubsub_notify::NotifyState::On { .. }))
                .then(|| sub.newest_preview())
                .flatten();
                let notice = pubsub_notify::on_tick(
                    &mut sub.notify,
                    sub.target.name(),
                    new,
                    rate,
                    preview.as_deref(),
                    now,
                    &policy,
                );
                if open == Some(i) {
                    sub.unread = 0;
                    continue;
                }
                sub.unread += new;
                unread += sub.unread;
                notices.extend(notice);
            }
        }
        self.summary.pubsub_unread = if self.pubsub_visible() { 0 } else { unread };
        for msg in pubsub_notify::toasts(notices) {
            self.toast.notify(msg);
        }
    }

    fn send_pubsub_cmd(&self, cmd: PubSubCmd) {
        if let Err(err) = self.pubsub_tx.send(cmd) {
            log::error!("Failed to send Pub/Sub command: {err:?}");
        }
    }

    /// Point the footer hint at the current mode.
    fn sync_hint(&mut self) {
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

    /// In `--read-only` mode, show a notice and return `true` so the caller
    /// stops before opening any write UI.
    fn deny_write(&mut self) -> bool {
        if self.read_only {
            self.keyspace.show_notice(READ_ONLY_NOTICE);
        }
        self.read_only
    }

    /// The latest `INFO` sample says the server runs in cluster mode.
    fn cluster_enabled(&self) -> bool {
        self.state
            .metrics
            .lock()
            .latest_snapshot
            .as_ref()
            .is_some_and(|s| s.cluster_enabled)
    }

    /// `s`: open the DB picker. Cluster mode only has db 0.
    fn open_db_picker(&mut self) {
        if self.mode != Mode::KeySpace {
            return;
        }
        if self.cluster_enabled() {
            self.keyspace.show_notice("Cluster mode only has db 0.");
            return;
        }
        let keyspace = self
            .state
            .info
            .lock()
            .as_ref()
            .map(|info| info.keyspace.clone())
            .unwrap_or_default();
        let db_count = *self.state.db_count.lock();
        let current = *self.state.db.lock();
        self.db_picker.open(db_count, current, &keyspace);
    }

    /// Enter in the DB picker: ask the runner to switch. The old DB's keys are
    /// cleared right away so nothing stale can be edited; the runner answers
    /// with `DbSwitched` (or an error, which restores the old keys).
    fn confirm_db_picker(&mut self) {
        let selected = self.db_picker.selected();
        self.db_picker.close();
        let Some(db) = selected else {
            return;
        };
        if db == *self.state.db.lock() {
            return;
        }
        self.reselect_key = None;
        self.restore_value_row = None;
        self.restore_value_pending = false;
        self.page_turn_row = None;
        self.keyspace.clear_for_db_switch();
        self.send_redis_event(RedisEvent::SelectDb(db));
        self.keys_in_flight += 1;
        self.keyspace.set_loading(true);
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
        self.request_filter_count();
    }

    /// Ask the runner to count the filter popup's matches once its text has
    /// settled (#24).
    fn request_filter_count(&mut self) {
        if let Some(pattern) = self.keyspace.filter_count_request(Instant::now()) {
            self.send_redis_event(RedisEvent::CountMatches { pattern });
        }
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
            | Action::KeysLoadAborted
            | Action::DbSwitched
            | Action::ValueLoadFailed
            | Action::FilterMatchCount { .. }
            | Action::YankReady
            | Action::YankFailed(_) => {}
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

        if self.keyspace.is_value_focused() || self.keyspace.is_text_focused() {
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
        if let OverlayRequest::Yank { key, r_type } = request {
            self.start_full_yank(key, r_type);
            return;
        }
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
            OverlayRequest::Yank { .. } => return,
        };
        self.send_redis_event(event);
    }

    fn enter_value(&mut self) {
        self.keyspace.enter_value();
    }

    /// `d`: delete. From the key list this deletes the whole selected key; while
    /// drilled into a collection value it deletes only the selected element.
    fn request_delete_key(&mut self) {
        if self.keyspace.selected_key().is_none() || self.deny_write() {
            return;
        }
        if self.keyspace.is_value_focused() {
            self.keyspace.open_delete_item_confirm();
        } else {
            self.keyspace.open_delete_confirm();
        }
    }

    fn request_set_ttl(&mut self) {
        if self.keyspace.selected_key().is_some() && !self.deny_write() {
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
                    if !self.deny_write() {
                        self.keyspace.open_edit_item(key);
                    }
                } else {
                    // Drill in first; a second `e` edits the selected element.
                    self.keyspace.enter_value();
                }
            }
            // STRING edits the whole value; JSON/Unknown fall through to the
            // editor (which shows a notice for non-STRING scalars).
            RedisType::String | RedisType::Json | RedisType::Stream | RedisType::Unknown(_) => {
                if !self.deny_write() {
                    self.keyspace.open_edit_value();
                }
            }
        }
    }

    /// `a`: open the add-key wizard. Available from the key list at any time.
    fn request_add_key(&mut self) {
        if !self.deny_write() {
            self.keyspace.open_new_key();
        }
    }

    /// `i`: append an item to the focused collection. No-op unless the user has
    /// drilled into a collection value (`is_value_focused()`), so it can't fire
    /// from the key list.
    fn request_add_item(&mut self) {
        if !self.keyspace.is_value_focused() || self.deny_write() {
            return;
        }
        if let Some((key, r_type)) = self.keyspace.selected_key() {
            self.keyspace.open_add_item(key, r_type);
        }
    }

    /// `y`: copy the selected element (inside a collection) or the selected
    /// key's whole value (#27). A value that isn't fully loaded is fetched in
    /// the background; above [`YANK_WARN_BYTES`] the user confirms first.
    fn yank_value(&mut self) {
        if self.mode == Mode::PubSub {
            self.yank_pubsub_payload();
            return;
        }
        if self.mode != Mode::KeySpace {
            return;
        }
        let Some((key, r_type)) = self.keyspace.selected_key() else {
            return;
        };
        if self.keyspace.is_value_focused() {
            if let Some(text) = self.keyspace.selected_element_text() {
                self.copy_text(&text, &format!("item of {key}"));
            }
            return;
        }
        if let Some(text) = self.keyspace.loaded_value_text() {
            self.copy_text(&text, &format!("value of {key}"));
            return;
        }
        if matches!(r_type, RedisType::Stream | RedisType::Unknown(_)) {
            self.toast.show(
                format!("Copying {} values isn't supported yet", r_type.name()),
                false,
            );
            return;
        }
        match self.keyspace.selected_size() {
            Some(size) if size > YANK_WARN_BYTES => {
                self.keyspace.open_yank_confirm(key, r_type, size);
            }
            _ => self.start_full_yank(key, r_type),
        }
    }

    /// Ask the runner for `key`'s whole value; `YankReady` copies it.
    fn start_full_yank(&mut self, key: String, r_type: RedisType) {
        self.toast.show(format!("Copying {key}…"), true);
        self.send_redis_event(RedisEvent::FetchFullValue { key, r_type });
    }

    /// The runner left a whole value in `SharedState::yank`: copy it.
    fn finish_yank(&mut self) {
        let Some((key, text)) = self.state.yank.lock().take() else {
            return;
        };
        self.copy_text(&text, &format!("value of {key}"));
    }

    /// `Shift-y`: copy the selected key's name.
    fn yank_key(&mut self) {
        if self.mode == Mode::PubSub {
            if let Some(copy) = self.pubsub_copy(false) {
                self.copy_text_noted(&copy.text, &copy.what, copy.note);
            }
            return;
        }
        if self.mode != Mode::KeySpace {
            return;
        }
        if let Some((key, _)) = self.keyspace.selected_key() {
            self.copy_text(&key, &format!("key name {key}"));
        }
    }

    /// `y` in the Pub/Sub tab: copy the selected message's stored payload
    /// (binary as hex). A payload cut at `max_payload` says so.
    fn yank_pubsub_payload(&mut self) {
        if let Some(copy) = self.pubsub_copy(true) {
            self.copy_text_noted(&copy.text, &copy.what, copy.note);
        }
    }

    /// What `y` (`payload`) / `Shift-y` copy in the Pub/Sub tab: the
    /// selected message's payload or channel.
    fn pubsub_copy(&self, payload: bool) -> Option<Copy> {
        let msg = self.pubsub.selected_message()?;
        Some(if payload {
            Copy {
                text: msg.payload.copy_text(),
                what: format!("payload of {}", msg.channel),
                note: msg.truncated.then_some("truncated"),
            }
        } else {
            Copy {
                what: format!("channel {}", msg.channel),
                text: msg.channel,
                note: None,
            }
        })
    }

    /// Put `text` on the clipboard and confirm with a toast.
    fn copy_text(&mut self, text: &str, what: &str) {
        self.copy_text_noted(text, what, None);
    }

    /// [`Self::copy_text`] with a `note` next to the size, e.g.
    /// `Copied payload of c (64.00 KiB, truncated)`.
    fn copy_text_noted(&mut self, text: &str, what: &str, note: Option<&str>) {
        let msg = match self.clipboard.copy(text) {
            Ok(via) => copied_toast(what, text.len(), note, via),
            Err(err) => format!("Copy failed: {err}"),
        };
        self.toast.show(msg, false);
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

    /// `n`: next page of the value when drilled into one, else of the keyspace.
    fn load_next_page(&mut self) {
        if self.keyspace.is_text_focused() {
            self.keyspace.scroll_text_page(true);
            return;
        }
        if self.keyspace.is_value_focused() {
            self.turn_value_page(true, 0);
            return;
        }
        // Paging reuses the sweep: no rescan, only the new page's metadata.
        if self.state.keyspace_state.lock().next_page() {
            self.fetch_keys();
        }
    }

    /// `p`: previous page of the value when drilled into one, else of the
    /// keyspace.
    fn load_previous_page(&mut self) {
        if self.keyspace.is_text_focused() {
            self.keyspace.scroll_text_page(false);
            return;
        }
        if self.keyspace.is_value_focused() {
            self.turn_value_page(false, 0);
            return;
        }
        if self.state.keyspace_state.lock().previous_page() {
            self.fetch_keys();
        }
    }

    /// Move the selected value one page forward / back and fetch it, selecting
    /// `row` once it lands. Ignored while a page is already loading, so holding
    /// `j` can't skip pages the user never saw.
    fn turn_value_page(&mut self, forward: bool, row: usize) {
        if self.keyspace.is_value_loading() {
            return;
        }
        let moved = {
            let mut value_state = self.state.value_state.lock();
            if forward {
                value_state.next_page()
            } else {
                value_state.previous_page()
            }
        };
        if moved {
            self.page_turn_row = Some(row);
            self.keyspace.set_value_loading();
            self.send_redis_event(RedisEvent::FetchValue);
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

    /// Fetch the selected key's value. Re-requesting the same key (e.g. after a
    /// write) keeps its current page; a different key starts at page 1.
    fn request_selected_value(&mut self) {
        let Some((key, r_type)) = self.keyspace.selected_key() else {
            return;
        };
        self.state.value_state.lock().select(&key, r_type);
        self.page_turn_row = None;
        self.keyspace.set_value_loading();
        self.send_redis_event(RedisEvent::FetchValue);
    }

    fn load_selected_value(&mut self) {
        let value = self.state.selected_value.lock().clone();
        self.keyspace.set_selected_value(value);

        if let Some(row) = self.page_turn_row.take() {
            if self.keyspace.is_value_focused() {
                self.keyspace.restore_value_focus(row);
            }
        }

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

    /// Size keyspace pages to the rows the table can show (#45). Runs after
    /// each draw, so a resize (or the cluster banner appearing) re-pages the
    /// sweep. The selected key stays selected and on screen.
    fn sync_page_size(&mut self) {
        if matches!(self.mode, Mode::Dashboard | Mode::PubSub) {
            return;
        }
        let rows = self.keyspace.visible_rows();
        if rows == 0 {
            return;
        }
        let changed = {
            let mut keyspace_state = self.state.keyspace_state.lock();
            let anchor =
                keyspace_state.page_range().start + self.keyspace.selected_row().unwrap_or(0);
            keyspace_state.set_page_size(rows, anchor)
        };
        if !changed {
            return;
        }
        if let Some((key, _)) = self.keyspace.selected_key() {
            self.reselect_key = Some(key);
        }
        self.fetch_keys();
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

    /// `j` / `k` in the Pub/Sub tab: scroll the open detail, else move in
    /// the focused pane (the log pauses when moving up).
    fn pubsub_move(&mut self, down: bool) {
        if self.pubsub.is_detail() {
            self.pubsub.scroll_detail(down);
            return;
        }
        match (self.pubsub.focus(), down) {
            (Pane::Log, true) => self.pubsub.log_down(),
            (Pane::Log, false) => self.pubsub.log_up(),
            (Pane::List, true) => {
                self.pubsub.select_next();
                self.mark_selected_read();
            }
            (Pane::List, false) => {
                self.pubsub.select_prev();
                self.mark_selected_read();
            }
        }
    }

    fn scroll_down(&mut self) {
        if self.mode == Mode::PubSub {
            self.pubsub_move(true);
            return;
        }
        if self.mode != Mode::KeySpace {
            return;
        }
        // When focus is in the value pane, `j`/`k` scroll the value's rows in
        // place — the selected key (and its loaded value) must not change.
        if self.keyspace.is_text_focused() {
            self.keyspace.scroll_next();
            return;
        }
        if self.keyspace.is_value_focused() {
            // On the last row of a value page, `j` turns to the next page.
            if self.keyspace.scroll_next() == ScrollOutcome::PastEnd {
                self.turn_value_page(true, 0);
            }
            return;
        }
        self.keyspace.scroll_next();
        self.keyspace.clear_selected_value();
        action::try_send_action(&self.tx, Action::RequestSelectedValue);
    }

    fn scroll_up(&mut self) {
        if self.mode == Mode::PubSub {
            self.pubsub_move(false);
            return;
        }
        if self.mode != Mode::KeySpace {
            return;
        }
        if self.keyspace.is_text_focused() {
            self.keyspace.scroll_previous();
            return;
        }
        if self.keyspace.is_value_focused() {
            // On the first row of a value page, `k` turns back and lands on the
            // previous page's last row.
            if self.keyspace.scroll_previous() == ScrollOutcome::PastStart {
                self.turn_value_page(false, usize::MAX);
            }
            return;
        }
        self.keyspace.scroll_previous();
        self.keyspace.clear_selected_value();
        action::try_send_action(&self.tx, Action::RequestSelectedValue);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redis_client::pubsub_store::Target;

    fn app() -> App {
        app_with_pubsub().0
    }

    fn app_with_pubsub() -> (App, mpsc::UnboundedReceiver<PubSubCmd>) {
        let (tx, rx) = mpsc::channel(8);
        let (redis_tx, _) = broadcast::channel(8);
        let (pubsub_tx, pubsub_rx) = mpsc::unbounded_channel();
        let app = App::new(
            SharedState::default(),
            tx,
            rx,
            redis_tx,
            pubsub_tx,
            "redis://localhost:6379".into(),
            4.0,
            30.0,
        );
        (app, pubsub_rx)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, crossterm::event::KeyModifiers::empty())
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
    fn pubsub_tab_back_survives_an_error_popup() {
        let mut app = app();
        app.toggle_dashboard();
        app.toggle_pubsub();
        assert_eq!(app.mode, Mode::PubSub);

        app.show_error_popup("Pub/Sub connection lost");
        app.switch_to_previous_mode();
        assert_eq!(app.mode, Mode::PubSub);

        assert_eq!(
            app.handle_key_event(key(KeyCode::Esc)),
            Some(Action::TogglePubSub)
        );
        app.toggle_pubsub();
        assert_eq!(app.mode, Mode::Dashboard);
    }

    #[test]
    fn add_popup_subscribes_once_and_remove_unsubscribes() {
        let (mut app, mut cmds) = app_with_pubsub();
        app.toggle_pubsub();
        for _ in 0..2 {
            app.pubsub.open_add();
            for c in "orders:*".chars() {
                assert_eq!(app.handle_key_event(key(KeyCode::Char(c))), None);
            }
            assert_eq!(app.handle_key_event(key(KeyCode::Enter)), None);
        }
        let target = Target::Pattern("orders:*".into());
        assert_eq!(cmds.try_recv(), Ok(PubSubCmd::Subscribe(target.clone())));
        assert!(cmds.try_recv().is_err(), "duplicate must not resubscribe");
        assert_eq!(app.state.pubsub.lock().subs.len(), 1);

        app.remove_subscription();
        assert_eq!(cmds.try_recv(), Ok(PubSubCmd::Unsubscribe(target)));
        assert!(app.state.pubsub.lock().subs.is_empty());
    }

    fn app_with_channels() -> (
        App,
        mpsc::UnboundedReceiver<PubSubCmd>,
        broadcast::Receiver<RedisEvent>,
    ) {
        let (mut app, cmds) = app_with_pubsub();
        let (redis_tx, redis_rx) = broadcast::channel(8);
        app.redis_tx = redis_tx;
        (app, cmds, redis_rx)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), crossterm::event::KeyModifiers::CONTROL)
    }

    #[test]
    fn opening_the_tab_lists_server_channels() {
        let (mut app, _, mut events) = app_with_channels();
        app.toggle_pubsub();
        assert!(matches!(
            events.try_recv(),
            Ok(RedisEvent::FetchPubSubChannels)
        ));
    }

    #[test]
    fn enter_on_a_server_channel_subscribes_it() {
        use crate::redis_client::pubsub_store::ServerChannels;

        let (mut app, mut cmds, _events) = app_with_channels();
        app.toggle_pubsub();
        app.state.pubsub.lock().set_server_channels(ServerChannels {
            channels: vec![("events".into(), 1)],
            total: 1,
            patterns: 0,
        });
        // An empty subscription list: the server channel is row 0.
        assert_eq!(
            app.handle_key_event(key(KeyCode::Enter)),
            Some(Action::PubSubEnter)
        );
        app.pubsub_enter();
        let target = Target::Channel("events".into());
        assert_eq!(cmds.try_recv(), Ok(PubSubCmd::Subscribe(target.clone())));
        let store = app.state.pubsub.lock();
        assert_eq!(store.targets(), vec![target]);
        assert_eq!(store.server_only_channels().count(), 0);
        drop(store);
        assert_eq!(app.pubsub.selected_sub(), Some(0));
    }

    #[test]
    fn publish_popup_sends_on_ctrl_s() {
        let (mut app, _, mut events) = app_with_channels();
        app.toggle_pubsub();
        let _ = events.try_recv();
        app.pubsub.open_publish(false);
        for c in "orders:new".chars() {
            assert_eq!(app.handle_key_event(key(KeyCode::Char(c))), None);
        }
        assert_eq!(app.handle_key_event(key(KeyCode::Tab)), None);
        assert_eq!(app.handle_key_event(key(KeyCode::Char('x'))), None);
        assert_eq!(app.handle_key_event(ctrl('s')), None);
        match events.try_recv() {
            Ok(RedisEvent::Publish { channel, payload }) => {
                assert_eq!((channel.as_str(), payload.as_str()), ("orders:new", "x"));
            }
            other => panic!("expected Publish, got {other:?}"),
        }
        assert!(!app.pubsub.is_popup());
    }

    #[test]
    fn read_only_publish_sends_nothing() {
        let (app, _, mut events) = app_with_channels();
        let mut app = app.read_only(true);
        app.toggle_pubsub();
        let _ = events.try_recv();
        assert!(
            !app.summary.hint.contains("publish"),
            "{}",
            app.summary.hint
        );
        app.pubsub.open_publish(app.read_only);
        for c in "chan".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }
        app.handle_key_event(ctrl('s'));
        assert!(events.try_recv().is_err());
        assert!(app.pubsub.is_publish_popup(), "stays open with its notice");
    }

    #[test]
    fn error_popup_returns_to_the_screen_it_opened_on() {
        let mut app = app();
        assert_eq!(app.mode, Mode::KeySpace);

        app.toggle_dashboard();
        assert_eq!(app.mode, Mode::Dashboard);

        app.show_error_popup("boom");
        assert_eq!(app.mode, Mode::Popup(PopupMode::Error));

        app.switch_to_previous_mode();
        assert_eq!(app.mode, Mode::Dashboard);
        assert!(app.error_message.is_none());

        app.toggle_dashboard();
        assert_eq!(app.mode, Mode::KeySpace);
    }

    /// An app with subscriptions `names` (notifications on for each).
    fn app_with_subs(names: &[&str]) -> App {
        let app = app();
        {
            let mut store = app.state.pubsub.lock();
            for name in names {
                let (i, _) = store.add(Target::Channel((*name).into()));
                store.subs[i].notify = store.subs[i].notify.toggled();
            }
        }
        app
    }

    fn publish(app: &App, channel: &str, payload: &str) {
        app.state
            .pubsub
            .lock()
            .push(channel, None, payload.as_bytes(), SystemTime::now());
    }

    fn unread(app: &App) -> Vec<u64> {
        app.state
            .pubsub
            .lock()
            .subs
            .iter()
            .map(|s| s.unread)
            .collect()
    }

    fn render_screen(app: &mut App) -> String {
        let area = Rect::new(0, 0, 120, 40);
        let mut buf = Buffer::empty(area);
        AppWidget.render(area, &mut buf, app);
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    + "\n"
            })
            .collect()
    }

    #[test]
    fn notifier_toasts_first_message_then_only_counts() {
        let mut app = app_with_subs(&["chat"]);
        let start = Instant::now();
        app.pubsub_tick(start, SystemTime::now());
        assert_eq!(app.toast.message(), None);

        publish(&app, "chat", "hi");
        app.pubsub_tick(start, SystemTime::now());
        assert_eq!(app.toast.message(), Some("chat: hi"));
        assert_eq!(unread(&app), vec![1]);
        assert_eq!(app.summary.pubsub_unread, 1);

        // Inside the cooldown: the badge grows, no new toast.
        app.toast.expire(start + Duration::from_secs(5));
        publish(&app, "chat", "again");
        publish(&app, "chat", "and again");
        app.pubsub_tick(start + Duration::from_secs(5), SystemTime::now());
        assert_eq!(app.toast.message(), None);
        assert_eq!(unread(&app), vec![3]);
        assert!(render_screen(&mut app).contains("PubSub 3"));

        // Off: still counted, never toasted.
        app.toggle_pubsub();
        app.toggle_notify();
        app.toggle_pubsub();
        publish(&app, "chat", "later");
        app.pubsub_tick(start + Duration::from_mins(1), SystemTime::now());
        assert_eq!(app.toast.message(), None);
        assert_eq!(unread(&app), vec![1], "opening the tab read it");
    }

    #[test]
    fn several_subscriptions_share_one_toast() {
        let mut app = app_with_subs(&["a", "b", "c"]);
        for name in ["a", "b", "c"] {
            publish(&app, name, "x");
        }
        app.pubsub_tick(Instant::now(), SystemTime::now());
        assert_eq!(
            app.toast.message(),
            Some("3 subscriptions have new messages")
        );
        app.toast.expire(Instant::now() + Duration::from_secs(10));
        assert_eq!(app.toast.message(), None, "nothing else queued");
    }

    #[test]
    fn pubsub_toast_never_replaces_a_sticky_toast() {
        let mut app = app_with_subs(&["chat"]);
        app.toast.show("Copying key…", true);
        publish(&app, "chat", "hi");
        app.pubsub_tick(Instant::now(), SystemTime::now());
        assert_eq!(app.toast.message(), Some("Copying key…"));
        assert_eq!(unread(&app), vec![1]);
    }

    #[test]
    fn unread_resets_when_selected_in_the_tab() {
        let mut app = app_with_subs(&["a", "b"]);
        publish(&app, "a", "x");
        publish(&app, "b", "x");
        publish(&app, "b", "y");
        app.pubsub_tick(Instant::now(), SystemTime::now());
        assert_eq!(unread(&app), vec![1, 2]);
        assert_eq!(app.summary.pubsub_unread, 3);

        app.toggle_pubsub();
        assert_eq!(unread(&app), vec![0, 2], "the first one is selected");
        assert_eq!(app.summary.pubsub_unread, 0, "no global badge in the tab");
        let text = render_screen(&mut app);
        assert!(text.contains("(2)"), "{text}");
        assert!(!text.contains("PubSub 2"), "{text}");

        app.scroll_down();
        assert_eq!(unread(&app), vec![0, 0]);
    }

    #[test]
    fn open_subscription_never_toasts() {
        let mut app = app_with_subs(&["a", "b"]);
        app.toggle_pubsub();
        publish(&app, "a", "x");
        app.pubsub_tick(Instant::now(), SystemTime::now());
        assert_eq!(app.toast.message(), None);
        assert_eq!(unread(&app), vec![0, 0]);

        // Another subscription toasts, even in the tab.
        publish(&app, "b", "y");
        app.pubsub_tick(Instant::now(), SystemTime::now());
        assert_eq!(app.toast.message(), Some("b: y"));
        assert_eq!(unread(&app), vec![0, 1]);
    }

    #[test]
    fn clear_restarts_the_count() {
        let mut app = app_with_subs(&["a"]);
        publish(&app, "a", "x");
        app.pubsub_tick(Instant::now(), SystemTime::now());
        app.state.pubsub.lock().clear(0);
        publish(&app, "a", "y");
        app.pubsub_tick(Instant::now(), SystemTime::now());
        assert_eq!(unread(&app), vec![1]);
    }

    #[test]
    fn toggle_notify_cycles_and_unmutes() {
        use crate::pubsub_notify::NotifyState;
        let mut app = app_with_subs(&["a"]);
        app.toggle_pubsub();
        let state = |app: &App| app.state.pubsub.lock().subs[0].notify;
        assert!(matches!(state(&app), NotifyState::On { .. }));
        app.toggle_notify();
        assert_eq!(state(&app), NotifyState::Off);
        app.state.pubsub.lock().subs[0].notify = NotifyState::AutoMuted;
        assert!(render_screen(&mut app).contains('🔕'));
        app.toggle_notify();
        assert!(matches!(state(&app), NotifyState::On { .. }));
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

    /// Press `code` in the app: the key goes through the key map, and the
    /// resulting action runs the same App method `handle_action` would
    /// (that one needs a terminal).
    fn press(app: &mut App, code: KeyCode, modifiers: crossterm::event::KeyModifiers) {
        let Some(action) = app.handle_key_event(KeyEvent::new(code, modifiers)) else {
            return;
        };
        match action {
            Action::PubSubFocus => app.pubsub.toggle_focus(),
            Action::PubSubTogglePause => app.pubsub.toggle_pause(),
            Action::PubSubEnter => app.pubsub_enter(),
            Action::ScrollDown => app.scroll_down(),
            Action::ScrollUp => app.scroll_up(),
            Action::TogglePubSub => app.toggle_pubsub(),
            other => panic!("unexpected action {other:?}"),
        }
    }

    fn tap(app: &mut App, code: KeyCode) {
        press(app, code, crossterm::event::KeyModifiers::empty());
    }

    /// The tab open on `orders:*` with three JSON messages.
    fn app_on_orders() -> App {
        let mut app = app();
        app.toggle_pubsub();
        app.subscribe_target(Target::Pattern("orders:*".into()));
        let mut store = app.state.pubsub.lock();
        for i in 1..=3 {
            store.push(
                "orders:new",
                Some("orders:*"),
                format!(r#"{{"id":{i}}}"#).as_bytes(),
                SystemTime::now(),
            );
        }
        drop(store);
        app
    }

    #[test]
    fn tab_space_and_enter_drive_the_log() {
        use crate::widgets::pubsub::Pane;

        let mut app = app_on_orders();
        assert_eq!(app.pubsub.focus(), Pane::List);
        tap(&mut app, KeyCode::Tab);
        assert_eq!(app.pubsub.focus(), Pane::Log);

        tap(&mut app, KeyCode::Char(' '));
        assert!(app.pubsub.log_cursor().is_paused());
        app.state
            .pubsub
            .lock()
            .push("orders:new", Some("orders:*"), b"4", SystemTime::now());
        assert!(render_screen(&mut app).contains("paused · +1 new"));
        tap(&mut app, KeyCode::Char(' '));
        assert!(!app.pubsub.log_cursor().is_paused());

        // k pauses on the previous row; j twice moves past the end and follows.
        tap(&mut app, KeyCode::Char('k'));
        assert!(app.pubsub.log_cursor().is_paused());
        tap(&mut app, KeyCode::Char('j'));
        assert!(app.pubsub.log_cursor().is_paused());
        tap(&mut app, KeyCode::Char('j'));
        assert!(!app.pubsub.log_cursor().is_paused());

        tap(&mut app, KeyCode::Tab);
        assert_eq!(app.pubsub.focus(), Pane::List);
        // Enter on a subscription in the list focuses its log.
        tap(&mut app, KeyCode::Enter);
        assert_eq!(app.pubsub.focus(), Pane::Log);
        tap(&mut app, KeyCode::Char('k'));
        tap(&mut app, KeyCode::Enter);
        assert!(app.pubsub.is_detail());
        let text = render_screen(&mut app);
        assert!(text.contains("\"id\": 3"), "{text}");
    }

    #[test]
    fn esc_closes_the_detail_before_leaving_the_tab() {
        let mut app = app_on_orders();
        tap(&mut app, KeyCode::Tab);
        tap(&mut app, KeyCode::Enter);
        assert!(app.pubsub.is_detail());
        tap(&mut app, KeyCode::Esc);
        assert!(!app.pubsub.is_detail());
        assert_eq!(app.mode, Mode::PubSub);
        tap(&mut app, KeyCode::Esc);
        assert_eq!(app.mode, Mode::KeySpace);

        // `b` leaves at once and drops the detail.
        app.toggle_pubsub();
        tap(&mut app, KeyCode::Enter);
        assert!(app.pubsub.is_detail());
        tap(&mut app, KeyCode::Char('b'));
        assert_eq!(app.mode, Mode::KeySpace);
        assert!(!app.pubsub.is_detail());
    }

    #[test]
    fn y_and_shift_y_pick_the_payload_and_channel() {
        let mut app = app_on_orders();
        assert_eq!(
            app.handle_key_event(key(KeyCode::Char('y'))),
            Some(Action::YankValue)
        );
        assert_eq!(
            app.handle_key_event(KeyEvent::new(
                KeyCode::Char('Y'),
                crossterm::event::KeyModifiers::SHIFT
            )),
            Some(Action::YankKey)
        );
        // Following: the newest message.
        let copy = app.pubsub_copy(true).unwrap();
        assert_eq!(copy.text, r#"{"id":3}"#);
        assert_eq!(copy.what, "payload of orders:new");
        assert_eq!(copy.note, None);
        let copy = app.pubsub_copy(false).unwrap();
        assert_eq!(
            (copy.text.as_str(), copy.what.as_str()),
            ("orders:new", "channel orders:new")
        );

        // The selected row, not the newest one.
        tap(&mut app, KeyCode::Tab);
        tap(&mut app, KeyCode::Char('k'));
        assert_eq!(app.pubsub_copy(true).unwrap().text, r#"{"id":2}"#);

        // A payload cut at `max_payload` says so.
        let big = "x".repeat(config::get().pubsub.max_payload() + 10);
        app.state.pubsub.lock().push(
            "orders:big",
            Some("orders:*"),
            big.as_bytes(),
            SystemTime::now(),
        );
        tap(&mut app, KeyCode::Char(' '));
        let copy = app.pubsub_copy(true).unwrap();
        assert_eq!(copy.note, Some("truncated"));
        assert_eq!(copy.text.len(), config::get().pubsub.max_payload());
        assert_eq!(
            copied_toast(&copy.what, 65_536, copy.note, Via::System),
            "Copied payload of orders:big (64.00 KiB, truncated)"
        );
        assert_eq!(
            copied_toast("key name k", 1, None, Via::Terminal),
            "Copied key name k (1 B) via terminal (OSC 52)"
        );
    }
}
