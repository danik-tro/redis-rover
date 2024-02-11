use futures::StreamExt;
use tokio::sync::mpsc;

use crate::{
    action::Action,
    app::util::{restore_terminal, setup_terminal},
    termination::Interrupted,
};

const RENDERING_TICK_RATE: std::time::Duration = std::time::Duration::from_millis(250);

pub struct Manager {
    action_tx: mpsc::UnboundedSender<Action>,
}

impl Manager {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<Action>) {
        let (action_tx, action_rx) = mpsc::unbounded_channel();

        (Self { action_tx }, action_rx)
    }

    pub async fn run(
        self,
        mut interrupt_rx: tokio::sync::broadcast::Receiver<Interrupted>,
    ) -> color_eyre::Result<Interrupted> {
        // TODO: create an app object
        let mut terminal = setup_terminal()?;
        let mut crossterm_events = crossterm::event::EventStream::new();
        let mut ticker = tokio::time::interval(RENDERING_TICK_RATE);

        let interrupted_by = loop {
            tokio::select! {
                _ = ticker.tick() => {},
                cross_event = crossterm_events.next() => match cross_event {
                    Some(Ok(event)) => self.handle_crossterm_event(event),
                    None => break Interrupted::UserInt,
                    _ => {},
                },
                Ok(interrupted) = interrupt_rx.recv() => {
                    break interrupted;
                },
            }
        };

        restore_terminal(&mut terminal)?;
        Ok(interrupted_by)
    }

    fn handle_crossterm_event(&self, event: crossterm::event::Event) {
        todo!()
    }
}
