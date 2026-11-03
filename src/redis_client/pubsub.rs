//! Background task owning the dedicated Pub/Sub connection.
//!
//! A subscribed connection can't run other commands, so it is separate from
//! the shared `ConnectionManager`. Messages go straight into the
//! [`PubSubStore`]; the UI reads it on render, so there is no `Action` per
//! message.

use std::{sync::Arc, time::Duration, time::SystemTime};

use futures::StreamExt;
use parking_lot::Mutex;
use redis::aio::{PubSubSink, PubSubStream};
use tokio::sync::mpsc::{Sender, UnboundedReceiver};
use tokio_util::sync::CancellationToken;

use crate::action::{try_send_action, Action};

use super::pubsub_store::{ConnStatus, PubSubStore, Target};

/// How long opening the Pub/Sub connection may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Control messages from the App. The store already reflects them when they
/// are sent: `Subscribe` follows `PubSubStore::add`, `Unsubscribe` follows
/// `PubSubStore::remove`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PubSubCmd {
    Subscribe(Target),
    Unsubscribe(Target),
}

/// Run the Pub/Sub task until `cancel` fires or the command channel closes.
///
/// The connection opens on the first `Subscribe`, so a session that never
/// subscribes costs nothing. When it drops, the status goes to
/// `Disconnected` and the next `Subscribe` reconnects and subscribes every
/// target in the store again.
pub async fn run(
    client: redis::Client,
    store: Arc<Mutex<PubSubStore>>,
    mut cmd_rx: UnboundedReceiver<PubSubCmd>,
    action_tx: Sender<Action>,
    cancel: CancellationToken,
) {
    let mut task = PubSubTask {
        client,
        store,
        action_tx,
        conn: None,
    };
    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(cmd) => task.apply(cmd).await,
                None => break,
            },
            msg = next_msg(task.conn.as_mut()) => match msg {
                Some(msg) => task.store.lock().push_msg(&msg, SystemTime::now()),
                None => task.lost("the server closed the connection"),
            },
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

struct PubSubTask {
    client: redis::Client,
    store: Arc<Mutex<PubSubStore>>,
    action_tx: Sender<Action>,
    conn: Option<(PubSubSink, PubSubStream)>,
}

impl PubSubTask {
    async fn apply(&mut self, cmd: PubSubCmd) {
        match cmd {
            PubSubCmd::Subscribe(target) => {
                if self.conn.is_none() {
                    // Connecting subscribes everything in the store, this
                    // target included.
                    self.connect().await;
                    return;
                }
                if let Err(err) = self.subscribe(&target).await {
                    self.store.lock().remove(&target);
                    self.error(format!("Failed to subscribe to {target}: {err}"));
                    if is_connection_error(&err) {
                        self.lost(&err.to_string());
                    }
                }
            }
            PubSubCmd::Unsubscribe(target) => {
                let Some((sink, _)) = self.conn.as_mut() else {
                    return;
                };
                let res = match &target {
                    Target::Channel(name) => sink.unsubscribe(name).await,
                    Target::Pattern(name) => sink.punsubscribe(name).await,
                };
                if let Err(err) = res {
                    log::warn!("Unsubscribe from {target} failed: {err}");
                    if is_connection_error(&err) {
                        self.lost(&err.to_string());
                    }
                }
            }
        }
    }

    async fn subscribe(&mut self, target: &Target) -> redis::RedisResult<()> {
        let Some((sink, _)) = self.conn.as_mut() else {
            return Ok(());
        };
        match target {
            Target::Channel(name) => sink.subscribe(name).await,
            Target::Pattern(name) => sink.psubscribe(name).await,
        }
    }

    /// Open the connection and subscribe every target in the store. Targets
    /// that fail are dropped from the store with an error popup.
    async fn connect(&mut self) {
        self.store.lock().status = ConnStatus::Connecting;
        let opened = tokio::time::timeout(CONNECT_TIMEOUT, self.client.get_async_pubsub()).await;
        let pubsub = match opened {
            Ok(Ok(pubsub)) => pubsub,
            Ok(Err(err)) => return self.connect_failed(&err.to_string()),
            Err(_) => return self.connect_failed(&format!("timed out after {CONNECT_TIMEOUT:?}")),
        };
        self.conn = Some(pubsub.split());

        let targets = self.store.lock().targets();
        let mut failed = Vec::new();
        for target in targets {
            if let Err(err) = self.subscribe(&target).await {
                if is_connection_error(&err) {
                    self.lost(&err.to_string());
                    return;
                }
                failed.push(format!("{target}: {err}"));
                self.store.lock().remove(&target);
            }
        }
        self.store.lock().status = ConnStatus::Connected;
        if !failed.is_empty() {
            self.error(format!("Failed to subscribe:\n{}", failed.join("\n")));
        }
    }

    fn connect_failed(&mut self, reason: &str) {
        log::error!("Pub/Sub connect failed: {reason}");
        self.store.lock().status = ConnStatus::Disconnected(reason.to_owned());
        self.error(format!("Pub/Sub connection failed: {reason}"));
    }

    /// The connection is gone: keep subscriptions and buffers, report once.
    fn lost(&mut self, reason: &str) {
        if self.conn.take().is_none() {
            return;
        }
        log::error!("Pub/Sub connection lost: {reason}");
        self.store.lock().status = ConnStatus::Disconnected(reason.to_owned());
        self.error(format!(
            "Pub/Sub connection lost: {reason}\nAdd a subscription to reconnect."
        ));
    }

    fn error(&self, msg: String) {
        try_send_action(&self.action_tx, Action::Error(msg));
    }
}

fn is_connection_error(err: &redis::RedisError) -> bool {
    err.is_connection_dropped() || err.is_io_error() || err.is_connection_refusal()
}

#[cfg(test)]
mod tests {
    use redis::AsyncCommands;
    use tokio::sync::mpsc;

    use super::*;
    use crate::redis_client::pubsub_store::BufferLimits;

    async fn wait_for(store: &Arc<Mutex<PubSubStore>>, check: impl Fn(&PubSubStore) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !check(&store.lock()) {
            assert!(tokio::time::Instant::now() < deadline, "timed out");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Needs Redis on localhost:6379: `cargo test -- --ignored pubsub`.
    #[tokio::test]
    #[ignore = "needs a Redis server on localhost:6379"]
    async fn pubsub_live_pattern_subscription() {
        let client = redis::Client::open("redis://localhost:6379").unwrap();
        let store = Arc::new(Mutex::new(PubSubStore::new(BufferLimits {
            messages: 1000,
            bytes: 8 * 1024 * 1024,
            payload: 64 * 1024,
        })));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (action_tx, mut action_rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run(
            client.clone(),
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
}
