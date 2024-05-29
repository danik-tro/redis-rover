use color_eyre::eyre::Result;
use crossterm::event::KeyEvent;
use ratatui::{
    prelude::Rect,
    style::Stylize,
    widgets::{Block, StatefulWidget, Widget},
    Frame,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::{action::Action, config::Config, mode::Mode, tui, widgets::home::Home};

pub struct AppWidget;

pub struct App {
    pub home: Home,
    pub config: Config,
    pub tick_rate: f64,
    pub frame_rate: f64,
    pub should_quit: bool,
    pub should_suspend: bool,
    pub mode: Mode,
    pub last_tick_key_events: Vec<KeyEvent>,
    tx: mpsc::UnboundedSender<Action>,
    rx: mpsc::UnboundedReceiver<Action>,
}

impl App {
    pub fn new(tick_rate: f64, frame_rate: f64) -> Result<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let config = Config::new()?;

        let home = Home::default();
        let mode = Mode::Home;

        Ok(Self {
            home,
            tick_rate,
            frame_rate,
            should_quit: false,
            should_suspend: false,
            config,
            mode,
            last_tick_key_events: Vec::new(),
            tx,
            rx,
        })
    }

    pub async fn run(&mut self) -> Result<()> {
        let mut tui = tui::Tui::new()?
            .tick_rate(self.tick_rate)
            .frame_rate(self.frame_rate);
        // tui.mouse(true);
        tui.enter()?;

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

        Ok(maybe_action)
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
            .bg(ratatui::style::Color::Rgb(18, 74, 72))
            .render(area, buf);

        match state.mode {
            Mode::Home => state.home.render(area, buf),
        }
    }
}
