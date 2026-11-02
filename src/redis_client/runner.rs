use std::time::Duration;

use redis::aio::ConnectionManager;
use tokio::{
    sync::broadcast::{self, Sender as BroadcastSender},
    sync::mpsc::Sender as ActionSender,
    task::JoinHandle,
    time::MissedTickBehavior,
};
use tokio_util::sync::CancellationToken;

use crate::{action::Action, config, state::SharedState};

use super::{
    client,
    event::RedisEvent,
    storage::Storage,
    types::{KeyMeta, KeyspaceState},
};

type FetchError = Box<dyn std::error::Error + Sync + Send>;

const BROADCAST_CAPACITY: usize = 50;

pub struct Runner {
    cancellation_token: CancellationToken,
    manager: ConnectionManager,

    state: SharedState,
    info_task: JoinHandle<()>,
    state_task: JoinHandle<()>,

    action_tx: ActionSender<Action>,
    tx: BroadcastSender<RedisEvent>,
}

impl Runner {
    pub fn new(
        manager: ConnectionManager,
        state: SharedState,
        action_tx: ActionSender<Action>,
    ) -> Self {
        let info_task = tokio::spawn(async {});
        let state_task = tokio::spawn(async {});
        let cancellation_token = CancellationToken::new();

        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);

        Self {
            cancellation_token,
            manager,
            state,
            info_task,
            state_task,
            action_tx,
            tx,
        }
    }

    #[must_use]
    pub fn cancellation_token(mut self, token: CancellationToken) -> Self {
        self.cancellation_token = token;
        self
    }

    pub fn tx(&self) -> BroadcastSender<RedisEvent> {
        self.tx.clone()
    }

    pub fn start(&mut self) {
        self.launch_refresh_info_task();
        self.launch_refresh_state_task();
    }

    /// Cancels the background tasks and awaits their completion so in-flight
    /// `INFO` / `SCAN` / `MEMORY USAGE` requests don't keep running on a process
    /// that's exiting. Falls back to `abort()` if a task doesn't stop within the
    /// timeout.
    pub async fn shutdown(self) {
        self.cancellation_token.cancel();

        let info_abort = self.info_task.abort_handle();
        let state_abort = self.state_task.abort_handle();

        let timeout = Duration::from_secs(2);
        if tokio::time::timeout(timeout, async {
            let _ = tokio::join!(self.info_task, self.state_task);
        })
        .await
        .is_err()
        {
            log::warn!("Redis background tasks did not stop within {timeout:?}; aborting");
            info_abort.abort();
            state_abort.abort();
        }
    }

    fn launch_refresh_state_task(&mut self) {
        let manager = self.manager.clone();
        let cancellation_token = self.cancellation_token.clone();

        let action_tx = self.action_tx.clone();
        let mut rx = self.tx.subscribe();
        let state = self.state.clone();
        let storage = Storage::new(manager);

        self.state_task = tokio::spawn(async move {
            let mut event_handler = EventHandler::new(state, action_tx, storage);

            loop {
                tokio::select! {
                    Ok(event) = rx.recv() => {
                        event_handler.handle(event).await;
                    },
                    () = cancellation_token.cancelled() => {
                        break;
                    },
                }
            }
        });
    }

    fn launch_refresh_info_task(&mut self) {
        let tick: Duration = config::get().dashboard.interval();
        let info = self.state.info.clone();
        let metrics = self.state.metrics.clone();
        let mut manager = self.manager.clone();
        let cancellation_token = self.cancellation_token.clone();
        let action_tx = self.action_tx.clone();

        self.info_task = tokio::spawn(async move {
            let mut refresh_interval = tokio::time::interval(tick);
            refresh_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
            // Report only the first failure of an outage; INFO is polled every
            // tick, so reporting each one would re-open the popup every tick.
            let mut info_failing = false;
            // Counts successful samples; CLUSTER INFO is polled every 5th one.
            let mut tick_no: u64 = 0;

            loop {
                tokio::select! {
                    _ = refresh_interval.tick() => {
                        let info_res = client::redis_info_with_snapshot(&mut manager).await;

                        match info_res {
                            Ok((redis_info, snapshot)) => {
                                let cluster_enabled = snapshot.cluster_enabled;
                                *info.lock() = Some(redis_info);
                                info_failing = false;
                                tick_no += 1;
                                {
                                    let mut m = metrics.lock();
                                    m.record(snapshot);
                                    if !cluster_enabled {
                                        m.set_cluster(None);
                                    }
                                }

                                if cluster_enabled && tick_no % 5 == 1 {
                                    // No lock is held across this await.
                                    match client::cluster_info(&mut manager).await {
                                        Ok(c) => metrics.lock().set_cluster(Some(c)),
                                        Err(err) => log::warn!("Failed to fetch CLUSTER INFO: {err}"),
                                    }
                                }
                            },
                            Err(err) => {
                                log::error!("Failed to fetch INFO: {err}");
                                metrics.lock().mark_unavailable();
                                if !info_failing {
                                    info_failing = true;
                                    crate::action::try_send_action(
                                        &action_tx,
                                        Action::Error(format!("Failed to fetch INFO: {err}")),
                                    );
                                }
                            },
                        }
                    },
                    () = cancellation_token.cancelled() => {
                        break;
                    }
                }
            }
        });
    }
}

