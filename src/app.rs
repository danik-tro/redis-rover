use crate::{config, redis_client::event::RedisEvent, state::SharedState};
use color_eyre::eyre::Result;
use ratatui::{
    buffer::Buffer,
    crossterm::event::KeyEvent,
    layout::{Flex, Layout},
    prelude::Rect,
    style::Stylize,
    widgets::{Block, StatefulWidget, Widget},
};
use tokio::sync::{
    broadcast,
    mpsc::{self, UnboundedReceiver, UnboundedSender},
};
use tokio_util::sync::CancellationToken;

use crate::{
    action::Action,
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

    should_quit: bool,
    last_tick_key_events: Vec<KeyEvent>,

    tx: mpsc::UnboundedSender<Action>,
    rx: mpsc::UnboundedReceiver<Action>,

    redis_tx: broadcast::Sender<RedisEvent>,
    summary: Info,
    keyspace: KeySpace,
}

impl App {
    pub fn new(
        state: SharedState,
        tx: UnboundedSender<Action>,
        rx: UnboundedReceiver<Action>,
        redis_tx: broadcast::Sender<RedisEvent>,
        tick_rate: f64,
        frame_rate: f64,
    ) -> Result<Self> {
        let _ = config::get();

        let mode = Mode::KeySpace;

        let summary = Info::new(state.info.clone());
        let keyspace = KeySpace::new(Vec::new());

        Ok(Self {
            state,
            summary,
            keyspace,
            tick_rate,
            frame_rate,
            should_quit: false,
            mode,
            previous_mode: None,
            last_tick_key_events: Vec::new(),
            tx,
            rx,
            redis_tx,
        })
    }

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
                self.handle_event(e)?.map(|action| self.tx.send(action));
            }

            while let Ok(action) = self.rx.try_recv() {
                self.handle_action(action, &mut tui)?
                    .map(|action| self.tx.send(action));
            }
            if self.should_quit {
                tui.stop()?;
                break;
            }
        }
        tui.exit()?;
        Ok(())
    }

    fn resize(&mut self, tui: &mut tui::Tui, (w, h): (u16, u16)) -> Result<()> {
        tui.resize(Rect::new(0, 0, w, h))?;
        self.tx.send(Action::Render)?;

        Ok(())
    }

    fn handle_event(&mut self, e: tui::Event) -> Result<Option<Action>> {
        let maybe_action = match e {
            tui::Event::Quit => Some(Action::Quit),
            tui::Event::Tick => Some(Action::Tick),
            tui::Event::Render => Some(Action::Render),
            tui::Event::Resize(x, y) => Some(Action::Resize(x, y)),
            tui::Event::Key(key) => self.handle_key_event(key)?,
            _ => None,
        };

        Ok(maybe_action)
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        let action = self.handle_keybindings(key);
        Ok(action.map(Into::into))
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

    fn handle_action(&mut self, action: Action, tui: &mut tui::Tui) -> Result<Option<Action>> {
        if action != Action::Tick && action != Action::Render {
            log::debug!("{action:?}");
        }
        match action {
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
            Action::ScrollDown => self.scroll_down(),
            Action::ScrollUp => self.scroll_up(),
            Action::LoadNextPage => self.load_next_page(),
            Action::LoadPreviousPage => self.load_previous_page(),
            _ => {}
        }

        let maybe_action = None;

        Ok(maybe_action)
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
        Block::default()
            .bg(config::get().colors.base00)
            .render(area, buf);

        use ratatui::layout::Constraint;

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
        match self.mode {
            Mode::KeySpace => self.render_key_space(area, buf),
            _ => {}
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

    fn load_next_page(&mut self) {
        {
            let mut state = self.state.keyspace_state.lock().unwrap();
            state.update_cursor();
            self.keyspace
                .update_filters(state.pattern.clone(), state.cursor);
        }
        self.refresh_space();
    }

    fn load_previous_page(&mut self) {
        {
            let mut state = self.state.keyspace_state.lock().unwrap();
            state.set_previous_cursor();
            self.keyspace
                .update_filters(state.pattern.clone(), state.cursor);
        }
        self.refresh_space();
    }

    fn load_new_keys(&mut self) {
        self.keyspace
            .set_keys(self.state.keys.lock().unwrap().clone());
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
        match self.mode {
            Mode::KeySpace => self.keyspace.scroll_next(),
            _ => {}
        }
    }

    fn scroll_up(&mut self) {
        match self.mode {
            Mode::KeySpace => self.keyspace.scroll_previous(),
            _ => {}
        }
    }
}
