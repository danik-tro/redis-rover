//! Live updates of the open key.
//!
//! A background task owns one connection of its own. On a RESP3 server it
//! runs `CLIENT TRACKING ON OPTIN NOLOOP` and arms the open key with
//! `CLIENT CACHING YES` + `TYPE` (+ `PTTL`); the server then pushes one
//! `invalidate` when the key changes. The task re-arms the key right away,
//! before telling the App (`Action::OpenKeyChanged`), so a change that lands
//! while the App refetches is never missed. The value itself is still
//! fetched by the value task (`FetchValue`).
//!
//! A lost connection is reconnected with the Pub/Sub task's [`backoff`]; the
//! key is re-armed and refetched once, since changes in the gap were missed.
//!
//! Without tracking (RESP2, a refused `CLIENT TRACKING`, or
//! `live.mode = "poll"`) the task polls instead: every `poll_interval_ms`,
//! while the key list is on screen, it takes a cheap fingerprint of the open
//! key ([`fingerprint`]) and reports a change only when it differs.
//!
//! Notices are spaced at least [`NOTICE_GAP`] apart (the last one always
//! goes out), so a key written many times a second doesn't restart the
//! value fetch on every write.

use std::{
    hash::{DefaultHasher, Hash, Hasher},
    sync::Arc,
    time::Duration,
};

use parking_lot::Mutex;

use redis::{
    aio::{ConnectionManager, MultiplexedConnection},
    AsyncConnectionConfig, ProtocolVersion, PushInfo, PushKind, Value,
};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::mpsc::{self, Sender, UnboundedReceiver, UnboundedSender},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

use super::{client, pubsub::backoff, types::ValueState};
use crate::{
    action::{try_send_action, Action},
    config::{LiveConfig, LiveMode},
};

/// A key gone within this long of its known expiry counts as expired.
const EXPIRY_SLACK: Duration = Duration::from_secs(1);

/// Least time between two `OpenKeyChanged` notices.
pub const NOTICE_GAP: Duration = Duration::from_millis(100);

/// Keys up to this size (`MEMORY USAGE`) are fingerprinted whole with
/// `DUMP`; larger ones by the page on screen.
const DUMP_MAX_BYTES: u64 = 64 * 1024;

/// How long opening the live connection (and each setup command) may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The name the live connection carries in `CLIENT LIST`.
pub const CLIENT_NAME: &str = "rrover-live";

/// What the startup check found out about the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Capability {
    /// `HELLO 3` failed: RESP2 only (before Redis 6, or a proxy).
    Resp2,
    /// RESP3, but `CLIENT TRACKING ON` was refused (with the server's reason).
    TrackingRefused(String),
    /// RESP3 and `CLIENT TRACKING ON` accepted.
    Tracking,
}

/// How the open key is followed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// The server pushes invalidations.
    Track,
    /// Poll; `reason` says why tracking isn't used (`None` when configured).
    Poll { reason: Option<String> },
    /// Neither: `live.mode = "tracking"` on a server that can't track.
    NotLive(String),
    /// `live.mode = "off"`.
    Off,
}

/// Pick the plan for `mode` given what the server can do. `capability` is
/// `None` when it wasn't checked (`poll` and `off` never need it).
pub fn choose(mode: LiveMode, capability: Option<&Capability>) -> Plan {
    let missing = |capability: Option<&Capability>| match capability {
        Some(Capability::Resp2) | None => "no RESP3".to_owned(),
        Some(Capability::TrackingRefused(reason)) => reason.clone(),
        Some(Capability::Tracking) => unreachable!("tracking is available"),
    };
    match (mode, capability) {
        (LiveMode::Off, _) => Plan::Off,
        (LiveMode::Poll, _) => Plan::Poll { reason: None },
        (LiveMode::Auto | LiveMode::Tracking, Some(Capability::Tracking)) => Plan::Track,
        (LiveMode::Auto, other) => Plan::Poll {
            reason: Some(missing(other)),
        },
        (LiveMode::Tracking, other) => Plan::NotLive(missing(other)),
    }
}

/// Whether `mode` needs the RESP3 / `CLIENT TRACKING` check at all.
pub fn needs_capability(mode: LiveMode) -> bool {
    matches!(mode, LiveMode::Auto | LiveMode::Tracking)
}

