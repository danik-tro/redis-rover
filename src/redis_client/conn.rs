//! The connection the rest of the app talks through: one server or a whole
//! cluster. An enum rather than a generic, so `Storage` and the `client`
//! helpers stay connection-agnostic without a type parameter on each of them.

use redis::{
    aio::{ConnectionLike, ConnectionManager},
    cluster_async::ClusterConnection,
    Cmd, Pipeline, RedisFuture, Value,
};

/// A cheap-to-clone handle on a Redis connection. Open one with
/// [`runner::connect`](super::runner::connect), which applies the configured
/// timeouts.
#[derive(Clone)]
pub enum Conn {
    /// One server, reconnected by the manager.
    Single(ConnectionManager),
    /// A cluster: commands are routed by slot.
    Cluster(ClusterConnection),
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

impl ConnectionLike for Conn {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
        match self {
            Self::Single(conn) => conn.req_packed_command(cmd),
            Self::Cluster(conn) => conn.req_packed_command(cmd),
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
        }
    }

    fn get_db(&self) -> i64 {
        match self {
            Self::Single(conn) => conn.get_db(),
            Self::Cluster(conn) => conn.get_db(),
        }
    }
}
