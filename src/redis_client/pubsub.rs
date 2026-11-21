//! Background task owning the dedicated Pub/Sub connection.
//!
//! A subscribed connection can't run other commands, so it is separate from
//! the shared `ConnectionManager`. Messages go straight into the
//! [`PubSubStore`]; the UI reads it on render, so there is no `Action` per
//! message.

use std::{sync::Arc, time::Duration, time::SystemTime};

use futures::StreamExt;
use parking_lot::Mutex;
use redis::{
    aio::{MultiplexedConnection, PubSubSink, PubSubStream},
    cluster::{ClusterClient, ClusterConfig},
    cluster_async::ClusterConnection,
    AsyncConnectionConfig, ConnectionInfo, ProtocolVersion, PushInfo, PushKind, Value,
};
use tokio::{
    sync::mpsc::{self, Sender, UnboundedReceiver},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

use crate::action::{try_send_action, Action};

use super::{
    bytes::{text_from_bytes, Raw},
    cluster::{is_topology_error, MasterNode},
    conn::client_at,
    pubsub_store::{ConnStatus, PubSubStore, Target},
    sentinel::ServerClient,
};

/// How long opening the Pub/Sub connection may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How often a cluster's shard subscriptions are checked against its masters.
const TOPOLOGY_CHECK: Duration = Duration::from_secs(1);
/// Longest wait between two reconnect attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Control messages from the App. The store already reflects them when they
/// are sent: `Subscribe` follows `PubSubStore::add`, `Unsubscribe` follows
/// `PubSubStore::remove`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PubSubCmd {
    Subscribe(Target),
    Unsubscribe(Target),
}

/// Wait before reconnect attempt `attempt` (1-based): 1 s, 2 s, 4 s … capped
/// at [`MAX_BACKOFF`].
pub fn backoff(attempt: u32) -> Duration {
    let exp = attempt.saturating_sub(1).min(16);
    Duration::from_secs(1 << exp).min(MAX_BACKOFF)
}

/// Run the Pub/Sub task until `cancel` fires or the command channel closes.
///
/// The connection opens on the first `Subscribe`, so a session that never
/// subscribes costs nothing. When it drops, the task reconnects with
/// [`backoff`] and subscribes every target in the store again; buffers and
/// stats are kept.
pub async fn run(
    client: impl Into<ServerClient>,
    cluster: Option<ClusterClient>,
    masters: Arc<Mutex<Arc<Vec<MasterNode>>>>,
    store: Arc<Mutex<PubSubStore>>,
    mut cmd_rx: UnboundedReceiver<PubSubCmd>,
    action_tx: Sender<Action>,
    cancel: CancellationToken,
) {
    let shard_masters = Arc::clone(&masters.lock());
    let mut task = PubSubTask {
        client: client.into(),
        cluster,
        shard_masters,
        masters,
        store,
        action_tx,
        conn: None,
        shard: None,
        retry: None,
        lost_targets: Vec::new(),
    };
    let mut topology = tokio::time::interval(TOPOLOGY_CHECK);
    topology.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let on_cluster = task.cluster.is_some();
    loop {
        let retry_at = task.retry.map(|(_, at)| at);
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(cmd) => task.apply(cmd).await,
                None => break,
            },
            _ = topology.tick(), if on_cluster => task.follow_topology().await,
            () = task.client.changed() => task.lost("the master changed"),
            msg = next_msg(task.conn.as_mut()) => match msg {
                Some(msg) => task.store.lock().push_msg(&msg, SystemTime::now()),
                None => task.lost("the server closed the connection"),
            },
            push = next_push(task.shard.as_mut()) => match push {
                Some(push) => task.on_push(&push),
                None => task.lost("the shard connection closed"),
            },
            () = sleep_until(retry_at) => task.reconnect().await,
            () = cancel.cancelled() => break,
        }
    }
}

/// Next message of the open connection; never resolves without one.
async fn next_msg(conn: Option<&mut (PubSubSink, PubSubStream)>) -> Option<redis::Msg> {
    match conn {
        Some((_, stream)) => stream.next().await,
        None => std::future::pending().await,
    }
}