/// Control messages from the App.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveCmd {
    /// Follow `key` on DB `db` (the key whose value is open).
    Track { key: String, db: i64 },
    /// No key is open.
    Untrack,
    /// The key list isn't on screen (`true`) or is again (`false`).
    Pause(bool),
}

/// Whether the open key is live, as the value pane shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Liveness {
    /// The server pushes changes (`CLIENT TRACKING`).
    Live,
    /// Polled every this many milliseconds.
    Polling { every_ms: u64 },
    /// Not following changes, and why.
    NotLive(String),
}

impl Default for Liveness {
    fn default() -> Self {
        Self::NotLive("connecting".to_owned())
    }
}

/// What happened to the open key.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum KeyChange {
    /// It changed (or the server can't tell, after a reconnect); `r_type` is
    /// its `TYPE` now.
    Changed { r_type: String },
    /// It no longer exists.
    #[default]
    Deleted,
    /// It no longer exists and its TTL had just run out.
    Expired,
}

/// Run the live task until `cancel` fires or the command channel closes.
/// The connection opens on the first `Track`.
/// `value_state` is the App's: polling fingerprints the page it shows.
pub async fn run(
    client: redis::Client,
    config: LiveConfig,
    value_state: Arc<Mutex<ValueState>>,
    mut cmd_rx: UnboundedReceiver<LiveCmd>,
    action_tx: Sender<Action>,
    cancel: CancellationToken,
) {
    let mut task = LiveTask::new(client, config, value_state, action_tx);
    loop {
        let retry_at = task.retry.map(|(_, at)| at);
        let poll_at = task.poll.as_ref().and_then(|poll| poll.next_at);
        let notice_at = task.pending.as_ref().map(|(at, _)| *at);
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(cmd) => task.apply(cmd).await,
                None => break,
            },
            push = next_push(task.conn.as_mut()) => task.on_push(push).await,
            () = sleep_until(retry_at) => task.reconnect().await,
            () = sleep_until(poll_at) => task.poll().await,
            () = sleep_until(notice_at) => task.flush_notice(Instant::now()),
            () = cancel.cancelled() => break,
        }
    }
}

/// The tracking connection and the pushes it delivers.
struct Tracked {
    conn: MultiplexedConnection,
    pushes: UnboundedReceiver<PushInfo>,
}

/// Next push of the open connection (`None`: its sender is gone, so the
/// connection is too); never resolves without a connection.
async fn next_push(conn: Option<&mut Tracked>) -> Option<PushInfo> {
    match conn {
        Some(tracked) => tracked.pushes.recv().await,
        None => std::future::pending().await,
    }
}

/// Sleep until `at`; never resolves without one.
async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

struct LiveTask {
    /// The client the app started with; the connection is built from it
    /// with the tracked key's DB.
    client: redis::Client,
    config: LiveConfig,
    /// The App's value state: what polling fingerprints.
    value_state: Arc<Mutex<ValueState>>,
    action_tx: Sender<Action>,
    conn: Option<Tracked>,
    /// DB the connection is (or will be) opened on.
    db: i64,
    /// The open key.
    key: Option<String>,
    /// When the open key expires, from the `PTTL` read when it was armed.
    expires_at: Option<Instant>,
    /// Pending reconnect: attempt number and when it starts.
    retry: Option<(u32, Instant)>,
    /// The status last sent to the App.
    status: Option<Liveness>,
    /// The server can't track and polling isn't wanted (`live.mode =
    /// "tracking"`); don't ask again on every key.
    no_tracking: bool,
    /// Polling instead of tracking (`Some` from the start in poll mode, or
    /// once the server turned out unable to track).
    poll: Option<Poll>,
    /// The key list is off screen (dashboard, Pub/Sub): no polling.
    paused: bool,
    /// When the last `OpenKeyChanged` went out.
    last_notice: Option<Instant>,
    /// A notice held back by [`NOTICE_GAP`], and when it is due.
    pending: Option<(Instant, Action)>,
}

