use crate::{config, mode::PopupMode, redis_client::event::RedisEvent, state::SharedState};
use color_eyre::eyre::Result;
use crossterm::event::KeyCode;
use ratatui::{
    buffer::Buffer,
    crossterm::event::KeyEvent,
    layout::{Constraint, Flex, Layout},
    prelude::Rect,
    style::Stylize,
    widgets::{Block, StatefulWidget, Widget},
};
use tokio::sync::{
    broadcast,
    mpsc::{self, Receiver, Sender},
};
use tokio_util::sync::CancellationToken;

use crate::{
    action::{self, Action},
    mode::Mode,
    tui,
    widgets::{
        info::{Info, InfoWidget},
        keyspace::{KeySpace, KeySpaceWidget},
    },
};

pub struct AppWidget;

pub struct App {
    state: SharedState,

    tick_rate: f64,
    frame_rate: f64,

    mode: Mode,
    previous_mode: Option<Mode>,
    error_message: Option<String>,

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
        if self.keyspace.is_popup() && (key.code != KeyCode::Enter && key.code != KeyCode::Esc) {
            self.keyspace.handle_key(key);
            return None;
        }

        self.handle_keybindings(key)
    }

    fn handle_keybindings(&mut self, key: KeyEvent) -> Option<Action> {
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
            Action::ConfirmKeyspacePopup => self.set_keyspace_filter(),
            Action::DeleteKeyspaceFilter => self.delete_keyspace_filter(),
            Action::Error(ref msg) => self.show_error_popup(msg),
            _ => {}
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

        let [main, footer] = Layout::vertical([Constraint::Percentage(100), Constraint::Length(4)])
            .flex(Flex::Center)
            .margin(1)
            .areas(area);

        StatefulWidget::render(InfoWidget, footer, buf, &mut state.summary);
        state.render_main_block(main, buf);
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

    fn close_popup(&mut self) {
        if !self.keyspace.is_popup() {
            return;
        }

        self.keyspace.exit_popup();
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
        if self.mode == Mode::KeySpace {
            self.keyspace.scroll_next();
            self.keyspace.clear_selected_value();
            action::try_send_action(&self.tx, Action::RequestSelectedValue);
        }
    }

    fn scroll_up(&mut self) {
        if self.mode == Mode::KeySpace {
            self.keyspace.scroll_previous();
            self.keyspace.clear_selected_value();
            action::try_send_action(&self.tx, Action::RequestSelectedValue);
        }
    }
}
