use std::time::Duration;

use redis::aio::ConnectionManager;
use tokio::{
    sync::broadcast::{self, Sender},
    sync::mpsc::UnboundedSender,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{action::Action, state::SharedState};

use super::{client, event::RedisEvent};

const BROADCAST_CAPACITY: usize = 50;

pub struct Runner {
    cancelation_token: CancellationToken,
    manager: ConnectionManager,

    state: SharedState,
    info_task: JoinHandle<()>,

    action_tx: UnboundedSender<Action>,
    tx: Sender<RedisEvent>,
}

impl Runner {
    pub fn new(
        manager: ConnectionManager,
        state: SharedState,
        action_tx: UnboundedSender<Action>,
    ) -> Self {
        let info_task = tokio::spawn(async {});
        let cancelation_token = CancellationToken::new();

        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);

        Self {
            manager,
            info_task,
            cancelation_token,
            action_tx,
            tx,
            state,
        }
    }

    #[must_use]
    pub fn cancelation_token(mut self, token: CancellationToken) -> Self {
        self.cancelation_token = token;
        self
    }

    pub fn tx(&self) -> Sender<RedisEvent> {
        self.tx.clone()
    }

    pub fn start(&mut self) {
        self.launch_refresh_info_task();
        self.launch_refresh_state_task()
    }

    fn launch_refresh_state_task(&mut self) {
        let manager = self.manager.clone();
        let cancelation_token = self.cancelation_token.clone();

        let action_tx = self.action_tx.clone();
        let mut rx = self.tx.subscribe();
        let state = self.state.clone();

        tokio::spawn(async move {
            let mut event_handler = EventHandler::new(state, action_tx, manager);

            loop {
                tokio::select! {
                    Ok(event) = rx.recv() => {
                        event_handler.handle(event).await;
                    },
                    _ = cancelation_token.cancelled() => {
                        break;
                    },
                }
            }
        });
    }

    fn launch_refresh_info_task(&mut self) {
        let tick: Duration = std::time::Duration::from_secs_f64(2.0);
        let info = self.state.info.clone();
        let mut manager = self.manager.clone();
        let cancelation_token = self.cancelation_token.clone();

        self.info_task = tokio::spawn(async move {
            let mut refresh_interval = tokio::time::interval(tick);

            loop {
                tokio::select! {
                    _ = refresh_interval.tick() => {
                        let info_res = client::redis_info(&mut manager).await;

                        match info_res {
                            Ok(redis_info) => *info.lock().unwrap() = Some(redis_info),
                            Err(_err) => {
                                // TODO: show the popup
                            },
                        }
                    },
                    _ = cancelation_token.cancelled() => {
                        break;
                    }
                }
            }
        });
    }
}

pub struct EventHandler {
    state: SharedState,
    tx: UnboundedSender<Action>,
    manager: ConnectionManager,
}

impl EventHandler {
    fn new(state: SharedState, tx: UnboundedSender<Action>, manager: ConnectionManager) -> Self {
        Self { state, tx, manager }
    }

    async fn handle(&mut self, event: RedisEvent) {
        match event {
            RedisEvent::FetchKeys { cursor, pattern } => {
                match client::keys(&mut self.manager, cursor, pattern).await {
                    Ok((_cursor, keys)) => {
                        let mut store = self.state.keys.lock().unwrap();
                        _ = std::mem::replace(&mut *store, keys);
                        self.action_hook(Action::LoadKeysIntoKeySpace);
                    }
                    Err(_) => {
                        todo!("TODO: implement error popup")
                    }
                }
            }
        }
    }

    fn action_hook(&self, action: Action) {
        if let Err(err) = self.tx.send(action) {
            log::debug!("failed to send action hook: {err:?}");
        }
    }
}