/// Poll mode state.
#[derive(Default)]
struct Poll {
    /// Its connection, on `db`. The manager reconnects by itself; a
    /// fingerprint taken after a gap still shows what changed in it.
    conn: Option<(i64, ConnectionManager)>,
    /// When the next check runs (`None`: no key, or paused).
    next_at: Option<Instant>,
    /// The last fingerprint and what it was taken of.
    last: Option<(View, u64)>,
    /// `DUMP` was refused (ACL): fingerprint by the page instead.
    no_dump: bool,
}

/// What a fingerprint covers: a page turn or an opened JSON field changes
/// it without the key changing, so the baseline restarts.
#[derive(Debug, Clone, PartialEq, Eq)]
struct View {
    key: String,
    page: usize,
    json_path: Vec<String>,
    whole: bool,
}

impl LiveTask {
    fn new(
        client: redis::Client,
        config: LiveConfig,
        value_state: Arc<Mutex<ValueState>>,
        action_tx: Sender<Action>,
    ) -> Self {
        let db = client.get_connection_info().redis_settings().db();
        Self {
            client,
            config,
            value_state,
            action_tx,
            conn: None,
            db,
            key: None,
            expires_at: None,
            retry: None,
            status: None,
            no_tracking: false,
            poll: (config.mode == LiveMode::Poll).then(Poll::default),
            paused: false,
            last_notice: None,
            pending: None,
        }
    }

    async fn apply(&mut self, cmd: LiveCmd) {
        match cmd {
            LiveCmd::Track { key, db } => {
                let same_db = db == self.db;
                self.db = db;
                self.key = Some(key);
                self.expires_at = None;
                if self.poll.is_some() {
                    self.start_polling().await;
                    return;
                }
                if self.retry.is_some() || self.no_tracking {
                    // Down: armed on reconnect.
                    return;
                }
                if self.conn.is_some() && !same_db {
                    // Tracking needs the arming read on the key's DB.
                    self.conn = None;
                }
                if self.conn.is_none() {
                    // A fresh connection: the App is fetching the value
                    // already, so only arm it.
                    self.connect(false).await;
                } else {
                    self.arm().await;
                }
            }
            LiveCmd::Untrack => {
                self.key = None;
                self.expires_at = None;
                self.pending = None;
                if let Some(poll) = &mut self.poll {
                    poll.next_at = None;
                    poll.last = None;
                }
            }
            LiveCmd::Pause(paused) => {
                self.paused = paused;
                let due = (!paused && self.key.is_some()).then(Instant::now);
                if let Some(poll) = &mut self.poll {
                    poll.next_at = due;
                }
            }
        }
    }

    /// Open the connection on `self.db` and arm the open key. With
    /// `refetch`, tell the App the key may have changed (after a reconnect).
    async fn connect(&mut self, refetch: bool) {
        let (push_tx, pushes) = mpsc::unbounded_channel();
        let opened = match self.db_client() {
            Ok(client) => open_tracking(&client, Some(push_tx)).await,
            Err(err) => Err(err),
        };
        match opened {
            Ok((Capability::Tracking, Some(conn))) => {
                self.conn = Some(Tracked { conn, pushes });
                // `arm` reports `Live` once the key is armed.
                if refetch {
                    self.key_changed().await;
                } else {
                    self.arm().await;
                }
            }
            Ok((capability, _)) => match choose(self.config.mode, Some(&capability)) {
                Plan::Poll { reason } => {
                    log::info!(
                        "Live updates: polling ({})",
                        reason.as_deref().unwrap_or("configured")
                    );
                    self.poll = Some(Poll::default());
                    self.start_polling().await;
                }
                Plan::NotLive(reason) => {
                    log::info!("Live updates: off ({reason})");
                    self.no_tracking = true;
                    self.set_status(Liveness::NotLive(reason));
                }
                Plan::Track | Plan::Off => {}
            },
            Err(err) => {
                log::warn!("Opening the live connection failed: {err}");
                self.schedule(1);
            }
        }
    }

    fn db_client(&self) -> redis::RedisResult<redis::Client> {
        let info = self.client.get_connection_info().clone();
        let settings = info.redis_settings().clone().set_db(self.db);
        redis::Client::open(info.set_redis_settings(settings))
    }

