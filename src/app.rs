use std::time::{Duration, Instant};

use crate::{config, mode::PopupMode, redis_client::event::RedisEvent, state::SharedState};
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
    mode::Mode,
    tui,
    widgets::{
        info::{Info, InfoWidget},
        keyspace::{KeySpace, KeySpaceWidget, OverlayRequest},
    },
};

pub struct AppWidget;

/// How long the `?` help overlay stays up before auto-dismissing.
const HELP_TIMEOUT: Duration = Duration::from_secs(7);

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
    last_tick_key_events: Vec<KeyEvent>,

    tx: mpsc::Sender<Action>,
    rx: mpsc::Receiver<Action>,

    redis_tx: broadcast::Sender<RedisEvent>,
    summary: Info,
    keyspace: KeySpace,
}

impl App {
    pub fn new(
        state: SharedState,
        tx: Sender<Action>,
        rx: Receiver<Action>,
        redis_tx: broadcast::Sender<RedisEvent>,
        tick_rate: f64,
        frame_rate: f64,
    ) -> Self {
        let _ = config::get();

        let mode = Mode::KeySpace;

        let summary = Info::new(state.info.clone());
        let keyspace = KeySpace::new(Vec::new());

        Self {
            state,
            summary,
            keyspace,
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
            last_tick_key_events: Vec::new(),
            tx,
            rx,
            redis_tx,
        }
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
            .cancelation_token(cancellation_token.clone());

        // tui.mouse(true);
        tui.enter()?;

        loop {
            // TODO: refactor with async_channel crate
            // replace with select multiplex
            if let Some(e) = tui.next().await {
                if let Some(action) = self.handle_event(&e) {
                    action::try_send_action(&self.tx, action);
                }
            }

            while let Ok(action) = self.rx.try_recv() {
                if let Some(next) = self.handle_action(&action, &mut tui)? {
                    action::try_send_action(&self.tx, next);
                }
            }
            if self.should_quit {
                tui.stop().await?;
                break;
            }
        }
        tui.exit()?;
        Ok(())
    }

    fn resize(&mut self, tui: &mut tui::Tui, (w, h): (u16, u16)) -> Result<()> {
        tui.resize(Rect::new(0, 0, w, h))?;
        action::try_send_action(&self.tx, Action::Render);

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

        self.last_tick_key_events.push(key);

        config::get()
            .keybindings
            .event_to_command(self.mode, &self.last_tick_key_events)
            .or_else(|| {
                config::get()
                    .keybindings
                    .event_to_command(Mode::Common, &self.last_tick_key_events)
            })
            .map(Into::into)
    }

    fn handle_action(&mut self, action: &Action, tui: &mut tui::Tui) -> Result<Option<Action>> {
        if action != &Action::Tick && action != &Action::Render {
            log::debug!("{action:?}");
        }
        self.maybe_dismiss_help(action);
        match *action {
            Action::Tick => {
                self.last_tick_key_events.drain(..);
            }
            Action::Quit => self.should_quit = true,
            Action::Resize(w, h) => self.resize(tui, (w, h))?,
            Action::Render => self.draw(tui)?,
            Action::PreviousMode => self.switch_to_previous_mode(),
            Action::LoadKeySpace => self.load_keyspace(),
            Action::RefreshSpace => self.refresh_space(),
            Action::LoadKeysIntoKeySpace => self.load_new_keys(),
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
            App::render_help_overlay(area, buf);
        }
    }
}

/// Render logic
impl App {
    fn render_main_block(&mut self, area: Rect, buf: &mut Buffer) {
        if self.mode == Mode::KeySpace {
            self.render_key_space(area, buf);
        }
    }

    fn render_key_space(&mut self, area: Rect, buf: &mut Buffer) {
        StatefulWidget::render(KeySpaceWidget, area, buf, &mut self.keyspace);
    }

    /// Render the `?` help overlay: a centered popup listing every command and
    /// the key sequence(s) bound to it. The box grows ("extends") to fit its
    /// content — both width and height are derived from the rows — and is
    /// clamped to the available area.
    fn render_help_overlay(area: Rect, buf: &mut Buffer) {
        let cfg = config::get();
        let keybindings = &cfg.keybindings;

        // Commands shown in the help, paired with a human-readable label. Each
        // is resolved to its configured key sequences via
        // `KeyBindings::get_config_for_command`.
        let entries: [(Command, &str); 16] = [
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
            (Command::EditValue, "Edit value (string)"),
            (Command::DeleteKey, "Delete key"),
            (Command::SetTtl, "Set TTL"),
            (Command::ClosePopup, "Close popup / overlay"),
            (Command::ToggleHelp, "Toggle this help"),
            (Command::Quit, "Quit"),
        ];

        let rows: Vec<(String, String)> = entries
            .iter()
            .map(|(command, label)| {
                let mut binds = keybindings.get_config_for_command(Mode::KeySpace, *command);
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
        if let Some(ref mut m) = self.previous_mode.or(Some(Mode::KeySpace)) {
            std::mem::swap(&mut self.mode, m);
        }
    }

    fn show_error_popup(&mut self, msg: &str) {
        self.error_message = Some(msg.to_owned());
        self.mode = Mode::Popup(PopupMode::Error);
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
            Action::Help | Action::Render | Action::DiscardKeyspacePopup => {}
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

    /// Translate the confirmed action overlay into a Redis write event.
    fn confirm_action_overlay(&mut self) {
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

    fn request_delete_key(&mut self) {
        if self.keyspace.selected_key().is_some() {
            self.keyspace.open_delete_confirm();
        }
    }

    fn request_set_ttl(&mut self) {
        if self.keyspace.selected_key().is_some() {
            self.keyspace.open_set_ttl();
        }
    }

    fn request_edit_value(&mut self) {
        if self.keyspace.selected_key().is_some() {
            self.keyspace.open_edit_value();
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

        {
            let pattern = self.keyspace.confirm_filter_pattern();
            let mut state = self.state.keyspace_state.lock();
            state.set_pattern(pattern.clone());

            self.keyspace.update_filters(pattern, None);
        }
        self.refresh_space();
    }

    fn delete_keyspace_filter(&mut self) {
        {
            let mut state = self.state.keyspace_state.lock();
            state.delete_pattern();
            self.keyspace.update_filters(None, None);
        }
        self.refresh_space();
    }

    fn load_next_page(&mut self) {
        {
            let mut state = self.state.keyspace_state.lock();
            state.update_cursor();
            self.keyspace
                .update_filters(state.pattern.clone(), state.cursor);
        }
        self.refresh_space();
    }

    fn load_previous_page(&mut self) {
        {
            let mut state = self.state.keyspace_state.lock();
            state.set_previous_cursor();
            self.keyspace
                .update_filters(state.pattern.clone(), state.cursor);
        }
        self.refresh_space();
    }

    fn load_new_keys(&mut self) {
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

    fn load_keyspace(&self) {
        if let Err(err) = self.redis_tx.send(RedisEvent::FetchKeys) {
            log::error!("Failed to send redis event: {err:?}");
        }
    }

    fn refresh_space(&mut self) {
        self.keyspace.refresh();
        if let Err(err) = self.redis_tx.send(RedisEvent::FetchKeys) {
            log::error!("Failed to send redis event: {err:?}");
        }
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
