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

use std::time::Duration;

use redis::{
    aio::MultiplexedConnection, AsyncConnectionConfig, ProtocolVersion, PushInfo, PushKind, Value,
};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::mpsc::{self, Sender, UnboundedReceiver, UnboundedSender},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

use super::pubsub::backoff;
use crate::{
    action::{try_send_action, Action},
    config::{LiveConfig, LiveMode},
};

/// A key gone within this long of its known expiry counts as expired.
const EXPIRY_SLACK: Duration = Duration::from_secs(1);

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
pub async fn run(
    client: redis::Client,
    config: LiveConfig,
    mut cmd_rx: UnboundedReceiver<LiveCmd>,
    action_tx: Sender<Action>,
    cancel: CancellationToken,
) {
    let db = client.get_connection_info().redis_settings().db();
    let mut task = LiveTask {
        client,
        config,
        action_tx,
        conn: None,
        db,
        key: None,
        expires_at: None,
        retry: None,
        status: None,
        no_tracking: false,
    };
    loop {
        let retry_at = task.retry.map(|(_, at)| at);
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(cmd) => task.apply(cmd).await,
                None => break,
            },
            push = next_push(task.conn.as_mut()) => task.on_push(push).await,
            () = sleep_until(retry_at) => task.reconnect().await,
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
    /// The server can't track; don't ask again on every key.
    no_tracking: bool,
}

impl LiveTask {
    async fn apply(&mut self, cmd: LiveCmd) {
        match cmd {
            LiveCmd::Track { key, db } => {
                let same_db = db == self.db;
                self.db = db;
                self.key = Some(key);
                self.expires_at = None;
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
            }
            LiveCmd::Pause(_) => {}
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
            Ok((capability, _)) => {
                let reason = match choose(self.config.mode, Some(&capability)) {
                    Plan::Poll { reason } => reason,
                    Plan::NotLive(reason) => Some(reason),
                    Plan::Track | Plan::Off => None,
                };
                log::info!("Live updates: no CLIENT TRACKING ({capability:?})");
                self.no_tracking = true;
                self.set_status(Liveness::NotLive(
                    reason.unwrap_or_else(|| "no tracking".to_owned()),
                ));
            }
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
        let change = if r_type == "none" {
            if expired_by.is_some_and(|at| at <= Instant::now() + EXPIRY_SLACK) {
                KeyChange::Expired
            } else {
                KeyChange::Deleted
            }
        } else {
            KeyChange::Changed { r_type }
        };
        try_send_action(&self.action_tx, Action::OpenKeyChanged { key, change });
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
        self.connect(true).await;
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