    /// Make the server track the open key: `CLIENT CACHING YES` applies to
    /// the next command, `TYPE`. Returns the key's type (`none` when it is
    /// gone), or `None` without a key or when the connection was lost.
    async fn arm(&mut self) -> Option<String> {
        let key = self.key.clone()?;
        let tracked = self.conn.as_mut()?;
        let reply: redis::RedisResult<(String, i64)> = redis::pipe()
            .cmd("CLIENT")
            .arg("CACHING")
            .arg("YES")
            .ignore()
            .cmd("TYPE")
            .arg(&key)
            .cmd("PTTL")
            .arg(&key)
            .query_async(&mut tracked.conn)
            .await;
        match reply {
            Ok((r_type, pttl)) => {
                self.expires_at = u64::try_from(pttl)
                    .ok()
                    .map(|ms| Instant::now() + Duration::from_millis(ms));
                // Only now: a change before this point would be missed.
                self.set_status(Liveness::Live);
                Some(r_type)
            }
            Err(err) if is_connection_error(&err) => {
                self.lost(&err.to_string());
                None
            }
            Err(err) => {
                // Not armed: say so rather than look live.
                log::warn!("Arming {key} for tracking failed: {err}");
                self.set_status(Liveness::NotLive(err.to_string()));
                None
            }
        }
    }

    async fn on_push(&mut self, push: Option<PushInfo>) {
        let Some(push) = push else {
            self.lost("the connection closed");
            return;
        };
        match push.kind {
            PushKind::Disconnection => self.lost("the connection closed"),
            PushKind::Invalidate
                if self
                    .key
                    .as_deref()
                    .is_some_and(|key| invalidates(&push.data, key)) =>
            {
                self.key_changed().await;
            }
            _ => {}
        }
    }

    /// The open key changed (or may have): re-arm it, then tell the App
    /// what it is now.
    async fn key_changed(&mut self) {
        let expired_by = self.expires_at;
        let Some(r_type) = self.arm().await else {
            return;
        };
        let Some(key) = self.key.clone() else {
            return;
        };
        let change = classify(r_type, expired_by);
        self.notify(key, change);
    }

    /// Send `OpenKeyChanged`, or hold it until [`NOTICE_GAP`] has passed
    /// since the last one (a newer change replaces a held one).
    fn notify(&mut self, key: String, change: KeyChange) {
        self.notify_at(key, change, Instant::now());
    }

    fn notify_at(&mut self, key: String, change: KeyChange, now: Instant) {
        let action = Action::OpenKeyChanged { key, change };
        match self.last_notice {
            Some(last) if now < last + NOTICE_GAP => {
                self.pending = Some((last + NOTICE_GAP, action));
            }
            _ => {
                self.last_notice = Some(now);
                self.pending = None;
                try_send_action(&self.action_tx, action);
            }
        }
    }

    fn flush_notice(&mut self, now: Instant) {
        if let Some((_, action)) = self.pending.take() {
            self.last_notice = Some(now);
            try_send_action(&self.action_tx, action);
        }
    }

    /// Open the polling connection on `self.db` (again after a DB switch)
    /// and check the open key at once, which sets the baseline.
    async fn start_polling(&mut self) {
        let Some(poll) = &mut self.poll else {
            return;
        };
        poll.last = None;
        poll.next_at = None;
        if poll.conn.as_ref().is_none_or(|(db, _)| *db != self.db) {
            poll.conn = None;
            let opened = match self.db_client() {
                Ok(client) => tokio::time::timeout(CONNECT_TIMEOUT, ConnectionManager::new(client))
                    .await
                    .unwrap_or_else(|_| {
                        Err(redis::RedisError::from((redis::ErrorKind::Io, "timed out")))
                    }),
                Err(err) => Err(err),
            };
            match opened {
                Ok(conn) => {
                    if let Some(poll) = &mut self.poll {
                        poll.conn = Some((self.db, conn));
                    }
                }
                Err(err) => {
                    log::warn!("Opening the polling connection failed: {err}");
                    self.schedule(1);
                    return;
                }
            }
        }
        self.set_status(self.polling_status());
        let due = (!self.paused && self.key.is_some()).then(Instant::now);
        if let Some(poll) = &mut self.poll {
            poll.next_at = due;
        }
    }

