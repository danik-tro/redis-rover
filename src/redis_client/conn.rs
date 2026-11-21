//! The connection the rest of the app talks through: one server or a whole
//! cluster. An enum rather than a generic, so `Storage` and the `client`
//! helpers stay connection-agnostic without a type parameter on each of them.

use redis::{
    aio::{ConnectionLike, ConnectionManager},
    cluster::ClusterClient,
    cluster_async::ClusterConnection,
    cluster_routing::{Route, RoutingInfo, SingleNodeRoutingInfo, SlotAddr},
    Arg, Cmd, ConnectionAddr, Pipeline, RedisFuture, Value,
};

use super::{
    runner::{self, Link},
    sentinel::{FollowConn, MasterRoute, ServerClient},
};

type ConnectError = Box<dyn std::error::Error + Sync + Send>;

/// A cheap-to-clone handle on a Redis connection. Open one with
/// [`Connector::open`] or [`runner::connect`], which apply the configured
/// timeouts.
#[derive(Clone)]
pub enum Conn {
    /// One server, reconnected by the manager.
    Single(ConnectionManager),
    /// A cluster: commands are routed by slot.
    Cluster(ClusterConnection),
    /// A cluster handle whose commands and pipelines all go to one node.
    /// A node runs a pipeline over several of its slots, which the slot
    /// routing of [`Conn::Cluster`] refuses as `CROSSSLOT`.
    Node(ClusterConnection, SingleNodeRoutingInfo),
    /// One server behind Sentinel: follows the master through a failover.
    Follow(FollowConn),
}

impl Conn {
    /// Whether commands are routed over a cluster's nodes.
    #[must_use]
    pub fn is_cluster(&self) -> bool {
        matches!(self, Self::Cluster(_) | Self::Node(..))
    }

    /// This connection, with everything sent to the home node (see
    /// [`home_route`]). One server is its own home.
    #[must_use]
    pub fn home(&self) -> Self {
        match self {
            Self::Cluster(conn) => Self::Node(conn.clone(), home_node()),
            other => other.clone(),
        }
    }

    /// This connection, with everything sent to the node at `host:port`.
    /// For commands without a key (`SCAN`, `DBSIZE`, `INFO`): routed by slot
    /// they would silently reach another node while the client's slot map
    /// is stale, since no `MOVED` corrects them. One server is every node.
    #[must_use]
    pub fn on_address(&self, host: &str, port: u16) -> Self {
        match self {
            Self::Cluster(conn) | Self::Node(conn, _) => Self::Node(
                conn.clone(),
                SingleNodeRoutingInfo::ByAddress {
                    host: host.to_owned(),
                    port,
                },
            ),
            single @ (Self::Single(_) | Self::Follow(_)) => single.clone(),
        }
    }

    /// This connection, with everything sent to the master of `slot`. One
    /// server serves every slot.
    #[must_use]
    pub fn on_slot(&self, slot: u16) -> Self {
        match self {
            Self::Cluster(conn) | Self::Node(conn, _) => Self::Node(
                conn.clone(),
                SingleNodeRoutingInfo::SpecificNode(Route::new(slot, SlotAddr::Master)),
            ),
            single @ (Self::Single(_) | Self::Follow(_)) => single.clone(),
        }
    }
}

impl From<ConnectionManager> for Conn {
    fn from(manager: ConnectionManager) -> Self {
        Self::Single(manager)
    }
}

impl From<ClusterConnection> for Conn {
    fn from(connection: ClusterConnection) -> Self {
        Self::Cluster(connection)
    }
}

/// Commands about one node rather than a key. The cluster client would send
/// them to a random node (`SCAN`, whose cursor only means something on the
/// node that issued it) or to all of them with a merged reply (`INFO`,
/// `DBSIZE`), so they go to [`home_route`] unless a caller picks a node.
fn is_node_command(cmd: &Cmd) -> bool {
    const NODE_COMMANDS: [&[u8]; 9] = [
        b"SCAN", b"INFO", b"DBSIZE", b"CONFIG", b"SLOWLOG", b"PUBSUB", b"CLUSTER", b"PING", b"ROLE",
    ];
    match cmd.args_iter().next() {
        Some(Arg::Simple(name)) => NODE_COMMANDS
            .iter()
            .any(|known| name.eq_ignore_ascii_case(known)),
        _ => false,
    }
}

/// The master of slot 0: always one master, and the client follows it
/// through a failover, which a fixed address would not.
fn home_route() -> RoutingInfo {
    RoutingInfo::SingleNode(home_node())
}

fn home_node() -> SingleNodeRoutingInfo {
    SingleNodeRoutingInfo::SpecificNode(Route::new(0, SlotAddr::Master))
}