/// Next push of the shard connection; never resolves without one.
async fn next_push(shard: Option<&mut ShardConn>) -> Option<PushInfo> {
    match shard {
        Some(shard) => shard.pushes.recv().await,
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

/// The RESP3 connection for shard channels: `aio::PubSub` has no
/// `SSUBSCRIBE`, so messages arrive as pushes on a multiplexed connection.
struct ShardConn {
    conn: ShardLink,
    pushes: UnboundedReceiver<PushInfo>,
}

/// One server, or a cluster connection that sends each shard channel to the
/// master of its slot and subscribes it again after a failover.
enum ShardLink {
    Single(MultiplexedConnection),
    Cluster(ClusterConnection),
}

impl ShardLink {
    async fn subscribe(&mut self, channel: &str) -> redis::RedisResult<()> {
        match self {
            Self::Single(conn) => redis::cmd("SSUBSCRIBE")
                .arg(Raw(channel))
                .query_async::<Value>(conn)
                .await
                .map(|_| ()),
            Self::Cluster(conn) => conn.ssubscribe(Raw(channel)).await,
        }
    }

    async fn unsubscribe(&mut self, channel: &str) -> redis::RedisResult<()> {
        match self {
            Self::Single(conn) => redis::cmd("SUNSUBSCRIBE")
                .arg(Raw(channel))
                .query_async::<Value>(conn)
                .await
                .map(|_| ()),
            Self::Cluster(conn) => conn.sunsubscribe(Raw(channel)).await,
        }
    }
}

struct PubSubTask {
    /// The server, or behind Sentinel whichever is the master now.
    client: ServerClient,
    /// RESP3 cluster client: routes each `SSUBSCRIBE` to the slot's owner.
    cluster: Option<ClusterClient>,
    /// The cluster's masters, kept current by the info task.
    masters: Arc<Mutex<Arc<Vec<MasterNode>>>>,
    /// The list the shard connection was last checked against.
    shard_masters: Arc<Vec<MasterNode>>,
    store: Arc<Mutex<PubSubStore>>,
    action_tx: Sender<Action>,
    conn: Option<(PubSubSink, PubSubStream)>,
    /// Opened with the first shard target; lost and reconnected with `conn`.
    shard: Option<ShardConn>,
    /// Pending reconnect: attempt number and when it starts.
    retry: Option<(u32, Instant)>,
    /// Targets held when the connection dropped; they get a marker row once
    /// it is back.
    lost_targets: Vec<Target>,
}

impl PubSubTask {
    async fn apply(&mut self, cmd: PubSubCmd) {
        match cmd {
            PubSubCmd::Subscribe(target) => {
                if self.retry.is_some() {
                    // Down: the target is in the store and gets subscribed
                    // on reconnect.
                    return;
                }
                if self.conn.is_none() {
                    // Connecting subscribes everything in the store, this
                    // target included.
                    if let Err(reason) = self.connect().await {
                        self.error(format!("Pub/Sub connection failed: {reason}"));
                        self.schedule(1);
                    }
                    return;
                }
                if let Err(err) = self.subscribe(&target).await {
                    if is_retryable(&err) {
                        // Kept in the store: resubscribed on reconnect.
                        self.lost(&err.to_string());
                    } else {
                        self.store.lock().remove(&target);
                        self.error(format!("Failed to subscribe to {target}: {err}"));
                    }
                }
            }
            PubSubCmd::Unsubscribe(target) => {
                let res = match (&target, self.conn.as_mut(), self.shard.as_mut()) {
                    (Target::Channel(name), Some((sink, _)), _) => sink.unsubscribe(name).await,
                    (Target::Pattern(name), Some((sink, _)), _) => sink.punsubscribe(name).await,
                    (Target::Shard(name), _, Some(shard)) => shard.conn.unsubscribe(name).await,
                    _ => return,
                };
                if let Err(err) = res {
                    log::warn!("Unsubscribe from {target} failed: {err}");
                    if is_retryable(&err) {
                        self.lost(&err.to_string());
                    }
                }
            }
        }
    }

    async fn subscribe(&mut self, target: &Target) -> redis::RedisResult<()> {
        if let Target::Shard(name) = target {
            if self.shard.is_none() {
                self.shard = Some(self.open_shard().await?);
            }
            let Some(shard) = self.shard.as_mut() else {
                return Ok(());
            };
            return shard.conn.subscribe(name).await;
        }
        let Some((sink, _)) = self.conn.as_mut() else {
            return Ok(());
        };
        match target {
            Target::Channel(name) => sink.subscribe(name).await,
            Target::Pattern(name) => sink.psubscribe(name).await,
            Target::Shard(_) => Ok(()),
        }
    }

    /// The cluster's masters changed. The client only resubscribes on a node
    /// it reconnects to, and a promoted replica isn't one, so shard channels
    /// go on a new connection, which subscribes them on the current owners.
    async fn follow_topology(&mut self) {
        let current = Arc::clone(&self.masters.lock());
        if Arc::ptr_eq(&current, &self.shard_masters) {
            return;
        }
        self.shard_masters = current;
        if self.shard.is_none() {
            return;
        }
        self.shard = None;
        let shards: Vec<Target> = self
            .store
            .lock()
            .targets()
            .into_iter()
            .filter(|target| matches!(target, Target::Shard(_)))
            .collect();
        for target in &shards {
            if let Err(err) = self.subscribe(target).await {
                log::warn!("Resubscribing {target} after a topology change failed: {err}");
                self.lost(&err.to_string());
                return;
            }
        }
        self.store
            .lock()
            .mark_reconnected(&shards, SystemTime::now());
    }

    /// A RESP3 connection whose pushes come to this task.
    async fn open_shard(&self) -> redis::RedisResult<ShardConn> {
        if let Some(cluster) = &self.cluster {
            // The seeds may be the node that just died: start from the
            // masters known now, then fall back on the seeds.
            if let Some(current) = self.current_masters_client() {
                if let Ok(shard) = open_cluster_shard(&current).await {
                    return Ok(shard);
                }
            }
            return open_cluster_shard(cluster).await;
        }
        let current = self.client.current();
        let info = current.get_connection_info().clone();
        let settings = info
            .redis_settings()
            .clone()
            .set_protocol(ProtocolVersion::RESP3);
        let client = redis::Client::open(info.set_redis_settings(settings))?;
        let (tx, pushes) = mpsc::unbounded_channel();
        let config = AsyncConnectionConfig::new()
            .set_connection_timeout(Some(CONNECT_TIMEOUT))
            .set_response_timeout(Some(CONNECT_TIMEOUT))
            .set_push_sender(tx);
        let conn = client
            .get_multiplexed_async_connection_with_config(&config)
            .await?;
        Ok(ShardConn {
            conn: ShardLink::Single(conn),
            pushes,
        })
    }

    /// A RESP3 cluster client seeded with the current masters.
    fn current_masters_client(&self) -> Option<ClusterClient> {
        let masters = Arc::clone(&self.masters.lock());
        let base = self.client.current();
        let nodes: Vec<ConnectionInfo> = masters
            .iter()
            .filter_map(|node| client_at(&base, &node.host, node.port).ok())
            .map(|client| {
                let info = client.get_connection_info().clone();
                let settings = info
                    .redis_settings()
                    .clone()
                    .set_protocol(ProtocolVersion::RESP3);
                info.set_redis_settings(settings)
            })
            .collect();
        if nodes.is_empty() {
            return None;
        }
        ClusterClient::new(nodes).ok()
    }

    /// The plain Pub/Sub connection. Any cluster node carries plain channels,
    /// so the current masters are tried first and the seed, which may be
    /// down, last. One server has no masters list and uses its own address.
    async fn open_pubsub(&self) -> Result<redis::aio::PubSub, String> {
        let masters = Arc::clone(&self.masters.lock());
        let base = self.client.current();
        let mut clients: Vec<redis::Client> = masters
            .iter()
            .filter_map(|node| client_at(&base, &node.host, node.port).ok())
            .collect();
        clients.push(base);
        let mut reason = String::new();
        for client in clients {
            match tokio::time::timeout(CONNECT_TIMEOUT, client.get_async_pubsub()).await {
                Ok(Ok(pubsub)) => return Ok(pubsub),
                Ok(Err(err)) => reason = err.to_string(),
                Err(_) => reason = format!("timed out after {CONNECT_TIMEOUT:?}"),
            }
        }
        Err(reason)
    }

    /// A push from the shard connection.
    fn on_push(&mut self, push: &PushInfo) {
        match push.kind {
            PushKind::SMessage => {
                if let [Value::BulkString(channel), Value::BulkString(payload)] =
                    push.data.as_slice()
                {
                    let channel = text_from_bytes(channel);
                    self.store
                        .lock()
                        .push_shard(&channel, payload, SystemTime::now());
                }
            }
            // One node of a cluster dropping isn't the end of the link: the
            // client reconnects to it, `follow_topology` handles a failover.
            PushKind::Disconnection if self.cluster.is_none() => {
                self.lost("the shard connection closed");
            }
            _ => {}
        }
    }

    /// Open the connection and subscribe every target in the store. Targets
    /// the server rejects are dropped from the store with an error popup.
    /// Returns the reason when the connection couldn't be opened or dropped
    /// while subscribing.
    async fn connect(&mut self) -> Result<(), String> {
        self.store.lock().status = ConnStatus::Connecting;
        let pubsub = self.open_pubsub().await?;
        self.conn = Some(pubsub.split());

        let targets = self.store.lock().targets();
        let mut failed = Vec::new();
        for target in targets {
            if let Err(err) = self.subscribe(&target).await {
                if is_retryable(&err) {
                    self.conn = None;
                    self.shard = None;
                    return Err(err.to_string());
                }
                failed.push(format!("{target}: {err}"));
                self.store.lock().remove(&target);
            }
        }
        self.store.lock().status = ConnStatus::Connected;
        if !failed.is_empty() {
            self.error(format!("Failed to subscribe:\n{}", failed.join("\n")));
        }
        Ok(())
    }

    /// A reconnect attempt is due.
    async fn reconnect(&mut self) {
        let Some((attempt, _)) = self.retry.take() else {
            return;
        };
        if self.store.lock().subs.is_empty() {
            // Nothing left to listen to: idle until the next subscribe.
            self.store.lock().status = ConnStatus::NotStarted;
            self.lost_targets.clear();
            return;
        }
        match self.connect().await {
            Ok(()) => {
                log::info!("Pub/Sub reconnected after {attempt} attempt(s)");
                let lost = std::mem::take(&mut self.lost_targets);
                self.store.lock().mark_reconnected(&lost, SystemTime::now());
            }
            Err(reason) => {
                log::warn!("Pub/Sub reconnect attempt {attempt} failed: {reason}");
                self.schedule(attempt.saturating_add(1));
            }
        }
    }

    /// Plan reconnect attempt `attempt` after its [`backoff`].
    fn schedule(&mut self, attempt: u32) {
        let next_at = Instant::now() + backoff(attempt);
        self.retry = Some((attempt, next_at));
        self.store.lock().status = ConnStatus::Reconnecting {
            attempt,
            next_at: next_at.into_std(),
        };
    }

    /// The connection is gone: keep subscriptions and buffers, reconnect.
    /// The header shows the state, so there is no popup.
    fn lost(&mut self, reason: &str) {
        self.shard = None;
        if self.conn.take().is_none() {
            return;
        }
        log::warn!("Pub/Sub connection lost: {reason}");
        self.lost_targets = self.store.lock().targets();
        self.schedule(1);
    }

    fn error(&self, msg: String) {
        try_send_action(&self.action_tx, Action::Error(msg));
    }
}

pub(super) fn is_connection_error(err: &redis::RedisError) -> bool {
    err.is_connection_dropped() || err.is_io_error() || err.is_connection_refusal()
}

/// Worth another attempt, with the target kept: the link is down, or a
/// cluster is between masters (`CLUSTERDOWN`, `MOVED`, a timed-out node).
fn is_retryable(err: &redis::RedisError) -> bool {
    is_connection_error(err) || err.is_timeout() || is_topology_error(err)
}

/// A shard connection on `cluster`, its pushes going to the returned receiver.
async fn open_cluster_shard(cluster: &ClusterClient) -> redis::RedisResult<ShardConn> {
    let (tx, pushes) = mpsc::unbounded_channel();
    let config = ClusterConfig::new()
        .set_connection_timeout(CONNECT_TIMEOUT)
        .set_response_timeout(CONNECT_TIMEOUT)
        .set_push_sender(tx);
    let conn = cluster.get_async_connection_with_config(config).await?;
    Ok(ShardConn {
        conn: ShardLink::Cluster(conn),
        pushes,
    })
}

#[cfg(test)]
mod tests {
    use redis::AsyncCommands;
    use tokio::sync::mpsc;

    use super::*;
    use crate::redis_client::{
        pubsub_store::{BufferLimits, EntryKind},
        storage::Storage,
    };

    #[test]
    fn backoff_doubles_up_to_thirty_seconds() {
        let secs: Vec<u64> = (1..=8).map(|a| backoff(a).as_secs()).collect();
        assert_eq!(secs, vec![1, 2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(backoff(0), Duration::from_secs(1));
        assert_eq!(backoff(u32::MAX), MAX_BACKOFF);
    }

    /// Live tests run one at a time: the reconnect test kills every Pub/Sub
    /// client on the server, the other test's included.
    static LIVE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn live_store() -> Arc<Mutex<PubSubStore>> {
        Arc::new(Mutex::new(PubSubStore::new(BufferLimits {
            messages: 1000,
            bytes: 8 * 1024 * 1024,
            payload: 64 * 1024,
        })))
    }

    /// `RROVER_TEST_URL`, or `None` (and a note) so the test skips.
    fn live_url() -> Option<String> {
        let url = std::env::var("RROVER_TEST_URL").ok();
        if url.is_none() {
            eprintln!("skipped: set RROVER_TEST_URL to a Redis server to run this test");
        }
        url
    }

    async fn wait_for(store: &Arc<Mutex<PubSubStore>>, check: impl Fn(&PubSubStore) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !check(&store.lock()) {
            assert!(tokio::time::Instant::now() < deadline, "timed out");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Needs Redis at `RROVER_TEST_URL`: `cargo test -- --ignored pubsub`.
    #[tokio::test]
    #[ignore = "needs a Redis server at RROVER_TEST_URL"]
    async fn pubsub_live_pattern_subscription() {
        let Some(url) = live_url() else {
            return;
        };
        let _live = LIVE.lock().await;
        let client = redis::Client::open(url).unwrap();
        let store = live_store();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (action_tx, mut action_rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run(
            client.clone(),
            None,
            Arc::default(),
            store.clone(),
            cmd_rx,
            action_tx,
            cancel.clone(),
        ));
        assert_eq!(store.lock().status, ConnStatus::NotStarted);

        let target = Target::Pattern("rrover-test-orders:*".into());
        store.lock().add(target.clone());
        cmd_tx.send(PubSubCmd::Subscribe(target.clone())).unwrap();
        wait_for(&store, |s| s.status == ConnStatus::Connected).await;

        let mut con = client.get_multiplexed_async_connection().await.unwrap();
        let receivers: i64 = con
            .publish("rrover-test-orders:new", r#"{"id":1}"#)
            .await
            .unwrap();
        assert_eq!(receivers, 1);
        wait_for(&store, |s| s.subs[0].buffer.len() == 1).await;
        {
            let s = store.lock();
            let msg = s.subs[0].buffer.last_n(1).next().unwrap();
            assert_eq!(msg.channel, "rrover-test-orders:new");
            assert_eq!(msg.seq, 1);
        }

        let mut pipe = redis::pipe();
        for i in 0..2000 {
            pipe.publish("rrover-test-orders:new", i).ignore();
        }
        let () = pipe.query_async(&mut con).await.unwrap();
        wait_for(&store, |s| s.subs[0].stats.total == 2001).await;
        {
            let s = store.lock();
            assert_eq!(s.subs[0].buffer.len(), 1000);
            assert_eq!(s.subs[0].buffer.last_n(1).next().unwrap().seq, 2001);
        }

        // Unsubscribing stops delivery.
        store.lock().remove(&target);
        cmd_tx.send(PubSubCmd::Unsubscribe(target)).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let n: i64 = con.publish("rrover-test-orders:new", "x").await.unwrap();
            if n == 0 {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "still subscribed");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        cancel.cancel();
        handle.await.unwrap();
        assert!(action_rx.try_recv().is_err(), "no error expected");
    }

    /// Needs Redis at `RROVER_TEST_URL`: `cargo test -- --ignored pubsub`.
    /// Kills every Pub/Sub client on that server.
    #[tokio::test]
    #[ignore = "needs a Redis server at RROVER_TEST_URL"]
    async fn pubsub_live_reconnects_and_resubscribes() {
        let Some(url) = live_url() else {
            return;
        };
        let _live = LIVE.lock().await;
        let client = redis::Client::open(url).unwrap();
        let store = live_store();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (action_tx, mut action_rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run(
            client.clone(),
            None,
            Arc::default(),
            store.clone(),
            cmd_rx,
            action_tx,
            cancel.clone(),
        ));

        let channel = Target::Channel("rrover-test-reconnect".into());
        let pattern = Target::Pattern("rrover-test-reconnect*".into());
        for target in [&channel, &pattern] {
            store.lock().add(target.clone());
            cmd_tx.send(PubSubCmd::Subscribe(target.clone())).unwrap();
        }
        wait_for(&store, |s| s.status == ConnStatus::Connected).await;

        let mut con = client.get_multiplexed_async_connection().await.unwrap();
        // Wait until both subscriptions are active on the server.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let n: i64 = con
                .publish("rrover-test-reconnect", "before")
                .await
                .unwrap();
            if n == 2 {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "not subscribed");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Discovery sees our channel, with our connection among its subscribers.
        let manager = crate::redis_client::runner::connect(
            client.clone(),
            crate::redis_client::runner::Link::Interactive,
        )
        .await
        .unwrap();
        let server = Storage::new(manager).pubsub_channels().await.unwrap();
        let (_, subs) = server
            .channels
            .iter()
            .find(|(name, _)| name == "rrover-test-reconnect")
            .expect("channel listed");
        assert!(*subs >= 1);
        assert!(server.patterns >= 1);

        let () = redis::cmd("CLIENT")
            .arg("KILL")
            .arg("TYPE")
            .arg("pubsub")
            .query_async::<i64>(&mut con)
            .await
            .map(|_| ())
            .unwrap();
        wait_for(&store, |s| {
            matches!(s.status, ConnStatus::Reconnecting { attempt: 1, .. })
        })
        .await;
        wait_for(&store, |s| s.status == ConnStatus::Connected).await;
        assert_eq!(store.lock().targets(), vec![channel, pattern]);

        let n: i64 = con.publish("rrover-test-reconnect", "after").await.unwrap();
        assert_eq!(n, 2, "both subscriptions are back");
        wait_for(&store, |s| {
            s.subs.iter().all(|sub| {
                sub.buffer.last_n(1).next().is_some_and(|m| {
                    m.payload == crate::redis_client::pubsub_store::Payload::Text("after".into())
                })
            })
        })
        .await;
        for sub in &store.lock().subs {
            let kinds: Vec<_> = sub.buffer.last_n(2).map(|m| m.kind).collect();
            assert_eq!(
                kinds,
                vec![EntryKind::Reconnected, EntryKind::Message],
                "{}",
                sub.target
            );
            assert!(sub.stats.total >= 2);
        }

        cancel.cancel();
        handle.await.unwrap();
        assert!(action_rx.try_recv().is_err(), "no error expected");
    }
}