    fn polling_status(&self) -> Liveness {
        let every_ms = u64::try_from(self.config.poll_interval().as_millis()).unwrap_or(u64::MAX);
        Liveness::Polling { every_ms }
    }

    /// A poll is due: fingerprint the open key and report a change.
    async fn poll(&mut self) {
        let interval = self.config.poll_interval();
        let Some(poll) = &mut self.poll else {
            return;
        };
        poll.next_at = Some(Instant::now() + interval);
        let (Some(key), Some((_, conn))) = (self.key.clone(), poll.conn.clone()) else {
            return;
        };
        let snapshot = self.value_state.lock().clone();
        if snapshot.key.as_deref() != Some(key.as_str()) {
            // The App hasn't opened it yet.
            return;
        }
        let whole = !poll.no_dump && snapshot.size.is_some_and(|size| size <= DUMP_MAX_BYTES);
        let view = View {
            key: key.clone(),
            page: snapshot.page,
            json_path: snapshot.json_path.clone(),
            whole,
        };
        let taken = fingerprint(conn, snapshot, whole).await;
        let Some(poll) = &mut self.poll else {
            return;
        };
        let print = match taken {
            Ok(print) => print,
            Err(Failure::DumpRefused(err)) => {
                log::info!("DUMP refused, polling by page: {err}");
                poll.no_dump = true;
                poll.next_at = Some(Instant::now());
                return;
            }
            Err(Failure::Connection(err)) => {
                log::warn!("Polling {key} failed: {err}");
                self.set_status(Liveness::NotLive("reconnecting".to_owned()));
                return;
            }
            Err(Failure::Other(err)) => {
                log::warn!("Polling {key} failed: {err}");
                self.set_status(Liveness::NotLive(err));
                return;
            }
        };
        let previous = poll.last.replace((view.clone(), print.hash));
        let expired_by = std::mem::replace(
            &mut self.expires_at,
            u64::try_from(print.pttl)
                .ok()
                .map(|ms| Instant::now() + Duration::from_millis(ms)),
        );
        self.set_status(self.polling_status());
        if let Some((last_view, last_hash)) = previous {
            if last_view == view && last_hash != print.hash {
                self.notify(key, classify(print.r_type, expired_by));
            }
        }
    }

    /// A reconnect attempt is due.
    async fn reconnect(&mut self) {
        let Some((attempt, _)) = self.retry.take() else {
            return;
        };
        if self.key.is_none() {
            // Nothing open: connect again on the next `Track`.
            return;
        }
        if self.poll.is_some() {
            self.start_polling().await;
        } else {
            self.connect(true).await;
        }
        if self.retry.is_some() {
            // `connect` scheduled attempt 1 again; keep counting.
            self.schedule(attempt.saturating_add(1));
        } else {
            log::info!("Live connection back after {attempt} attempt(s)");
        }
    }

    /// Plan reconnect attempt `attempt` after its [`backoff`].
    fn schedule(&mut self, attempt: u32) {
        self.retry = Some((attempt, Instant::now() + backoff(attempt)));
        self.set_status(Liveness::NotLive("reconnecting".to_owned()));
    }

    /// The connection is gone: say so, then reconnect.
    fn lost(&mut self, reason: &str) {
        if self.conn.take().is_none() {
            return;
        }
        log::warn!("Live connection lost: {reason}");
        self.schedule(1);
    }

    fn set_status(&mut self, status: Liveness) {
        if self.status.as_ref() != Some(&status) {
            self.status = Some(status.clone());
            try_send_action(&self.action_tx, Action::LiveStatus(status));
        }
    }
}

/// What a key that changed is now: its type, or gone. Gone within
/// [`EXPIRY_SLACK`] of its known expiry counts as expired.
fn classify(r_type: String, expired_by: Option<Instant>) -> KeyChange {
    classify_at(r_type, expired_by, Instant::now())
}

fn classify_at(r_type: String, expired_by: Option<Instant>, now: Instant) -> KeyChange {
    if r_type != "none" {
        KeyChange::Changed { r_type }
    } else if expired_by.is_some_and(|at| at <= now + EXPIRY_SLACK) {
        KeyChange::Expired
    } else {
        KeyChange::Deleted
    }
}

