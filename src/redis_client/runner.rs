use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
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
    live::{self, LiveCmd},
    pubsub::{self, PubSubCmd},
    storage::{Storage, TypeScanRejected},
    types::{RedisType, SweepApply, ValueState},
};

type FetchError = Box<dyn std::error::Error + Sync + Send>;

const BROADCAST_CAPACITY: usize = 50;

/// Most `SCAN` steps one filter match count may take (~200k keys at COUNT 1000).
const COUNT_MAX_STEPS: usize = 200;
/// Time budget for one filter match count.
const COUNT_BUDGET: Duration = Duration::from_secs(2);

/// Keys per metadata backfill pipeline (3 commands each).
const BACKFILL_BATCH: usize = 1000;
/// Least time between two `KeyMetaBackfilled` actions (redraws).
const BACKFILL_NOTIFY: Duration = Duration::from_millis(250);

/// Least time between two `LoadAllProgress` actions.
const LOAD_NOTIFY: Duration = Duration::from_millis(250);

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
    /// Refuse every write. Shared with `App`, so toggling it in the session
    /// moves the guarantee too.
    read_only: Arc<AtomicBool>,

    state: SharedState,
    info_task: JoinHandle<()>,
    state_task: JoinHandle<()>,
    /// Owns the dedicated Pub/Sub connection (opened on the first subscribe).
    pubsub_task: JoinHandle<()>,
    pubsub_tx: UnboundedSender<PubSubCmd>,
    /// Taken by `start` when the Pub/Sub task is spawned.
    pubsub_rx: Option<UnboundedReceiver<PubSubCmd>>,
    /// Follows the open key (`live.mode`); not spawned when it is `off`.
    live_task: JoinHandle<()>,
    live_tx: UnboundedSender<LiveCmd>,
    /// Taken by `start` (and dropped with `live.mode = "off"`, so sends fail
    /// instead of queueing).
    live_rx: Option<UnboundedReceiver<LiveCmd>>,

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
        let live_task = tokio::spawn(async {});
        let (live_tx, live_rx) = mpsc::unbounded_channel();
        let cancellation_token = CancellationToken::new();

        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);

        Self {
            cancellation_token,
            client,
            manager,
            read_only: Arc::new(AtomicBool::new(false)),
            state,
            info_task,
            state_task,
            pubsub_task,
            pubsub_tx,
            pubsub_rx: Some(pubsub_rx),
            live_task,
            live_tx,
            live_rx: Some(live_rx),
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
    pub fn read_only(self, read_only: bool) -> Self {
        self.read_only.store(read_only, Ordering::Release);
        self
    }

    /// The read-only flag the runner checks before every write. `App`
    /// holds the same `Arc`.
    #[must_use]
    pub fn read_only_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.read_only)
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

    /// Control channel of the live task.
    #[must_use]
    pub fn live_tx(&self) -> UnboundedSender<LiveCmd> {
        self.live_tx.clone()
    }

    pub fn start(&mut self) {
        self.launch_refresh_info_task();
        self.launch_refresh_state_task();
        self.launch_pubsub_task();
        self.launch_live_task();
    }

    /// Spawn the live task unless `live.mode` is `off`. It opens its
    /// connection on the first `Track`.
    fn launch_live_task(&mut self) {
        let Some(cmd_rx) = self.live_rx.take() else {
            return;
        };
        let config = config::get().live;
        if config.mode == crate::config::LiveMode::Off {
            return;
        }
        self.live_task = tokio::spawn(live::run(
            self.client.clone(),
            config,
            cmd_rx,
            self.action_tx.clone(),
            self.cancellation_token.clone(),
        ));
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
        let live_abort = self.live_task.abort_handle();

        let timeout = Duration::from_secs(2);
        if tokio::time::timeout(timeout, async {
            let _ = tokio::join!(
                self.info_task,
                self.state_task,
                self.pubsub_task,
                self.live_task
            );
        })
        .await
        .is_err()
        {
            log::warn!("Redis background tasks did not stop within {timeout:?}; aborting");
            info_abort.abort();
            state_abort.abort();
            pubsub_abort.abort();
            live_abort.abort();
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
        let read_only = Arc::clone(&self.read_only);

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
    read_only: Arc<AtomicBool>,
    /// `ValueState::generation` of the last value fetch started.
    last_value_generation: Option<u64>,
    /// The running filter match count, if any. A new request aborts it.
    count_task: Option<JoinHandle<()>>,
    /// The running full-value fetch for copying, if any. A new one aborts it.
    yank_task: Option<JoinHandle<()>>,
    /// The running value page fetch, if any. A new one aborts it.
    value_task: Option<JoinHandle<()>>,
    /// The running metadata backfill, if any.
    backfill_task: Option<JoinHandle<()>>,
    /// The backfill's own connection, opened on its first run (and again
    /// after a DB switch), so its pipelines never queue in front of a page.
    backfill_storage: Option<Storage>,
    /// The running load-all (or `g o` search), if any.
    load_task: Option<JoinHandle<()>>,
    /// The load-all's own connection, opened like `backfill_storage`.
    load_storage: Option<Storage>,
}

impl Drop for EventHandler {
    /// The spawned fetches outlive nothing: they stop with the state task.
    fn drop(&mut self) {
        for task in [
            &self.count_task,
            &self.yank_task,
            &self.value_task,
            &self.backfill_task,
            &self.load_task,
        ]
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
        read_only: Arc<AtomicBool>,
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
            backfill_task: None,
            backfill_storage: None,
            load_task: None,
            load_storage: None,
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
        if event.is_write() && self.read_only.load(Ordering::Acquire) {
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
            RedisEvent::BackfillMeta => self.backfill_meta().await,
            RedisEvent::LoadAll { until } => self.load_all(until).await,
            RedisEvent::StopLoadAll => self.stop_load_all(),
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
                for task in [
                    self.yank_task.take(),
                    self.value_task.take(),
                    self.backfill_task.take(),
                    self.load_task.take(),
                ]
                .into_iter()
                .flatten()
                {
                    task.abort();
                }
                self.storage = Storage::new(manager);
                self.value_storage = Storage::new(value_manager);
                self.backfill_storage = None;
                self.load_storage = None;
                *self.state.db.lock() = db;
                self.state.keyspace_state.lock().reset_for_db();
                self.state.value_state.lock().clear();
                *self.state.keys.lock() = None;
                *self.state.page_rows.lock() = None;
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

    /// Start the metadata backfill unless it is already running. It fetches
    /// metadata for the keys the sort still lacks, 1,000 per pipeline, the
    /// keys nearest the current page first, until all are known, the sort no
    /// longer needs it, or the App pauses it (another screen). Each batch is
    /// merged into the order; `KeyMetaBackfilled` tells the App to redraw.
    async fn backfill_meta(&mut self) {
        if self
            .backfill_task
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            return;
        }
        if self
            .state
            .keyspace_state
            .lock()
            .backfill_request(1)
            .is_none()
        {
            return;
        }
        let storage = if let Some(storage) = self.backfill_storage.clone() {
            storage
        } else {
            let Some(storage) = self.open_side_storage("backfill").await else {
                return;
            };
            self.backfill_storage = Some(storage.clone());
            storage
        };
        let state = self.state.clone();
        let tx = self.tx.clone();
        self.backfill_task = Some(tokio::spawn(async move {
            let mut last_notice: Option<std::time::Instant> = None;
            // A batch was stored since the last notice.
            let mut unseen = false;
            loop {
                let Some(request) = state.keyspace_state.lock().backfill_request(BACKFILL_BATCH)
                else {
                    break;
                };
                let metas = match storage.fetch_meta(&request.names).await {
                    Ok(metas) => metas,
                    Err(err) => {
                        log::warn!("Metadata backfill stopped: {err}");
                        break;
                    }
                };
                if !state.keyspace_state.lock().record_meta(&request, &metas) {
                    continue;
                }
                unseen = true;
                if last_notice.is_none_or(|at| at.elapsed() >= BACKFILL_NOTIFY) {
                    last_notice = Some(std::time::Instant::now());
                    unseen = false;
                    crate::action::try_send_action(&tx, Action::KeyMetaBackfilled);
                }
            }
            // The last batch always shows.
            if unseen {
                crate::action::try_send_action(&tx, Action::KeyMetaBackfilled);
            }
        }));
    }

    /// A connection of its own on the current DB, for a background task
    /// whose commands should never queue in front of a page.
    async fn open_side_storage(&self, what: &str) -> Option<Storage> {
        let db = *self.state.db.lock();
        match self.connect_db(db).await {
            Ok(manager) => Some(Storage::new(manager)),
            Err(err) => {
                log::warn!("Opening the {what} connection failed: {err}");
                None
            }
        }
    }

    /// Sweep the rest of the keyspace in its own task, on its own
    /// connection: SCAN steps of `COUNT 1000` merged one by one, so
    /// page turns (which may race it, see `SweepApply::Raced`) stay quick.
    /// With `until`, stop once that key is loaded. Sends
    /// `LoadAllProgress` at most every [`LOAD_NOTIFY`] and `LoadAllDone`
    /// when it stops by itself; a sweep restart (rescan, filter, DB) stops
    /// it too. Replaces a load already running.
    async fn load_all(&mut self, until: Option<String>) {
        self.stop_load_all();
        let storage = if let Some(storage) = self.load_storage.clone() {
            storage
        } else if let Some(storage) = self.open_side_storage("load-all").await {
            self.load_storage = Some(storage.clone());
            storage
        } else {
            self.action_hook(Action::LoadAllDone { complete: false });
            return;
        };
        let state = self.state.clone();
        let tx = self.tx.clone();
        self.load_task = Some(tokio::spawn(async move {
            let complete = sweep_to_end(&storage, &state, &tx, until.as_deref()).await;
            crate::action::try_send_action(&tx, Action::LoadAllDone { complete });
        }));
    }

    fn stop_load_all(&mut self) {
        if let Some(task) = self.load_task.take() {
            task.abort();
        }
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
            let result = match self.storage.scan_until(&request).await {
                Ok(result) => result,
                // Before Redis 6: filter types on the client and ask again.
                Err(err) if err.is::<TypeScanRejected>() => {
                    self.state.keyspace_state.lock().disable_server_type_scan();
                    continue;
                }
                Err(err) => return Err(err),
            };
            let mut state = self.state.keyspace_state.lock();
            // Merge even after a page turn (the sweep is still valid), but
            // leave the newer page to the newer FetchKeys.
            // `Raced`: another SCAN moved the sweep first; ask again from there.
            match state.apply_result(&request, result) {
                SweepApply::Stale => return Ok(false),
                SweepApply::Applied if state.generation != request.generation => {
                    return Ok(false);
                }
                SweepApply::Applied | SweepApply::Raced => {}
            }
        }
        self.build_view().await?;

        let (generation, page, rows) = {
            let mut state = self.state.keyspace_state.lock();
            state.apply_reveal();
            state.clamp_page();
            (state.generation, state.page_keys(), state.page_rows())
        };
        let keys = self.storage.fetch_meta(&page).await?;

        let state = self.state.keyspace_state.lock();
        if state.generation != generation {
            return Ok(false);
        }
        *self.state.page_rows.lock() = rows;
        *self.state.keys.lock() = Some(keys);
        Ok(true)
    }

    /// Build a pending sort order. A large one is sorted in
    /// `spawn_blocking` on a copy of the names, so the keyspace lock is held
    /// only to copy them and to install the result.
    async fn build_view(&self) -> Result<(), FetchError> {
        let Some(job) = self.state.keyspace_state.lock().rebuild_view() else {
            return Ok(());
        };
        let sorted = tokio::task::spawn_blocking(move || job.run()).await?;
        // A sweep restart or sort change meanwhile drops it; the FetchKeys
        // that follows such a change builds the order again.
        self.state.keyspace_state.lock().install_sorted(sorted);
        Ok(())
    }

    fn action_hook(&self, action: Action) {
        crate::action::try_send_action(&self.tx, action);
    }
}

/// The load-all loop. Returns `true` when the sweep reached its end.
async fn sweep_to_end(
    storage: &Storage,
    state: &SharedState,
    tx: &ActionSender<Action>,
    until: Option<&str>,
) -> bool {
    let mut last_notice = std::time::Instant::now();
    loop {
        let request = {
            let keyspace = state.keyspace_state.lock();
            if until.is_some_and(|key| keyspace.index_of(key).is_some()) {
                return false;
            }
            match keyspace.load_request() {
                Some(request) => request,
                None => return true,
            }
        };
        let result = match storage.scan_until(&request).await {
            Ok(result) => result,
            Err(err) if err.is::<TypeScanRejected>() => {
                state.keyspace_state.lock().disable_server_type_scan();
                continue;
            }
            Err(err) => {
                log::error!("Load all failed: {err:?}");
                crate::action::try_send_action(
                    tx,
                    Action::Error(format!("Loading all keys failed: {err}")),
                );
                return false;
            }
        };
        let loaded = {
            let mut keyspace = state.keyspace_state.lock();
            // `Stale`: the sweep restarted (rescan, filter, DB); stop.
            // `Raced`: a page fetch moved the sweep first; go on from there.
            if keyspace.apply_result(&request, result) == SweepApply::Stale {
                return false;
            }
            keyspace.scanned.len()
        };
        if last_notice.elapsed() >= LOAD_NOTIFY {
            last_notice = std::time::Instant::now();
            crate::action::try_send_action(tx, Action::LoadAllProgress(loaded));
        }
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
