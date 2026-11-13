//! Helpers for the integration suite: Redis in Docker, seed data, and a
//! harness that drives the background runner through its channels the way
//! `App` does.
//!
//! Every test that uses this is `#[ignore]`d, so plain `cargo test` never
//! needs Docker. Run them with `cargo test -- --ignored --test-threads=1`.

// Each test binary uses only part of this module.
#![allow(dead_code)]

use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Once,
    },
    time::Duration,
};

use redis::{aio::ConnectionManager, AsyncCommands};
use redis_rover::internals::{
    Action, PubSubCmd, RedisEvent, Runner, SharedState, VALUE_FETCH_DELAY_MS,
};
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
    ContainerAsync, GenericImage, ImageExt,
};
use testcontainers_modules::redis::Redis;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

/// Image every test runs against.
pub const REDIS_TAG: &str = "7-alpine";
/// Redis with modules (`RedisJSON` 2.x), for the JSON tests.
pub const REDIS_STACK_IMAGE: &str = "redis/redis-stack-server";
pub const REDIS_STACK_TAG: &str = "7.4.0-v3";
/// How long a test waits for an expected action.
pub const WAIT: Duration = Duration::from_secs(10);

/// Same capacity as the app's action channel (`lib.rs`), so a test sees the
/// same back-pressure the app does.
const ACTION_CHANNEL_CAPACITY: usize = 256;

static SETUP: Once = Once::new();