/// A polled key's state.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Print {
    r_type: String,
    pttl: i64,
    hash: u64,
}

#[derive(Debug)]
enum Failure {
    DumpRefused(String),
    Connection(String),
    Other(String),
}

/// Fingerprint the key `snapshot` names: `TYPE` and `PTTL`, plus either
/// `DUMP` (`whole`, small keys: catches a change anywhere in the key) or
/// the page on screen, fetched as the value task does (bounded: one page,
/// at most 64 KiB of a string). `OBJECT IDLETIME` isn't used: our own reads
/// reset it, and it errors under an LFU policy.
async fn fingerprint(
    mut conn: ConnectionManager,
    snapshot: ValueState,
    whole: bool,
) -> Result<Print, Failure> {
    let Some(key) = snapshot.key.clone() else {
        return Err(Failure::Other("no key".to_owned()));
    };
    let mut pipe = redis::pipe();
    pipe.cmd("TYPE").arg(&key).cmd("PTTL").arg(&key);
    let reply = if whole {
        pipe.cmd("DUMP").arg(&key);
        pipe.query_async::<(String, i64, Option<Vec<u8>>)>(&mut conn)
            .await
            .map(|(r_type, pttl, dump)| (r_type, pttl, Some(dump)))
    } else {
        pipe.query_async::<(String, i64)>(&mut conn)
            .await
            .map(|(r_type, pttl)| (r_type, pttl, None))
    };
    let (r_type, pttl, dump) = reply.map_err(|err| {
        if is_connection_error(&err) {
            Failure::Connection(err.to_string())
        } else if whole {
            Failure::DumpRefused(err.to_string())
        } else {
            Failure::Other(err.to_string())
        }
    })?;
    let mut hasher = DefaultHasher::new();
    r_type.hash(&mut hasher);
    match dump {
        Some(dump) => dump.hash(&mut hasher),
        None if r_type == "none" => {}
        None if super::types::RedisType::from(r_type.as_str()) != snapshot.r_type => {
            // Another type: the page can't be read as the old one, and the
            // type alone already differs.
        }
        None => {
            // The App's state holds the HSCAN / SSCAN sweep it already did;
            // read the page fresh.
            let mut snapshot = snapshot;
            snapshot.rescan();
            let (_, loaded) = client::fetch_value_page(conn, snapshot)
                .await
                .map_err(|err| match err.downcast::<redis::RedisError>() {
                    Ok(err) if is_connection_error(&err) => Failure::Connection(err.to_string()),
                    Ok(err) => Failure::Other(err.to_string()),
                    Err(err) => Failure::Other(err.to_string()),
                })?;
            format!("{:?}", loaded.value).hash(&mut hasher);
            format!("{:?}", loaded.page).hash(&mut hasher);
        }
    }
    Ok(Print {
        r_type,
        pttl,
        hash: hasher.finish(),
    })
}

/// Whether an `invalidate` push's data names `key`. A nil payload is a
/// flush (`FLUSHDB` / `FLUSHALL`), which invalidates every key.
fn invalidates(data: &[Value], key: &str) -> bool {
    match data.first() {
        Some(Value::Nil) | None => true,
        Some(Value::Array(keys) | Value::Set(keys)) => keys.iter().any(|k| match k {
            Value::BulkString(bytes) => bytes == key.as_bytes(),
            Value::SimpleString(s) => s == key,
            _ => false,
        }),
        Some(_) => false,
    }
}

