//! `--probe`: connect, ask the server a few questions, print a short report.
//!
//! Each step that can fail on an older or restricted server (`HELLO`,
//! `ROLE`, `CLUSTER INFO`) is reported, never fatal: a Redis 5 server says
//! `RESP3: no` and the probe still exits 0. Only failing to connect (or
//! `PING`) is an error.

use std::{
    fmt::Write as _,
    time::{Duration, Instant},
};

use color_eyre::eyre::{eyre, Result};
use redis::{aio::MultiplexedConnection, RedisResult, Value};

use crate::redis_client::{
    client::parse_info,
    live::{self, Capability},
};

/// How long connecting (and each command) may take.
const TIMEOUT: Duration = Duration::from_secs(5);

/// Connect with `client` and build the report. `label` is the
/// credential-free target (`connection_label`).
///
/// # Errors
///
/// The connection or `PING` fails or times out.
pub async fn report(client: &redis::Client, label: &str) -> Result<String> {
    let start = Instant::now();
    let mut conn = connect(client).await?;
    let pong: String = timed(redis::cmd("PING").query_async(&mut conn))
        .await
        .map_err(|e| eyre!("PING failed: {e}"))?;
    let rtt = start.elapsed();

    let info: RedisResult<String> = timed(
        redis::cmd("INFO")
            .arg("server")
            .query_async::<String>(&mut conn),
    )
    .await;
    let role: RedisResult<Value> = timed(redis::cmd("ROLE").query_async(&mut conn)).await;
    let cluster: RedisResult<String> = timed(
        redis::cmd("CLUSTER")
            .arg("INFO")
            .query_async::<String>(&mut conn),
    )
    .await;
    let resp3 = resp3_supported(client).await.unwrap_or(false);
    let live = live_line(client, resp3).await;

    let rows = [
        ("target", label.to_owned()),
        (
            "PING",
            format!(
                "{pong} ({:.1} ms incl. connect)",
                rtt.as_secs_f64() * 1000.0
            ),
        ),
        ("server", server_line(info.as_deref())),
        ("role", role_line(&role)),
        ("cluster", cluster_line(&cluster)),
        ("RESP3", resp3_line(resp3).to_owned()),
        ("live", live),
    ];
    let mut out = String::new();
    for (name, value) in rows {
        let _ = writeln!(out, "{name:<9} {value}");
    }
    Ok(out)
}

async fn connect(client: &redis::Client) -> Result<MultiplexedConnection> {
    tokio::time::timeout(TIMEOUT, client.get_multiplexed_async_connection())
        .await
        .map_err(|_| eyre!("connecting timed out after {TIMEOUT:?}"))?
        .map_err(|e| eyre!("connecting failed: {e}"))
}

async fn timed<T>(fut: impl std::future::Future<Output = RedisResult<T>>) -> RedisResult<T> {
    tokio::time::timeout(TIMEOUT, fut)
        .await
        .unwrap_or_else(|_| Err(redis::RedisError::from((redis::ErrorKind::Io, "timed out"))))
}

/// `Redis 7.2.4 (standalone)`, from `INFO server`.
fn server_line(info: Result<&str, &redis::RedisError>) -> String {
    match info {
        Ok(raw) => {
            let fields = parse_info(raw);
            let version = fields.get("redis_version").copied().unwrap_or("?");
            match fields.get("redis_mode") {
                Some(mode) => format!("Redis {version} ({mode})"),
                None => format!("Redis {version}"),
            }
        }
        Err(e) => format!("n/a ({e})"),
    }
}

/// The first element of the `ROLE` reply: `master`, `slave`, `sentinel`.
fn role_line(role: &RedisResult<Value>) -> String {
    match role {
        Ok(Value::Array(items)) => match items.first() {
            Some(Value::BulkString(bytes)) => String::from_utf8_lossy(bytes).into_owned(),
            Some(Value::SimpleString(s)) => s.clone(),
            _ => "?".to_owned(),
        },
        Ok(_) => "?".to_owned(),
        Err(e) => format!("n/a ({e})"),
    }
}