/// Process-wide setup, once: the bundled config instead of the user's, and
/// rustls' crypto provider (as `redis_rover::run` installs it).
pub fn init() {
    SETUP.call_once(|| {
        let empty = std::env::temp_dir().join("rrover-test-no-config");
        std::env::set_var("REDIS_ROVER_CONFIG", &empty);
        std::env::set_var("RROVER_CONFIG", &empty);
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// The container behind a [`Server`]; stopped and removed on drop.
enum Container {
    Redis(ContainerAsync<Redis>),
    Stack(ContainerAsync<GenericImage>),
}

/// A running Redis container. Stopped and removed on drop.
pub struct Server {
    _container: Container,
    pub host: String,
    pub port: u16,
}

impl Server {
    /// URL of logical database `db`.
    pub fn url(&self, db: i64) -> String {
        format!("redis://{}:{}/{db}", self.host, self.port)
    }

    pub fn client(&self, db: i64) -> redis::Client {
        redis::Client::open(self.url(db)).expect("valid redis URL")
    }

    /// A fresh connection, outside the runner, for seeding and checking.
    pub async fn admin(&self, db: i64) -> ConnectionManager {
        ConnectionManager::new(self.client(db))
            .await
            .expect("admin connection")
    }
}

/// Start Redis and wait until it answers `PING` (the log line the image
/// waits for can come slightly before the port is reachable).
pub async fn start_redis() -> Server {
    start_redis_tag(REDIS_TAG).await
}

/// Like [`start_redis`], on another `redis` image tag (`5-alpine`).
pub async fn start_redis_tag(tag: &str) -> Server {
    init();
    let container = Redis::default()
        .with_tag(tag)
        .start()
        .await
        .expect("start redis container (is Docker running?)");
    let host = container
        .get_host()
        .await
        .expect("container host")
        .to_string();
    let port = container
        .get_host_port_ipv4(6379)
        .await
        .expect("container port");
    serve(Container::Redis(container), host, port).await
}

/// Start `redis/redis-stack-server` (`RedisJSON` 2.x and the other modules).
pub async fn start_redis_stack() -> Server {
    init();
    let container = GenericImage::new(REDIS_STACK_IMAGE, REDIS_STACK_TAG)
        .with_exposed_port(6379.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"))
        .start()
        .await
        .expect("start redis-stack container (is Docker running?)");
    let host = container
        .get_host()
        .await
        .expect("container host")
        .to_string();
    let port = container
        .get_host_port_ipv4(6379)
        .await
        .expect("container port");
    serve(Container::Stack(container), host, port).await
}

async fn serve(container: Container, host: String, port: u16) -> Server {
    let server = Server {
        _container: container,
        host,
        port,
    };
    wait_for_ping(&server.client(0)).await;
    server
}

/// Retry `PING` until it answers `PONG`, for up to `WAIT`.
pub async fn wait_for_ping(client: &redis::Client) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let reply = async {
            let mut con = client.get_multiplexed_async_connection().await?;
            redis::cmd("PING").query_async::<String>(&mut con).await
        }
        .await;
        match reply {
            Ok(pong) if pong == "PONG" => return,
            other => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "Redis never answered PING: {other:?}"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Slows every value fetch by `ms` until dropped (the runner's test hook), so
/// a test can keep one fetch in flight without stalling the server.
pub struct ValueDelay;

impl ValueDelay {
    pub fn set(ms: u64) -> Self {
        VALUE_FETCH_DELAY_MS.store(ms, Ordering::Relaxed);
        Self
    }
}

impl Drop for ValueDelay {
    fn drop(&mut self) {
        VALUE_FETCH_DELAY_MS.store(0, Ordering::Relaxed);
    }
}

/// A key prefix no other test uses, e.g. `t3-scan:`.
pub fn prefix(test: &str) -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    format!("t{}-{test}:", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// Keys written by [`seed`], by type.
#[derive(Debug, Default)]
pub struct Seeded {
    pub strings: Vec<String>,
    pub hashes: Vec<String>,
    pub lists: Vec<String>,
    pub sets: Vec<String>,
    pub zsets: Vec<String>,
    pub stream: String,
}

impl Seeded {
    pub fn all(&self) -> HashSet<String> {
        self.strings
            .iter()
            .chain(&self.hashes)
            .chain(&self.lists)
            .chain(&self.sets)
            .chain(&self.zsets)
            .chain(std::iter::once(&self.stream))
            .cloned()
            .collect()
    }
}

/// Write `per_type` keys of each type (string, hash, list, set, zset) plus one
/// stream under `prefix`, in one pipeline. Every other key gets a 1 h TTL.
/// Collections hold 3 elements: list `a b c`, set `a b c`, hash `f1..f3 =
/// v1..v3`, zset `a:1 b:2 c:3`.
pub async fn seed(con: &mut ConnectionManager, prefix: &str, per_type: usize) -> Seeded {
    let mut seeded = Seeded {
        stream: format!("{prefix}stream"),
        ..Seeded::default()
    };
    let mut pipe = redis::pipe();
    for i in 0..per_type {
        let name = |kind: &str| format!("{prefix}{kind}:{i}");
        let (string, hash, list, set, zset) = (
            name("str"),
            name("hash"),
            name("list"),
            name("set"),
            name("zset"),
        );
        pipe.set(&string, format!("value {i}")).ignore();
        pipe.hset_multiple(&hash, &[("f1", "v1"), ("f2", "v2"), ("f3", "v3")])
            .ignore();
        pipe.rpush(&list, &["a", "b", "c"]).ignore();
        pipe.sadd(&set, &["a", "b", "c"]).ignore();
        pipe.zadd_multiple(&zset, &[(1.0, "a"), (2.0, "b"), (3.0, "c")])
            .ignore();
        if i % 2 == 1 {
            for key in [&string, &hash, &list, &set, &zset] {
                pipe.expire(key, 3600).ignore();
            }
        }
        seeded.strings.push(string);
        seeded.hashes.push(hash);
        seeded.lists.push(list);
        seeded.sets.push(set);
        seeded.zsets.push(zset);
    }
    pipe.cmd("XADD")
        .arg(&seeded.stream)
        .arg("*")
        .arg("field")
        .arg("value")
        .ignore();
    let () = pipe.query_async(con).await.expect("seed pipeline");
    seeded
}

/// `count` plain string keys named `{prefix}{i}`, in one pipeline.
pub async fn seed_strings(con: &mut ConnectionManager, prefix: &str, count: usize) -> Vec<String> {
    let keys: Vec<String> = (0..count).map(|i| format!("{prefix}{i}")).collect();
    let mut pipe = redis::pipe();
    for key in &keys {
        pipe.set(key, "v").ignore();
    }
    let () = pipe.query_async(con).await.expect("seed pipeline");
    keys
}

/// Everything a test needs to read back: `TYPE`, `DUMP` and `PTTL` of each
/// key, plus `DBSIZE`. Two equal fingerprints mean the data didn't change.
pub async fn fingerprint(con: &mut ConnectionManager, keys: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(keys.len() + 1);
    let size: u64 = redis::cmd("DBSIZE").query_async(con).await.expect("DBSIZE");
    out.push(format!("dbsize={size}"));
    for key in keys {
        let kind: String = redis::cmd("TYPE")
            .arg(key)
            .query_async(con)
            .await
            .expect("TYPE");
        let dump: Option<Vec<u8>> = redis::cmd("DUMP")
            .arg(key)
            .query_async(con)
            .await
            .expect("DUMP");
        // PTTL drifts while the test runs; only "has a TTL or not" is stable.
        let ttl: i64 = con.pttl(key).await.expect("PTTL");
        out.push(format!("{key} {kind} {dump:?} ttl={}", ttl > 0));
    }
    out
}

/// The background runner, wired up like `redis_rover::run` wires it for `App`.
pub struct Harness {
    pub state: SharedState,
    events: broadcast::Sender<RedisEvent>,
    pubsub: mpsc::UnboundedSender<PubSubCmd>,
    actions: mpsc::Receiver<Action>,
    runner: Runner,
    cancel: CancellationToken,
}

impl Harness {
    pub async fn new(client: redis::Client, read_only: bool) -> Self {
        Self::try_new(client, read_only)
            .await
            .expect("connect the runner")
    }

    /// Like [`Harness::new`], but returns the connection error instead of
    /// panicking (the TLS test checks it).
    pub async fn try_new(client: redis::Client, read_only: bool) -> redis::RedisResult<Self> {
        init();
        let manager = ConnectionManager::new(client.clone()).await?;
        let state = SharedState::default();
        *state.db.lock() = client.get_connection_info().redis_settings().db();
        let (tx, actions) = mpsc::channel(ACTION_CHANNEL_CAPACITY);
        let cancel = CancellationToken::new();
        let mut runner = Runner::new(client, manager, state.clone(), tx)
            .cancellation_token(cancel.clone())
            .read_only(read_only);
        let events = runner.tx();
        let pubsub = runner.pubsub_tx();
        runner.start();
        Ok(Self {
            state,
            events,
            pubsub,
            actions,
            runner,
            cancel,
        })
    }

    /// Send a `RedisEvent`, as `App` does through its broadcast sender.
    /// The runner's read-only flag, as `App` shares it (the `R` toggle).
    pub fn read_only_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.runner.read_only_flag()
    }

    pub fn send(&self, event: RedisEvent) {
        self.events.send(event).expect("runner is listening");
    }

    pub fn pubsub(&self, cmd: PubSubCmd) {
        self.pubsub.send(cmd).expect("Pub/Sub task is listening");
    }

    /// Wait for the first action that `matches`, skipping (and remembering)
    /// the others. Panics with every action seen if none comes within `WAIT`.
    pub async fn expect(&mut self, what: &str, matches: impl Fn(&Action) -> bool) -> Action {
        let deadline = tokio::time::Instant::now() + WAIT;
        let mut seen = Vec::new();
        loop {
            match tokio::time::timeout_at(deadline, self.actions.recv()).await {
                Ok(Some(action)) if matches(&action) => return action,
                Ok(Some(action)) => seen.push(action),
                Ok(None) => panic!("action channel closed while waiting for {what}"),
                Err(elapsed) => panic!("{elapsed} waiting for {what}; saw {seen:?}"),
            }
        }
    }

    /// Every action that arrives within `window`.
    pub async fn drain(&mut self, window: Duration) -> Vec<Action> {
        let deadline = tokio::time::Instant::now() + window;
        let mut seen = Vec::new();
        while let Ok(Some(action)) = tokio::time::timeout_at(deadline, self.actions.recv()).await {
            seen.push(action);
        }
        seen
    }

    /// `FetchKeys` for the current page; returns the page's key names once
    /// `LoadKeysIntoKeySpace` arrives. Panics on `Error` / `KeysLoadAborted`.
    pub async fn fetch_page(&mut self) -> Vec<String> {
        self.send(RedisEvent::FetchKeys);
        let action = self
            .expect("keys or a failure", |a| {
                matches!(
                    a,
                    Action::LoadKeysIntoKeySpace | Action::KeysLoadAborted | Action::Error(_)
                )
            })
            .await;
        assert_eq!(action, Action::LoadKeysIntoKeySpace, "FetchKeys failed");
        self.state
            .keys
            .lock()
            .as_ref()
            .expect("LoadKeysIntoKeySpace leaves a page in state.keys")
            .iter()
            .map(|k| k.key.clone())
            .collect()
    }

    /// Page through the whole keyspace from page 1 (100 keys per page) and
    /// return every key in the order shown. Asserts the pages join up: each
    /// starts where the previous one ended, with no gaps or overlaps.
    pub async fn fetch_all_pages(&mut self) -> Vec<String> {
        {
            let mut ks = self.state.keyspace_state.lock();
            ks.set_page_size(100, 0);
            ks.page = 0;
            ks.rescan();
        }
        let mut shown = Vec::new();
        let mut expected_page = 0;
        loop {
            let keys = self.fetch_page().await;
            let mut ks = self.state.keyspace_state.lock();
            if ks.page < expected_page {
                // The sweep ended exactly on a page boundary, so the empty
                // page was clamped back to the last full one.
                assert!(ks.sweep_done, "page moved back but the sweep isn't done");
                break;
            }
            let range = ks.page_range();
            assert_eq!(
                range.start,
                shown.len(),
                "page {expected_page}: gap or overlap"
            );
            assert_eq!(
                keys.len(),
                range.len(),
                "page {expected_page}: rows vs range"
            );
            assert_eq!(keys, ks.page_keys(), "page shows the sweep's keys");
            shown.extend(keys);
            if !ks.next_page() {
                break;
            }
            expected_page += 1;
        }
        shown
    }

    pub async fn shutdown(self) {
        self.cancel.cancel();
        self.runner.shutdown().await;
    }
}