/// Check the server and, if it can, open a RESP3 connection with
/// `CLIENT TRACKING ON OPTIN NOLOOP`. Invalidation pushes (and the
/// library's `Disconnection` notice) go to `push`.
///
/// # Errors
///
/// The server can't be reached: connecting, `HELLO 3` or the tracking setup
/// timed out or lost the connection.
pub async fn open_tracking(
    client: &redis::Client,
    push: Option<UnboundedSender<PushInfo>>,
) -> redis::RedisResult<(Capability, Option<MultiplexedConnection>)> {
    if !crate::probe::resp3_supported(client).await? {
        return Ok((Capability::Resp2, None));
    }
    let info = client.get_connection_info().clone();
    let settings = info
        .redis_settings()
        .clone()
        .set_protocol(ProtocolVersion::RESP3);
    let resp3 = redis::Client::open(info.set_redis_settings(settings))?;
    let mut config = AsyncConnectionConfig::new()
        .set_connection_timeout(Some(CONNECT_TIMEOUT))
        .set_response_timeout(Some(CONNECT_TIMEOUT));
    if let Some(push) = push {
        config = config.set_push_sender(push);
    }
    let mut conn = resp3
        .get_multiplexed_async_connection_with_config(&config)
        .await?;
    // Only a label for `CLIENT LIST`; a server that refuses it still tracks.
    if let Err(err) = redis::cmd("CLIENT")
        .arg("SETNAME")
        .arg(CLIENT_NAME)
        .exec_async(&mut conn)
        .await
    {
        log::debug!("CLIENT SETNAME on the live connection failed: {err}");
    }
    match redis::cmd("CLIENT")
        .arg(&["TRACKING", "ON", "OPTIN", "NOLOOP"])
        .exec_async(&mut conn)
        .await
    {
        Ok(()) => Ok((Capability::Tracking, Some(conn))),
        Err(err) if is_connection_error(&err) => Err(err),
        Err(err) => Ok((Capability::TrackingRefused(err.to_string()), None)),
    }
}

pub fn is_connection_error(err: &redis::RedisError) -> bool {
    err.is_connection_dropped()
        || err.is_io_error()
        || err.is_connection_refusal()
        || err.is_timeout()
}

