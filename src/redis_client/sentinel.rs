//! A Redis behind Sentinel: which Sentinels to ask, and for which master.

use std::{fmt, time::Duration};

use redis::{
    sentinel::{SentinelClientBuilder, SentinelServerType},
    ConnectionAddr, ErrorKind, RedisError, RedisResult,
};

/// The port a Sentinel listens on unless the URL says otherwise.
pub const DEFAULT_PORT: u16 = 26379;

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

#[cfg(test)]
mod tests {
    use redis::ConnectionAddr;

    use super::SentinelTarget;

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
