use std::{
    str::FromStr,
    sync::{Arc, Mutex},
};

use color_eyre::eyre::Result;
use crossterm::event::KeyEvent;
use ratatui::{
    layout::Layout,
    prelude::Rect,
    style::Stylize,
    widgets::{Block, Borders, Paragraph, StatefulWidget, Widget},
    Frame,
};
use redis::aio::ConnectionManager;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    action::Action,
    config::Config,
    mode::Mode,
    redis_client::runner::Runner,
    tui,
    widgets::{
        home::Home,
        summary::{Summary, SummaryWidget},
    },
};

pub struct AppWidget;

pub struct App {
    home: Home,
    config: Config,

    tick_rate: f64,
    frame_rate: f64,

    should_quit: bool,
    mode: Mode,
    last_tick_key_events: Vec<KeyEvent>,

    tx: mpsc::UnboundedSender<Action>,
    rx: mpsc::UnboundedReceiver<Action>,

    summary: Summary,
}

impl App {
    pub fn new(tick_rate: f64, frame_rate: f64) -> Result<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let config = Config::new()?;

        let home = Home::default();
        let mode = Mode::Home;

        let summary = Summary::new(Arc::new(Mutex::new(None)), tx.clone());

        Ok(Self {
            summary,
            home,
            tick_rate,
            frame_rate,
            should_quit: false,
            config,
            mode,
            last_tick_key_events: Vec::new(),
            tx,
            rx,
        })
    }

    pub async fn run(&mut self) -> Result<()> {
        let cancellation_token = CancellationToken::new();

        let mut tui = tui::Tui::new()?
            .tick_rate(self.tick_rate)
            .frame_rate(self.frame_rate)
            .cancelation_token(cancellation_token);

        // tui.mouse(true);
        tui.enter()?;

        let client = redis::Client::open("redis://localhost:6379").unwrap();
        let manager = ConnectionManager::new(client).await.unwrap();

        let mut watcher = Runner::new(manager.clone(), self.summary.info());

        watcher.start();

        loop {
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
        let maybe_action = if let Some(keymap) = self.config.keybindings.get(&self.mode) {
            if let Some(action) = keymap.get(&vec![key]) {
                log::info!("Got action: {action:?}");
                Some(action.clone())
            } else {
                // If the key was not handled as a single key action,
                // then consider it for multi-key combinations.
                self.last_tick_key_events.push(key);

                // Check for multi-key combinations
                if let Some(action) = keymap.get(&self.last_tick_key_events) {
                    log::info!("Got action: {action:?}");
                    Some(action.clone())
                } else {
                    None
                }
            }
        } else {
            None
        };

        Ok(maybe_action.map(Into::into))
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
            _ => {}
        }

        let maybe_action = None;

        Ok(maybe_action)
    }

    fn draw(&mut self, tui: &mut tui::Tui) -> Result<()> {
        tui.draw(|frame| {
            frame.render_stateful_widget(AppWidget, frame.size(), self);
        })?;
        Ok(())
    }
}

impl StatefulWidget for AppWidget {
    type State = App;

    fn render(self, area: Rect, buf: &mut ratatui::prelude::Buffer, state: &mut Self::State) {
        Block::default()
            .bg(ratatui::style::Color::from_str("#282936").unwrap())
            .render(area, buf);

        use ratatui::layout::Constraint;

        let [header, main, footer, _] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);

        StatefulWidget::render(SummaryWidget, header, buf, &mut state.summary);
        Block::default()
            .bg(ratatui::style::Color::from_str("#382936").unwrap())
            .title("MODE: Home")
            .borders(Borders::BOTTOM | Borders::LEFT | Borders::RIGHT)
            .render(footer, buf);
    }
}
