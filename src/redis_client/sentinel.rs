//! A Redis behind Sentinel: which Sentinels to ask, and for which master.

use std::{
    fmt,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use redis::{
    aio::{ConnectionLike, ConnectionManager},
    sentinel::{SentinelClientBuilder, SentinelServerType},
    Cmd, ConnectionAddr, ErrorKind, Pipeline, RedisError, RedisResult, Value,
};
use tokio::sync::{watch, Mutex, Notify};

use super::{
    conn::client_at,
    escape::printable,
    pubsub::is_connection_error,
    runner::{self, Link},
};

/// The port a Sentinel listens on unless the URL says otherwise.
pub const DEFAULT_PORT: u16 = 26379;
/// How long one question to the Sentinels may take once the session runs.
const ASK_TIMEOUT: Duration = Duration::from_secs(2);

/// The Sentinels of a target and the master they are asked about. Its `Debug`
/// never shows the password.
#[derive(Clone, PartialEq, Eq)]
pub struct SentinelTarget {
    pub hosts: Vec<ConnectionAddr>,
    pub master: String,
    /// `AUTH` for the Sentinels themselves; the master's own credentials are
    /// those of the connection.
    password: Option<String>,
}

impl fmt::Debug for SentinelTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SentinelTarget")
            .field("hosts", &self.hosts)
            .field("master", &self.master)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl SentinelTarget {
    #[must_use]
    pub fn new(hosts: Vec<ConnectionAddr>, master: String, password: Option<String>) -> Self {
        Self {
            hosts,
            master,
            password,
        }
    }

    #[must_use]
    pub fn has_password(&self) -> bool {
        self.password.is_some()
    }

    /// `redis+sentinel://h1:26379,h2:26379/mymaster`, without credentials.
    #[must_use]
    pub fn url(&self) -> String {
        let hosts: Vec<String> = self.hosts.iter().map(ToString::to_string).collect();
        format!("redis+sentinel://{}/{}", hosts.join(","), self.master)
    }

    /// What the footer calls this target with its master at `addr`. The
    /// address comes from a server's reply.
    #[must_use]
    pub fn label(&self, addr: &ConnectionAddr) -> String {
        format!(
            "sentinel {} → {}",
            printable(&self.master),
            printable(&addr.to_string())
        )
    }

    /// The master's address now. `SentinelClient` asks each Sentinel and
    /// checks with `ROLE` that the node it names really is a master, using
    /// `base`'s credentials.
    ///
    /// # Errors
    ///
    /// No Sentinel answers, none knows the master, the master refuses the
    /// credentials, or it all takes longer than `within`.
    pub async fn resolve(
        &self,
        base: &redis::Client,
        within: Duration,
    ) -> RedisResult<(String, u16)> {
        let settings = base.get_connection_info().redis_settings();
        let mut builder = SentinelClientBuilder::new(
            self.hosts.clone(),
            &self.master,
            SentinelServerType::Master,
        )?
        .set_client_to_redis_db(settings.db());
        if let Some(username) = settings.username() {
            builder = builder.set_client_to_redis_username(username);
        }
        if let Some(password) = settings.password() {
            builder = builder.set_client_to_redis_password(password);
        }
        if let Some(password) = &self.password {
            builder = builder.set_client_to_sentinel_password(password);
        }
        let mut sentinel = builder.build()?;
        let client = tokio::time::timeout(within, sentinel.async_get_client())
            .await
            .map_err(|_| RedisError::from((ErrorKind::Io, "asking Sentinel timed out")))??;
        match client.get_connection_info().addr() {
            ConnectionAddr::Tcp(host, port) | ConnectionAddr::TcpTls { host, port, .. } => {
                Ok((host.clone(), *port))
            }
            _ => Err(RedisError::from((
                ErrorKind::InvalidClientConfig,
                "Sentinel named a master without a TCP address",
            ))),
        }
    }
}

/// The session's current master, shared by every connection. The Sentinel
/// task moves it; connections reopen on the new address.
#[derive(Clone)]
pub struct MasterRoute {
    target: Arc<SentinelTarget>,
    master: Arc<watch::Sender<redis::Client>>,
    generation: Arc<AtomicU64>,
    lost: Arc<Notify>,
}

impl MasterRoute {
    #[must_use]
    pub fn new(target: SentinelTarget, master: redis::Client) -> Self {
        Self {
            target: Arc::new(target),
            master: Arc::new(watch::Sender::new(master)),
            generation: Arc::new(AtomicU64::new(0)),
            lost: Arc::new(Notify::new()),
        }
    }

    /// A client on the current master.
    #[must_use]
    pub fn current(&self) -> redis::Client {
        self.master.borrow().clone()
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// A connection failed: ask the Sentinels now, not at the next check.
    fn report_lost(&self) {
        self.lost.notify_one();
    }

    /// Resolves when a connection reported a loss.
    pub async fn lost(&self) {
        self.lost.notified().await;
    }

    /// Ask the Sentinels. Returns the new footer label if the master moved.
    /// While they can't name one (mid-failover) nothing changes.
    pub async fn refresh(&self) -> Option<String> {
        let current = self.current();
        let (host, port) = match self.target.resolve(&current, ASK_TIMEOUT).await {
            Ok(address) => address,
            Err(err) => {
                log::debug!("Sentinel has no master to offer yet: {err}");
                return None;
            }
        };
        if same_address(current.get_connection_info().addr(), &host, port) {
            return None;
        }
        let moved = match client_at(&current, &host, port) {
            Ok(client) => client,
            Err(err) => {
                log::warn!("Sentinel named an unusable master: {err}");
                return None;
            }
        };
        let label = self.target.label(moved.get_connection_info().addr());
        log::info!("Sentinel moved the master: {label}");
        self.master.send_replace(moved);
        self.generation.fetch_add(1, Ordering::AcqRel);
        Some(label)
    }
}

fn same_address(addr: &ConnectionAddr, host: &str, port: u16) -> bool {
    match addr {
        ConnectionAddr::Tcp(h, p)
        | ConnectionAddr::TcpTls {
            host: h, port: p, ..
        } => h == host && *p == port,
        _ => false,
    }
}

/// What a long-lived task (Pub/Sub, MONITOR, live) connects with: a fixed
/// client, or the master Sentinel names at the moment.
#[derive(Clone)]
pub struct ServerClient {
    client: redis::Client,
    changes: Option<watch::Receiver<redis::Client>>,
}

impl From<redis::Client> for ServerClient {
    fn from(client: redis::Client) -> Self {
        Self {
            client,
            changes: None,
        }
    }
}

impl From<&MasterRoute> for ServerClient {
    fn from(route: &MasterRoute) -> Self {
        Self {
            client: route.current(),
            changes: Some(route.master.subscribe()),
        }
    }
}

impl ServerClient {
    /// A client on the server to use now.
    #[must_use]
    pub fn current(&self) -> redis::Client {
        self.changes
            .as_ref()
            .map_or_else(|| self.client.clone(), |rx| rx.borrow().clone())
    }

    /// Resolves when the master moved; never without Sentinel.
    pub async fn changed(&mut self) {
        let Some(rx) = self.changes.as_mut() else {
            return std::future::pending().await;
        };
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// A connection that follows the master: it opens a new manager once the
/// route moved, since a `ConnectionManager` only ever reconnects to the
/// address it was built with.
#[derive(Clone)]
pub struct FollowConn {
    route: MasterRoute,
    db: i64,
    link: Link,
    slot: Arc<Mutex<(u64, ConnectionManager)>>,
}

impl FollowConn {
    /// # Errors
    ///
    /// The current master can't be reached.
    pub async fn open(
        route: MasterRoute,
        db: i64,
        link: Link,
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        let generation = route.generation();
        let manager = runner::connect_manager(on_db(&route.current(), db)?, link).await?;
        Ok(Self {
            route,
            db,
            link,
            slot: Arc::new(Mutex::new((generation, manager))),
        })
    }

    #[must_use]
    pub fn db(&self) -> i64 {
        self.db
    }

    /// The manager on the current master. A failed reopen is tried again by
    /// the next command.
    async fn manager(&self) -> RedisResult<ConnectionManager> {
        let mut slot = self.slot.lock().await;
        let generation = self.route.generation();
        if slot.0 != generation {
            let client = on_db(&self.route.current(), self.db)?;
            let manager = runner::connect_manager(client, self.link)
                .await
                .map_err(|err| {
                    log::warn!("Connecting to the new master failed: {err}");
                    RedisError::from((ErrorKind::Io, "connecting to the new master failed"))
                })?;
            *slot = (generation, manager);
        }
        Ok(slot.1.clone())
    }

    fn note<T>(&self, reply: &RedisResult<T>) {
        if let Err(err) = reply {
            // `READONLY`: this node was demoted while the connection stayed up.
            if is_connection_error(err) || err.is_timeout() || err.code() == Some("READONLY") {
                self.route.report_lost();
            }
        }
    }

    pub(super) async fn command(&self, cmd: &Cmd) -> RedisResult<Value> {
        let reply = match self.manager().await {
            Ok(mut manager) => manager.req_packed_command(cmd).await,
            Err(err) => Err(err),
        };
        self.note(&reply);
        reply
    }

    pub(super) async fn pipeline(
        &self,
        cmd: &Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisResult<Vec<Value>> {
        let reply = match self.manager().await {
            Ok(mut manager) => manager.req_packed_commands(cmd, offset, count).await,
            Err(err) => Err(err),
        };
        self.note(&reply);
        reply
    }
}

fn on_db(client: &redis::Client, db: i64) -> RedisResult<redis::Client> {
    let info = client.get_connection_info().clone();
    let settings = info.redis_settings().clone().set_db(db);
    redis::Client::open(info.set_redis_settings(settings))
}

#[cfg(test)]
mod tests {
    use redis::ConnectionAddr;

    use super::{same_address, SentinelTarget};

    #[test]
    fn an_address_is_compared_by_host_and_port() {
        let addr = ConnectionAddr::Tcp("10.0.0.1".into(), 6379);
        assert!(same_address(&addr, "10.0.0.1", 6379));
        assert!(!same_address(&addr, "10.0.0.2", 6379));
        assert!(!same_address(&addr, "10.0.0.1", 6380));
    }

    #[test]
    fn the_label_escapes_what_a_server_sent() {
        let target = SentinelTarget::new(Vec::new(), "mymaster".into(), None);
        let addr = ConnectionAddr::Tcp("evil\u{1b}[2J".into(), 6379);
        let label = target.label(&addr);
        assert!(label.starts_with("sentinel mymaster → "));
        assert!(!label.contains('\u{1b}'), "{label:?}");
    }

    #[test]
    fn debug_and_url_never_show_the_password() {
        let target = SentinelTarget::new(
            vec![ConnectionAddr::Tcp("10.0.0.1".into(), 26379)],
            "mymaster".into(),
            Some("hunter2".into()),
        );
        assert!(!format!("{target:?}").contains("hunter2"));
        assert_eq!(target.url(), "redis+sentinel://10.0.0.1:26379/mymaster");
    }
}