pub struct EventHandler {
    state: SharedState,
    tx: ActionSender<Action>,
    storage: Storage,
    /// `ValueState::generation` of the last value fetch started. Queued
    /// `FetchValue` events all read the latest state, so after the first one the
    /// rest would refetch the same page.
    last_value_generation: Option<u64>,
}

impl EventHandler {
    fn new(state: SharedState, tx: ActionSender<Action>, storage: Storage) -> Self {
        Self {
            state,
            tx,
            storage,
            last_value_generation: None,
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn handle(&mut self, event: RedisEvent) {
        match event {
            RedisEvent::SetString { key, value } => {
                match self.storage.set_string(&key, &value).await {
                    Ok(()) => {
                        // Re-fetch the value so the detail view reflects the edit,
                        // and refresh the keyspace so the size column updates.
                        self.action_hook(Action::RequestSelectedValue);
                        self.action_hook(Action::RefreshSpace);
                    }
                    Err(err) => {
                        log::error!("SetString failed for {key}: {err:?}");
                        self.action_hook(Action::Error(format!("Failed to set {key}: {err}")));
                    }
                }
            }
            RedisEvent::DeleteKey { key } => match self.storage.delete_key(&key).await {
                Ok(()) => self.action_hook(Action::RefreshSpace),
                Err(err) => {
                    log::error!("DeleteKey failed for {key}: {err:?}");
                    self.action_hook(Action::Error(format!("Failed to delete {key}: {err}")));
                }
            },
            RedisEvent::SetTtl { key, secs } => match self.storage.set_ttl(&key, secs).await {
                Ok(()) => self.action_hook(Action::RefreshSpace),
                Err(err) => {
                    log::error!("SetTtl failed for {key}: {err:?}");
                    self.action_hook(Action::Error(format!("Failed to set TTL on {key}: {err}")));
                }
            },
            RedisEvent::CreateKey { key, spec } => {
                match self.storage.create_key(&key, &spec).await {
                    // Cursor lands on the new key via `App::reselect_key`.
                    Ok(()) => self.action_hook(Action::RefreshSpace),
                    Err(err) => {
                        log::error!("CreateKey failed for {key}: {err:?}");
                        self.action_hook(Action::Error(format!("Failed to create {key}: {err}")));
                    }
                }
            }
            RedisEvent::AddItem { key, item } => match self.storage.add_item(&key, &item).await {
                Ok(()) => {
                    // Re-fetch the value so the detail table reflects the new
                    // element, and refresh the keyspace so the size column updates.
                    self.action_hook(Action::RequestSelectedValue);
                    self.action_hook(Action::RefreshSpace);
                }
                Err(err) => {
                    log::error!("AddItem failed for {key}: {err:?}");
                    self.action_hook(Action::Error(format!("Failed to add item to {key}: {err}")));
                }
            },
            RedisEvent::EditItem { key, edit } => {
                match self.storage.edit_item(&key, &edit).await {
                    Ok(()) => {
                        // Re-fetch the value so the detail table reflects the
                        // edit; refresh the keyspace so the size column updates.
                        self.action_hook(Action::RequestSelectedValue);
                        self.action_hook(Action::RefreshSpace);
                    }
                    Err(err) => {
                        log::error!("EditItem failed for {key}: {err:?}");
                        self.action_hook(Action::Error(format!(
                            "Failed to edit item in {key}: {err}"
                        )));
                    }
                }
            }
            RedisEvent::DeleteItem { key, delete } => {
                match self.storage.delete_item(&key, &delete).await {
                    Ok(()) => {
                        self.action_hook(Action::RequestSelectedValue);
                        self.action_hook(Action::RefreshSpace);
                    }
                    Err(err) => {
                        log::error!("DeleteItem failed for {key}: {err:?}");
                        self.action_hook(Action::Error(format!(
                            "Failed to delete item from {key}: {err}"
                        )));
                    }
                }
            }
            RedisEvent::FetchValue => self.fetch_value().await,
            RedisEvent::FetchKeys => {
                // Work on a snapshot so the lock isn't held across `.await`.
                let snapshot = self.state.keyspace_state.lock().clone();
                let generation = snapshot.generation;

                match self.fetch_page(snapshot).await {
                    Ok((sweep, keys)) => {
                        {
                            let mut state = self.state.keyspace_state.lock();
                            if state.generation != generation {
                                // The user paged / filtered / refreshed meanwhile;
                                // a newer FetchKeys is already queued.
                                drop(state);
                                self.action_hook(Action::KeysLoadAborted);
                                return;
                            }
                            state.apply_sweep(sweep);
                            *self.state.keys.lock() = keys;
                        }
                        self.action_hook(Action::LoadKeysIntoKeySpace);
                    }
                    Err(err) => {
                        log::error!("FetchKeys failed: {err:?}");
                        self.action_hook(Action::Error(format!("Failed to fetch keys: {err}")));
                        self.action_hook(Action::KeysLoadAborted);
                    }
                }
            }
        }
    }

    /// Fetch the selected value's current page. Works on a snapshot so the lock
    /// isn't held across `.await`, and drops the result if the user moved to
    /// another key / page meanwhile (a newer `FetchValue` is then queued).
    async fn fetch_value(&mut self) {
        let snapshot = self.state.value_state.lock().clone();
        let Some(key) = snapshot.key.clone() else {
            return;
        };
        let generation = snapshot.generation;
        if self.last_value_generation == Some(generation) {
            return;
        }
        self.last_value_generation = Some(generation);

        match self.storage.fetch_value_page(snapshot).await {
            Ok((swept, loaded)) => {
                {
                    let mut state = self.state.value_state.lock();
                    if state.generation != generation {
                        return;
                    }
                    state.apply(swept);
                    *self.state.selected_value.lock() = Some(loaded);
                }
                self.action_hook(Action::LoadSelectedValueIntoView);
            }
            Err(err) => {
                log::error!("FetchValue failed for {key}: {err:?}");
                if self.state.value_state.lock().generation == generation {
                    self.action_hook(Action::ValueLoadFailed);
                    self.action_hook(Action::Error(format!(
                        "Failed to load value of {key}: {err}"
                    )));
                }
            }
        }
    }

    /// Advance the SCAN sweep until it covers the current page (or ends), then
    /// fetch metadata for just that page's keys.
    async fn fetch_page(
        &self,
        mut sweep: KeyspaceState,
    ) -> Result<(KeyspaceState, Vec<KeyMeta>), FetchError> {
        while sweep.needs_scan() {
            let (cursor, batch) = self
                .storage
                .scan_batch(
                    sweep.sweep_cursor,
                    sweep.pattern.as_deref(),
                    sweep.scan_count,
                )
                .await?;
            sweep.merge_batch(cursor, batch);
        }
        sweep.clamp_page();

        let keys = self
            .storage
            .fetch_meta(&sweep.scanned[sweep.page_range()])
            .await?;
        Ok((sweep, keys))
    }

    fn action_hook(&self, action: Action) {
        crate::action::try_send_action(&self.tx, action);
    }
}