impl ConnectionLike for Conn {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
        match self {
            Self::Single(conn) => conn.req_packed_command(cmd),
            Self::Cluster(conn) => {
                if is_node_command(cmd) {
                    Box::pin(conn.route_command(cmd.clone(), home_route()))
                } else {
                    conn.req_packed_command(cmd)
                }
            }
            Self::Node(conn, node) => {
                Box::pin(conn.route_command(cmd.clone(), RoutingInfo::SingleNode(node.clone())))
            }
            Self::Follow(conn) => Box::pin(conn.command(cmd)),
        }
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        cmd: &'a Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<Value>> {
        match self {
            Self::Single(conn) => conn.req_packed_commands(cmd, offset, count),
            Self::Cluster(conn) => conn.req_packed_commands(cmd, offset, count),
            Self::Node(conn, node) => {
                Box::pin(conn.route_pipeline(cmd.clone(), offset, count, node.clone()))
            }
            Self::Follow(conn) => Box::pin(conn.pipeline(cmd, offset, count)),
        }
    }

    fn get_db(&self) -> i64 {
        match self {
            Self::Single(conn) => conn.get_db(),
            Self::Cluster(conn) | Self::Node(conn, _) => conn.get_db(),
            Self::Follow(conn) => conn.db(),
        }
    }
}

/// The cluster clients of a session, built from the same seeds.
#[derive(Clone)]
pub struct ClusterClients {
    pub plain: ClusterClient,
    /// RESP3, for sharded Pub/Sub: its messages arrive as pushes.
    pub resp3: ClusterClient,
}

/// Opens the session's connections: to one server (on any DB) or to the
/// cluster (DB 0 only).
#[derive(Clone)]
pub struct Connector {
    client: redis::Client,
    cluster: Option<ClusterClients>,
    route: Option<MasterRoute>,
}

impl Connector {
    #[must_use]
    pub fn single(client: redis::Client) -> Self {
        Self {
            client,
            cluster: None,
            route: None,
        }
    }

    #[must_use]
    pub fn cluster(client: redis::Client, cluster: ClusterClients) -> Self {
        Self {
            client,
            cluster: Some(cluster),
            route: None,
        }
    }

    /// One server behind Sentinel: every connection follows `route`.
    #[must_use]
    pub fn sentinel(client: redis::Client, route: MasterRoute) -> Self {
        Self {
            client,
            cluster: None,
            route: Some(route),
        }
    }

    /// The master route of a Sentinel session.
    #[must_use]
    pub fn route(&self) -> Option<&MasterRoute> {
        self.route.as_ref()
    }

    /// What a long-lived task connects with.
    #[must_use]
    pub fn server_client(&self) -> ServerClient {
        match &self.route {
            Some(route) => route.into(),
            None => self.client.clone().into(),
        }
    }

    /// The RESP3 cluster client; `None` on one server.
    #[must_use]
    pub fn shard_client(&self) -> Option<ClusterClient> {
        self.cluster.as_ref().map(|cluster| cluster.resp3.clone())
    }

    /// A new connection for `link`. `db` is ignored on a cluster.
    ///
    /// # Errors
    ///
    /// The server can't be reached or the attempts time out.
    pub async fn open(&self, db: i64, link: Link) -> Result<Conn, ConnectError> {
        if let Some(route) = &self.route {
            return Ok(Conn::Follow(
                FollowConn::open(route.clone(), db, link).await?,
            ));
        }
        if let Some(cluster) = &self.cluster {
            return runner::connect_cluster(&cluster.plain, link).await;
        }
        let info = self.client.get_connection_info().clone();
        let settings = info.redis_settings().clone().set_db(db);
        let client = redis::Client::open(info.set_redis_settings(settings))?;
        runner::connect(client, link).await
    }
}

/// A client for the node at `host:port`, with `client`'s credentials and
/// TLS settings.
///
/// # Errors
///
/// The settings don't make a valid client.
pub fn client_at(
    client: &redis::Client,
    host: &str,
    port: u16,
) -> redis::RedisResult<redis::Client> {
    let info = client.get_connection_info().clone();
    let addr = match info.addr().clone() {
        ConnectionAddr::TcpTls {
            insecure,
            tls_params,
            ..
        } => ConnectionAddr::TcpTls {
            host: host.to_owned(),
            port,
            insecure,
            tls_params,
        },
        _ => ConnectionAddr::Tcp(host.to_owned(), port),
    };
    redis::Client::open(info.set_addr(addr))
}

/// `cluster_enabled:1` in an `INFO cluster` reply.
#[must_use]
pub fn reports_cluster(info: &str) -> bool {
    info.lines().any(|line| line.trim() == "cluster_enabled:1")
}

#[cfg(test)]
mod tests {
    use super::{is_node_command, reports_cluster};

    #[test]
    fn node_commands_stay_on_one_node() {
        for name in [
            "SCAN", "scan", "INFO", "DBSIZE", "CONFIG", "SLOWLOG", "CLUSTER",
        ] {
            assert!(is_node_command(&redis::cmd(name)), "{name}");
        }
        for name in ["GET", "HSET", "TYPE", "EVAL", "UNLINK"] {
            assert!(!is_node_command(&redis::cmd(name)), "{name}");
        }
    }

    #[test]
    fn cluster_is_read_from_info() {
        assert!(reports_cluster("# Cluster\r\ncluster_enabled:1\r\n"));
        assert!(!reports_cluster("# Cluster\r\ncluster_enabled:0\r\n"));
        assert!(!reports_cluster(""));
    }
}
