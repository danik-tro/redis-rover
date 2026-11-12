use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use redis::aio::ConnectionManager;
use tokio::{
    sync::broadcast::{self, Sender as BroadcastSender},
    sync::mpsc::{self, Sender as ActionSender, UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
    time::MissedTickBehavior,
};
use tokio_util::sync::CancellationToken;

use crate::{action::Action, config, state::SharedState};

use super::{
    client,
    event::RedisEvent,
    pubsub::{self, PubSubCmd},
    storage::Storage,
    types::{RedisType, ValueState},
};

type FetchError = Box<dyn std::error::Error + Sync + Send>;

const BROADCAST_CAPACITY: usize = 50;

/// Most `SCAN` steps one filter match count may take (~200k keys at COUNT 1000).
const COUNT_MAX_STEPS: usize = 200;
/// Time budget for one filter match count.
const COUNT_BUDGET: Duration = Duration::from_secs(2);

/// How long a DB switch may take to open its new connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Test hook: milliseconds each value fetch waits before it starts (0 = off).
/// The integration suite uses it to make one value fetch slow without
/// stalling the whole server (`DEBUG SLEEP` would). One atomic load in
/// production.
#[doc(hidden)]
pub static VALUE_FETCH_DELAY_MS: AtomicU64 = AtomicU64::new(0);

pub struct Runner {
    cancellation_token: CancellationToken,
    /// The client the app started with. A DB switch builds a new connection
    /// from it, so TLS settings and credentials carry over.
    client: redis::Client,
    manager: ConnectionManager,
    read_only: bool,

    state: SharedState,
    info_task: JoinHandle<()>,
    state_task: JoinHandle<()>,
    /// Owns the dedicated Pub/Sub connection (opened on the first subscribe).
    pubsub_task: JoinHandle<()>,
    pubsub_tx: UnboundedSender<PubSubCmd>,
    /// Taken by `start` when the Pub/Sub task is spawned.
    pubsub_rx: Option<UnboundedReceiver<PubSubCmd>>,

    action_tx: ActionSender<Action>,
    tx: BroadcastSender<RedisEvent>,
}

impl Runner {
    #[must_use]
    pub fn new(
        client: redis::Client,
        manager: ConnectionManager,
        state: SharedState,
        action_tx: ActionSender<Action>,
    ) -> Self {
        let info_task = tokio::spawn(async {});
        let state_task = tokio::spawn(async {});
        let pubsub_task = tokio::spawn(async {});
        let (pubsub_tx, pubsub_rx) = mpsc::unbounded_channel();
        let cancellation_token = CancellationToken::new();

        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);

        Self {
            cancellation_token,
            client,
            manager,
            read_only: false,
            state,
            info_task,
            state_task,
            pubsub_task,
            pubsub_tx,
            pubsub_rx: Some(pubsub_rx),
            action_tx,
            tx,
        }
    }

    #[must_use]
    pub fn cancellation_token(mut self, token: CancellationToken) -> Self {
        self.cancellation_token = token;
        self
    }

    /// Reject every write event (`--read-only`).
    #[must_use]
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    #[must_use]
    pub fn tx(&self) -> BroadcastSender<RedisEvent> {
        self.tx.clone()
    }

    /// Control channel of the Pub/Sub task.
    #[must_use]
    pub fn pubsub_tx(&self) -> UnboundedSender<PubSubCmd> {
        self.pubsub_tx.clone()
    }

    pub fn start(&mut self) {
        self.launch_refresh_info_task();
        self.launch_refresh_state_task();
        self.launch_pubsub_task();
    }

    /// Spawn the Pub/Sub task. It idles until the first subscribe, so no
    /// connection is opened for users who never use the tab.
    fn launch_pubsub_task(&mut self) {
        let Some(cmd_rx) = self.pubsub_rx.take() else {
            return;
        };
        self.pubsub_task = tokio::spawn(pubsub::run(
            self.client.clone(),
            self.state.pubsub.clone(),
            cmd_rx,
            self.action_tx.clone(),
            self.cancellation_token.clone(),
        ));
    }

    /// Cancels the background tasks and awaits their completion so in-flight
    /// `INFO` / `SCAN` / `MEMORY USAGE` requests don't keep running on a process
    /// that's exiting. Falls back to `abort()` if a task doesn't stop within the
    /// timeout.
    pub async fn shutdown(self) {
        self.cancellation_token.cancel();

        let info_abort = self.info_task.abort_handle();
        let state_abort = self.state_task.abort_handle();
        let pubsub_abort = self.pubsub_task.abort_handle();

        let timeout = Duration::from_secs(2);
        if tokio::time::timeout(timeout, async {
            let _ = tokio::join!(self.info_task, self.state_task, self.pubsub_task);
        })
        .await
        .is_err()
        {
            log::warn!("Redis background tasks did not stop within {timeout:?}; aborting");
            info_abort.abort();
            state_abort.abort();
            pubsub_abort.abort();
        }
    }

    fn launch_refresh_state_task(&mut self) {
        let manager = self.manager.clone();
        let cancellation_token = self.cancellation_token.clone();

        let action_tx = self.action_tx.clone();
        let mut rx = self.tx.subscribe();
        let state = self.state.clone();
        let storage = Storage::new(manager);
        let client = self.client.clone();
        let read_only = self.read_only;

        self.state_task = tokio::spawn(async move {
            // Values load over their own connection, so a large value reply
            // never holds back a keys page queued behind it on the socket.
            let value_storage = match open_connection(client.clone()).await {
                Ok(manager) => Storage::new(manager),
                Err(err) => {
                    log::warn!("Opening the value connection failed, sharing the main one: {err}");
                    storage.clone()
                }
            };
            let mut event_handler =
                EventHandler::new(state, action_tx, storage, value_storage, client, read_only);
            event_handler.load_db_count().await;

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
        let info_seq = self.state.info_seq.clone();
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
                                info_failing = false;
                                tick_no += 1;
                                {
                                    let mut m = metrics.lock();
                                    m.record(snapshot);
                                    if !cluster_enabled {
                                        m.set_cluster(None);
                                    }
                                }
                                *info.lock() = Some(redis_info);
                                // Last, so the footer sees both writes.
                                info_seq.fetch_add(1, Ordering::Release);

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
                                info_seq.fetch_add(1, Ordering::Release);
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
    /// Connection the value fetches use (see [`EventHandler::fetch_value`]).
    value_storage: Storage,
    /// Base client used to open a connection to another DB.
    client: redis::Client,
    read_only: bool,
    /// `ValueState::generation` of the last value fetch started.
    last_value_generation: Option<u64>,
    /// The running filter match count, if any. A new request aborts it.
    count_task: Option<JoinHandle<()>>,
    /// The running full-value fetch for copying, if any. A new one aborts it.
    yank_task: Option<JoinHandle<()>>,
    /// The running value page fetch, if any. A new one aborts it.
    value_task: Option<JoinHandle<()>>,
}

impl Drop for EventHandler {
    /// The spawned fetches outlive nothing: they stop with the state task.
    fn drop(&mut self) {
        for task in [&self.count_task, &self.yank_task, &self.value_task]
            .into_iter()
            .flatten()
        {
            task.abort();
        }
    }
}

impl EventHandler {
    fn new(
        state: SharedState,
        tx: ActionSender<Action>,
        storage: Storage,
        value_storage: Storage,
        client: redis::Client,
        read_only: bool,
    ) -> Self {
        Self {
            state,
            tx,
            storage,
            value_storage,
            client,
            read_only,
            last_value_generation: None,
            count_task: None,
            yank_task: None,
            value_task: None,
        }
    }

    /// Read `CONFIG GET databases` once for the DB picker. Managed services
    /// often block `CONFIG`; the default (16) is kept then.
    async fn load_db_count(&self) {
        match self.storage.database_count().await {
            Ok(Some(count)) => *self.state.db_count.lock() = count,
            Ok(None) => {}
            Err(err) => log::debug!("CONFIG GET databases failed, assuming 16: {err}"),
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn handle(&mut self, event: RedisEvent) {
        // The UI also refuses writes up front; this is the guarantee.
        if self.read_only && event.is_write() {
            log::warn!("Read-only mode: blocked {event:?}");
            self.action_hook(Action::Error(
                "Read-only mode: writes are disabled".to_string(),
            ));
            return;
        }
        match event {
            RedisEvent::SelectDb(db) => self.select_db(db).await,
            RedisEvent::CountMatches { pattern } => self.count_matches(pattern),
            RedisEvent::FetchFullValue { key, r_type } => self.fetch_full_value(key, r_type),
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
            RedisEvent::FetchValue => self.fetch_value(),
            RedisEvent::FetchPubSubChannels => match self.storage.pubsub_channels().await {
                Ok(channels) => {
                    self.state.pubsub.lock().set_server_channels(channels);
                    self.action_hook(Action::PubSubChannelsLoaded);
                }
                Err(err) => {
                    log::error!("FetchPubSubChannels failed: {err:?}");
                    self.action_hook(Action::Error(format!(
                        "Failed to list Pub/Sub channels: {err}"
                    )));
                }
            },
            RedisEvent::Publish { channel, payload } => {
                match self.storage.publish(&channel, &payload).await {
                    Ok(receivers) => self.action_hook(Action::Published { channel, receivers }),
                    Err(err) => {
                        log::error!("Publish to {channel} failed: {err:?}");
                        self.action_hook(Action::Error(format!(
                            "Failed to publish to {channel}: {err}"
                        )));
                    }
                }
            }
            RedisEvent::FetchKeys => self.fetch_keys().await,
        }
    }

    /// Switch to DB `db` by opening a new connection for it.
    ///
    /// `SELECT` on the shared connection isn't used: the connection manager
    /// reconnects with the DB from its connection info, so after a dropped
    /// connection it would silently go back to the old DB.
    async fn select_db(&mut self, db: i64) {
        match tokio::try_join!(self.connect_db(db), self.connect_db(db)) {
            Ok((manager, value_manager)) => {
                self.abort_count();
                for task in [self.yank_task.take(), self.value_task.take()]
                    .into_iter()
                    .flatten()
                {
                    task.abort();
                }
                self.storage = Storage::new(manager);
                self.value_storage = Storage::new(value_manager);
                *self.state.db.lock() = db;
                self.state.keyspace_state.lock().reset_for_db();
                self.state.value_state.lock().clear();
                *self.state.keys.lock() = None;
                *self.state.selected_value.lock() = None;
                self.last_value_generation = None;
                self.action_hook(Action::DbSwitched);
            }
            Err(err) => {
                log::error!("SelectDb {db} failed: {err:?}");
                self.action_hook(Action::Error(format!("Failed to switch to db {db}: {err}")));
                // Still on the old DB: load its current page again. The App
                // moved the last page out of `state.keys`, so there is
                // nothing to put back.
                self.fetch_keys().await;
            }
        }
    }

    /// Start counting the keys that match `pattern` in a separate task, so the
    /// event loop stays free. Replaces any count still running.
    fn count_matches(&mut self, pattern: String) {
        self.abort_count();
        let storage = self.storage.clone();
        let tx = self.tx.clone();
        self.count_task = Some(tokio::spawn(async move {
            match storage
                .count_matches(&pattern, COUNT_MAX_STEPS, COUNT_BUDGET)
                .await
            {
                Ok((count, complete)) => crate::action::try_send_action(
                    &tx,
                    Action::FilterMatchCount {
                        pattern,
                        count,
                        complete,
                    },
                ),
                // A preview only: log it rather than open an error popup.
                Err(err) => log::warn!("Counting matches for {pattern:?} failed: {err}"),
            }
        }));
    }

    /// Fetch `key`'s whole value in a separate task and leave its clipboard
    /// text in `SharedState::yank`, then send `YankReady` (or `YankFailed`).
    fn fetch_full_value(&mut self, key: String, r_type: RedisType) {
        if let Some(task) = self.yank_task.take() {
            task.abort();
        }
        let storage = self.storage.clone();
        let tx = self.tx.clone();
        let slot = self.state.yank.clone();
        self.yank_task = Some(tokio::spawn(async move {
            let action = match storage.fetch_full_value(&key, &r_type).await {
                Ok(value) => match value.to_clipboard_text() {
                    Some(text) => {
                        *slot.lock() = Some((key, text));
                        Action::YankReady
                    }
                    None => Action::YankFailed(format!(
                        "Copying {} values isn't supported yet",
                        r_type.name()
                    )),
                },
                Err(err) => {
                    log::error!("FetchFullValue failed for {key}: {err:?}");
                    Action::YankFailed(format!("Failed to copy {key}: {err}"))
                }
            };
            crate::action::try_send_action(&tx, action);
        }));
    }

    fn abort_count(&mut self) {
        if let Some(task) = self.count_task.take() {
            task.abort();
        }
    }

    async fn connect_db(&self, db: i64) -> Result<ConnectionManager, FetchError> {
        let info = self.client.get_connection_info().clone();
        let settings = info.redis_settings().clone().set_db(db);
        let client = redis::Client::open(info.set_redis_settings(settings))?;
        open_connection(client).await
    }

    /// Fetch the selected value's current page in a separate task on the value
    /// connection, so a slow value never delays the events queued behind it.
    /// A newer request aborts the running one. Works on a snapshot so the lock
    /// isn't held across `.await`; the task drops its result if the user moved
    /// to another key / page meanwhile.
    fn fetch_value(&mut self) {
        let snapshot = self.state.value_state.lock().clone();
        if snapshot.key.is_none() {
            return;
        }
        // Queued `FetchValue`s all read the latest state: after the first,
        // the rest would restart the same fetch.
        if self.last_value_generation == Some(snapshot.generation) {
            return;
        }
        self.last_value_generation = Some(snapshot.generation);
        if let Some(task) = self.value_task.take() {
            task.abort();
        }
        let storage = self.value_storage.clone();
        let state = self.state.clone();
        let tx = self.tx.clone();
        self.value_task = Some(tokio::spawn(async move {
            let delay = VALUE_FETCH_DELAY_MS.load(Ordering::Relaxed);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            load_value(&storage, &state, &tx, snapshot).await;
        }));
    }

    /// Load the current keyspace page into `state.keys` and send
    /// `LoadKeysIntoKeySpace`, or `KeysLoadAborted` when the user left the page
    /// meanwhile (a newer `FetchKeys` is then queued) or the fetch failed.
    async fn fetch_keys(&mut self) {
        match self.fetch_page().await {
            Ok(true) => self.action_hook(Action::LoadKeysIntoKeySpace),
            Ok(false) => self.action_hook(Action::KeysLoadAborted),
            Err(err) => {
                log::error!("FetchKeys failed: {err:?}");
                self.action_hook(Action::Error(format!("Failed to fetch keys: {err}")));
                self.action_hook(Action::KeysLoadAborted);
            }
        }
    }

    /// Advance the SCAN sweep until it covers the current page (or ends), then
    /// fetch metadata for just that page's keys. Returns `false` if the result
    /// is stale.
    ///
    /// The sweep is never copied and the lock is never held across `.await`:
    /// the runner takes a [`SweepRequest`](super::types::SweepRequest) under
    /// the lock, scans without it, and merges the raw keys back under it. If
    /// SCAN repeated keys and the page is still short, it asks again.
    async fn fetch_page(&self) -> Result<bool, FetchError> {
        loop {
            let Some(request) = self.state.keyspace_state.lock().sweep_request() else {
                break;
            };
            let result = self.storage.scan_until(&request).await?;
            let mut state = self.state.keyspace_state.lock();
            // Merge even after a page turn (the sweep is still valid), but
            // leave the newer page to the newer FetchKeys.
            if !state.apply_result(&request, result) || state.generation != request.generation {
                return Ok(false);
            }
        }

        let (generation, page) = {
            let mut state = self.state.keyspace_state.lock();
            state.clamp_page();
            (state.generation, state.page_keys())
        };
        let keys = self.storage.fetch_meta(&page).await?;

        let state = self.state.keyspace_state.lock();
        if state.generation != generation {
            return Ok(false);
        }
        *self.state.keys.lock() = Some(keys);
        Ok(true)
    }

    fn action_hook(&self, action: Action) {
        crate::action::try_send_action(&self.tx, action);
    }
}

/// Open a connection manager for `client`, giving up after [`CONNECT_TIMEOUT`].
async fn open_connection(client: redis::Client) -> Result<ConnectionManager, FetchError> {
    let manager = tokio::time::timeout(CONNECT_TIMEOUT, ConnectionManager::new(client))
        .await
        .map_err(|_| format!("timed out after {CONNECT_TIMEOUT:?}"))??;
    Ok(manager)
}

/// Fetch the value page `snapshot` describes. Unless the user moved on
/// meanwhile (`ValueState::generation` changed, checked under the lock), store
/// it in `selected_value` and send `LoadSelectedValueIntoView`; on failure send
/// `ValueLoadFailed` and an `Error`.
async fn load_value(
    storage: &Storage,
    state: &SharedState,
    tx: &ActionSender<Action>,
    snapshot: ValueState,
) {
    let Some(key) = snapshot.key.clone() else {
        return;
    };
    let generation = snapshot.generation;
    match storage.fetch_value_page(snapshot).await {
        Ok((swept, loaded)) => {
            {
                let mut value_state = state.value_state.lock();
                if value_state.generation != generation {
                    return;
                }
                value_state.apply(swept);
                *state.selected_value.lock() = Some(loaded);
            }
            crate::action::try_send_action(tx, Action::LoadSelectedValueIntoView);
        }
        Err(err) => {
            log::error!("FetchValue failed for {key}: {err:?}");
            if state.value_state.lock().generation == generation {
                crate::action::try_send_action(tx, Action::ValueLoadFailed);
                crate::action::try_send_action(
                    tx,
                    Action::Error(format!("Failed to load value of {key}: {err}")),
                );
            }
        }
    }
}
