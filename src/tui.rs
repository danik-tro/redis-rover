use std::{
    ops::{Deref, DerefMut},
    time::Duration,
};

use color_eyre::eyre::Result;
use crossterm::{
    cursor,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::{FutureExt, StreamExt};
use ratatui::{
    backend::CrosstermBackend as Backend,
    crossterm::event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event as CrosstermEvent, KeyEvent, KeyEventKind, MouseEvent,
    },
};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

pub type IO = std::io::Stdout;

pub fn io() -> IO {
    std::io::stdout()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Event {
    Init,
    Quit,
    Error,
    Closed,
    Tick,
    Render,
    FocusGained,
    FocusLost,
    Paste(String),
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize(u16, u16),
}

pub struct Tui {
    pub terminal: ratatui::Terminal<Backend<IO>>,
    pub event_task: JoinHandle<()>,
    pub cancellation_token: CancellationToken,
    pub event_rx: UnboundedReceiver<Event>,
    pub event_tx: UnboundedSender<Event>,
    pub frame_rate: f64,
    pub tick_rate: f64,
    pub mouse: bool,
    pub paste: bool,
}

impl Tui {
    /// Build a new TUI wrapping a crossterm-backed terminal.
    ///
    /// # Errors
    ///
    /// Returns an error if the ratatui terminal cannot be constructed
    /// (typically a stdout I/O failure).
    pub fn new() -> Result<Self> {
        let tick_rate = 4.0;
        let frame_rate = 10.0;
        let terminal = ratatui::Terminal::new(Backend::new(io()))?;
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let cancellation_token = CancellationToken::new();
        let event_task = tokio::spawn(async {});

        let mouse = false;
        let paste = false;
        Ok(Self {
            terminal,
            event_task,
            cancellation_token,
            event_rx,
            event_tx,
            frame_rate,
            tick_rate,
            mouse,
            paste,
        })
    }

    #[must_use]
    pub fn tick_rate(mut self, tick_rate: f64) -> Self {
        self.tick_rate = tick_rate;
        self
    }

    #[must_use]
    pub fn frame_rate(mut self, frame_rate: f64) -> Self {
        self.frame_rate = frame_rate;
        self
    }

    #[must_use]
    pub fn cancelation_token(mut self, token: CancellationToken) -> Self {
        self.cancellation_token = token;
        self
    }

    #[must_use]
    #[allow(dead_code)]
    pub fn mouse(mut self, mouse: bool) -> Self {
        self.mouse = mouse;
        self
    }

    #[must_use]
    #[allow(dead_code)]
    pub fn paste(mut self, paste: bool) -> Self {
        self.paste = paste;
        self
    }

    pub fn start(&mut self) {
        let tick_delay = std::time::Duration::from_secs_f64(1.0 / self.tick_rate);
        let render_delay = std::time::Duration::from_secs_f64(1.0 / self.frame_rate);

        let tui_cancelation_token = self.cancellation_token.clone();

        let event_tx = self.event_tx.clone();

        self.event_task = tokio::spawn(async move {
            let mut reader = crossterm::event::EventStream::new();
            let mut tick_interval = tokio::time::interval(tick_delay);
            let mut render_interval = tokio::time::interval(render_delay);

            event_tx.send(Event::Init).unwrap();

            loop {
                let tick_delay = tick_interval.tick();
                let render_delay = render_interval.tick();
                let crossterm_event = reader.next().fuse();
                tokio::select! {
                  () = tui_cancelation_token.cancelled() => {
                    break;
                  }
                  maybe_event = crossterm_event => {
                    match maybe_event {
                      Some(Ok(evt)) => {
                        match evt {
                          CrosstermEvent::Key(key) => {
                            if key.kind == KeyEventKind::Press {
                              event_tx.send(Event::Key(key)).unwrap();
                            }
                          },
                          CrosstermEvent::Mouse(mouse) => {
                            event_tx.send(Event::Mouse(mouse)).unwrap();
                          },
                          CrosstermEvent::Resize(x, y) => {
                            event_tx.send(Event::Resize(x, y)).unwrap();
                          },
                          CrosstermEvent::FocusLost => {
                            event_tx.send(Event::FocusLost).unwrap();
                          },
                          CrosstermEvent::FocusGained => {
                            event_tx.send(Event::FocusGained).unwrap();
                          },
                          CrosstermEvent::Paste(s) => {
                            event_tx.send(Event::Paste(s)).unwrap();
                          },
                        }
                      }
                      Some(Err(_)) => {
                        event_tx.send(Event::Error).unwrap();
                      }
                      None => {},
                    }
                  },
                  _ = tick_delay => {
                      event_tx.send(Event::Tick).unwrap();
                  },
                  _ = render_delay => {
                      event_tx.send(Event::Render).unwrap();
                  },
                }
            }
        });
    }

    /// Gracefully stops the event task: signals cancellation and awaits the
    /// task, falling back to `abort()` if it doesn't finish promptly. Must be
    /// called from an async context (the main shutdown path). The `Drop` /
    /// panic path uses [`Tui::force_stop`] instead, since it can't await.
    pub async fn stop(&mut self) -> Result<()> {
        self.cancel();
        match tokio::time::timeout(Duration::from_millis(50), &mut self.event_task).await {
            Ok(_) => {}
            Err(_) => {
                self.event_task.abort();
            }
        }
        Ok(())
    }

    /// Best-effort, non-blocking stop for sync teardown (`Drop`, panic handler):
    /// signals cancellation and aborts the event task without awaiting.
    fn force_stop(&self) {
        self.cancel();
        self.event_task.abort();
    }

    /// Enter the alternate screen, enable raw mode, and spawn the
    /// terminal event task.
    ///
    /// # Errors
    ///
    /// Returns an error if a crossterm terminal command fails.
    pub fn enter(&mut self) -> Result<()> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(io(), EnterAlternateScreen, cursor::Hide)?;
        if self.mouse {
            crossterm::execute!(io(), EnableMouseCapture)?;
        }
        if self.paste {
            crossterm::execute!(io(), EnableBracketedPaste)?;
        }
        self.start();
        Ok(())
    }

    /// Tear down the alternate screen and disable raw mode.
    ///
    /// # Errors
    ///
    /// Returns an error if a crossterm terminal command fails.
    pub fn exit(&mut self) -> Result<()> {
        self.force_stop();
        if crossterm::terminal::is_raw_mode_enabled()? {
            self.flush()?;
            if self.paste {
                crossterm::execute!(io(), DisableBracketedPaste)?;
            }
            if self.mouse {
                crossterm::execute!(io(), DisableMouseCapture)?;
            }
            crossterm::execute!(io(), LeaveAlternateScreen, cursor::Show)?;
            crossterm::terminal::disable_raw_mode()?;
        }
        Ok(())
    }

    pub fn cancel(&self) {
        self.cancellation_token.cancel();
    }

    pub async fn next(&mut self) -> Option<Event> {
        self.event_rx.recv().await
    }
}

impl Deref for Tui {
    type Target = ratatui::Terminal<Backend<IO>>;

    fn deref(&self) -> &Self::Target {
        &self.terminal
    }
}

impl DerefMut for Tui {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.terminal
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        if let Err(err) = self.exit() {
            log::error!("Tui::exit failed during drop: {err:?}");
        }
    }
}