/// The `--probe` row: what `live` would do on this server.
pub fn plan_line(config: &LiveConfig, plan: &Plan) -> String {
    match plan {
        Plan::Track => "tracking".to_owned(),
        Plan::Poll { reason } => {
            let secs = config.poll_interval().as_secs_f64();
            match reason {
                Some(reason) => format!("polling {secs}s ({reason})"),
                None => format!("polling {secs}s"),
            }
        }
        Plan::NotLive(reason) => format!("not live ({reason})"),
        Plan::Off => "off".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_per_mode_and_capability() {
        let refused = Capability::TrackingRefused("ERR unknown subcommand".into());
        let cases = [
            (LiveMode::Auto, Some(Capability::Tracking), Plan::Track),
            (
                LiveMode::Auto,
                Some(Capability::Resp2),
                Plan::Poll {
                    reason: Some("no RESP3".into()),
                },
            ),
            (
                LiveMode::Auto,
                Some(refused.clone()),
                Plan::Poll {
                    reason: Some("ERR unknown subcommand".into()),
                },
            ),
            (LiveMode::Tracking, Some(Capability::Tracking), Plan::Track),
            (
                LiveMode::Tracking,
                Some(Capability::Resp2),
                Plan::NotLive("no RESP3".into()),
            ),
            (
                LiveMode::Tracking,
                Some(refused),
                Plan::NotLive("ERR unknown subcommand".into()),
            ),
            (LiveMode::Poll, None, Plan::Poll { reason: None }),
            (
                LiveMode::Poll,
                Some(Capability::Tracking),
                Plan::Poll { reason: None },
            ),
            (LiveMode::Off, Some(Capability::Tracking), Plan::Off),
        ];
        for (mode, capability, plan) in cases {
            assert_eq!(
                choose(mode, capability.as_ref()),
                plan,
                "{mode:?} {capability:?}"
            );
        }
        assert!(needs_capability(LiveMode::Auto));
        assert!(needs_capability(LiveMode::Tracking));
        assert!(!needs_capability(LiveMode::Poll));
        assert!(!needs_capability(LiveMode::Off));
    }

    fn task(config: LiveConfig) -> (LiveTask, mpsc::Receiver<Action>) {
        let (tx, rx) = mpsc::channel(16);
        let client = redis::Client::open("redis://127.0.0.1:1/").unwrap();
        let task = LiveTask::new(client, config, Arc::default(), tx);
        (task, rx)
    }

    fn changed(n: u32) -> KeyChange {
        KeyChange::Changed {
            r_type: format!("t{n}"),
        }
    }

    #[tokio::test]
    async fn notices_are_spaced_and_the_last_one_goes_out() {
        let (mut task, mut rx) = task(LiveConfig::default());
        let t0 = Instant::now();
        task.notify_at("k".into(), changed(1), t0);
        assert!(matches!(rx.try_recv(), Ok(Action::OpenKeyChanged { .. })));

        // Two more within the gap: held, the newer replacing the older.
        let soon = t0 + Duration::from_millis(10);
        task.notify_at("k".into(), changed(2), soon);
        task.notify_at("k".into(), changed(3), soon);
        assert!(rx.try_recv().is_err());
        let (due, _) = task.pending.as_ref().expect("held");
        assert_eq!(*due, t0 + NOTICE_GAP);

        task.flush_notice(t0 + NOTICE_GAP);
        assert_eq!(
            rx.try_recv().unwrap(),
            Action::OpenKeyChanged {
                key: "k".into(),
                change: changed(3)
            }
        );
        assert!(task.pending.is_none());

        task.notify_at("k".into(), changed(4), t0 + NOTICE_GAP * 2);
        assert!(rx.try_recv().is_ok(), "after the gap: sent at once");
    }

    #[test]
    fn gone_keys_are_deleted_unless_their_ttl_ran_out() {
        let now = Instant::now();
        assert_eq!(classify_at("hash".into(), None, now), changed_type("hash"));
        assert_eq!(classify_at("none".into(), None, now), KeyChange::Deleted);
        assert_eq!(
            classify_at("none".into(), Some(now + Duration::from_millis(500)), now),
            KeyChange::Expired
        );
        assert_eq!(
            classify_at("none".into(), Some(now + Duration::from_mins(1)), now),
            KeyChange::Deleted,
            "deleted long before it would expire"
        );
    }

    fn changed_type(r_type: &str) -> KeyChange {
        KeyChange::Changed {
            r_type: r_type.into(),
        }
    }

    #[tokio::test]
    async fn poll_mode_waits_for_a_key_and_pauses() {
        let config = LiveConfig {
            mode: LiveMode::Poll,
            ..LiveConfig::default()
        };
        let (mut task, _rx) = task(config);
        assert!(task.poll.is_some(), "poll mode polls from the start");
        task.apply(LiveCmd::Pause(false)).await;
        assert_eq!(task.poll.as_ref().unwrap().next_at, None, "no key yet");

        task.key = Some("k".into());
        task.apply(LiveCmd::Pause(false)).await;
        assert!(task.poll.as_ref().unwrap().next_at.is_some());
        task.apply(LiveCmd::Pause(true)).await;
        assert_eq!(task.poll.as_ref().unwrap().next_at, None, "paused");
        task.apply(LiveCmd::Pause(false)).await;
        task.apply(LiveCmd::Untrack).await;
        assert_eq!(task.poll.as_ref().unwrap().next_at, None, "nothing open");
    }

    #[test]
    fn invalidate_names_the_key_or_flushes() {
        let keys = |names: &[&str]| {
            vec![Value::Array(
                names
                    .iter()
                    .map(|n| Value::BulkString(n.as_bytes().to_vec()))
                    .collect(),
            )]
        };
        assert!(invalidates(&keys(&["user:1"]), "user:1"));
        assert!(invalidates(&keys(&["a", "user:1"]), "user:1"));
        assert!(!invalidates(&keys(&["user:2"]), "user:1"));
        assert!(!invalidates(&keys(&["user:10"]), "user:1"));
        assert!(invalidates(&[Value::Nil], "user:1"), "a flush");
        assert!(!invalidates(&[Value::Int(1)], "user:1"));
    }

    #[test]
    fn probe_line_per_plan() {
        let cfg = LiveConfig::default();
        assert_eq!(plan_line(&cfg, &Plan::Track), "tracking");
        assert_eq!(
            plan_line(
                &cfg,
                &Plan::Poll {
                    reason: Some("no RESP3".into())
                }
            ),
            "polling 2s (no RESP3)"
        );
        let fast = LiveConfig {
            poll_interval_ms: 500,
            ..cfg
        };
        assert_eq!(
            plan_line(&fast, &Plan::Poll { reason: None }),
            "polling 0.5s"
        );
        assert_eq!(
            plan_line(&cfg, &Plan::NotLive("no RESP3".into())),
            "not live (no RESP3)"
        );
        assert_eq!(plan_line(&cfg, &Plan::Off), "off");
    }
}