/// `no` when cluster support is disabled, else the `cluster_state`.
fn cluster_line(cluster: &RedisResult<String>) -> String {
    match cluster {
        Ok(raw) => {
            let state = parse_info(raw).get("cluster_state").copied().unwrap_or("?");
            format!("yes (state {state})")
        }
        Err(e) if e.to_string().to_lowercase().contains("disabled") => "no".to_owned(),
        Err(e) => format!("n/a ({e})"),
    }
}

/// What `live.mode` would do on this server: `tracking`, `polling 2s (…)`.
async fn live_line(client: &redis::Client, resp3: bool) -> String {
    let config = crate::config::try_get()
        .map(|cfg| cfg.live)
        .unwrap_or_default();
    let capability = if !live::needs_capability(config.mode) {
        None
    } else if !resp3 {
        Some(Capability::Resp2)
    } else {
        match live::open_tracking(client, None).await {
            Ok((capability, _)) => Some(capability),
            Err(e) => return format!("n/a ({e})"),
        }
    };
    live::plan_line(&config, &live::choose(config.mode, capability.as_ref()))
}

/// Whether the server speaks RESP3: `HELLO 3` on a scratch connection
/// (`HELLO` switches the connection's protocol, so it gets one of its own).
/// Any error reply (unknown command before 6.0, `NOPROTO`) means RESP2 only.
///
/// # Errors
///
/// The scratch connection can't be opened, or `HELLO` times out or loses
/// the connection.
pub async fn resp3_supported(client: &redis::Client) -> RedisResult<bool> {
    let mut conn = connect(client)
        .await
        .map_err(|e| redis::RedisError::from((redis::ErrorKind::Io, "connect", e.to_string())))?;
    hello_means_resp3(timed(redis::cmd("HELLO").arg(3).query_async::<Value>(&mut conn)).await)
}

/// A reply means RESP3; an error reply means RESP2 only; a lost or timed
/// out connection says nothing and stays an error.
fn hello_means_resp3(hello: RedisResult<Value>) -> RedisResult<bool> {
    match hello {
        Ok(_) => Ok(true),
        Err(e) if e.is_io_error() || e.is_connection_dropped() || e.is_timeout() => Err(e),
        Err(_) => Ok(false),
    }
}

fn resp3_line(resp3: bool) -> &'static str {
    if resp3 {
        "yes"
    } else {
        "no"
    }
}

#[cfg(test)]
mod tests {
    use redis::{ErrorKind, RedisError, Value};

    use super::{cluster_line, hello_means_resp3, resp3_line, role_line, server_line};

    fn server_err(msg: &'static str) -> RedisError {
        RedisError::from((
            ErrorKind::Server(redis::ServerErrorKind::ResponseError),
            "err",
            msg.to_owned(),
        ))
    }

    #[test]
    fn old_servers_are_reported_not_fatal() {
        // Redis 5: HELLO is an unknown command.
        let hello = Err(server_err("unknown command `HELLO`"));
        assert!(!hello_means_resp3(hello).unwrap());
        assert!(
            !hello_means_resp3(Err(server_err("NOPROTO unsupported protocol version"))).unwrap()
        );
        assert!(hello_means_resp3(Ok(Value::Nil)).unwrap());
        let io = RedisError::from((ErrorKind::Io, "timed out"));
        assert!(hello_means_resp3(Err(io)).is_err());
        assert_eq!(resp3_line(false), "no");

        let cluster = Err(server_err("This instance has cluster support disabled"));
        assert_eq!(cluster_line(&cluster), "no");
        assert_eq!(
            cluster_line(&Ok(
                "cluster_state:ok\r\ncluster_slots_assigned:16384\r\n".into()
            )),
            "yes (state ok)"
        );

        let role = Ok(Value::Array(vec![Value::BulkString(b"master".to_vec())]));
        assert_eq!(role_line(&role), "master");
        assert!(role_line(&Err(server_err("unknown command `ROLE`"))).starts_with("n/a"));
    }

    #[test]
    fn server_line_reads_version_and_mode() {
        let raw = "# Server\r\nredis_version:5.0.14\r\nredis_mode:standalone\r\n";
        assert_eq!(server_line(Ok(raw)), "Redis 5.0.14 (standalone)");
        assert_eq!(server_line(Ok("redis_version:2.8.0\r\n")), "Redis 2.8.0");
    }
}
