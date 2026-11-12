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

use crate::redis_client::client::parse_info;

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
    // `HELLO 3` switches the connection to RESP3, so it gets one of its own.
    let hello = match connect(client).await {
        Ok(mut conn) => timed(redis::cmd("HELLO").arg(3).query_async::<Value>(&mut conn)).await,
        Err(e) => Err(redis::RedisError::from((
            redis::ErrorKind::Io,
            "connect",
            e.to_string(),
        ))),
    };

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
        ("RESP3", resp3_line(&hello).to_owned()),
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

/// Any error from `HELLO 3` (unknown command before 6.0, `NOPROTO`) means
/// the server speaks RESP2 only.
fn resp3_line(hello: &RedisResult<Value>) -> &'static str {
    if hello.is_ok() {
        "yes"
    } else {
        "no"
    }
}

#[cfg(test)]
mod tests {
    use redis::{ErrorKind, RedisError, Value};

    use super::{cluster_line, resp3_line, role_line, server_line};

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
        assert_eq!(resp3_line(&hello), "no");
        assert_eq!(resp3_line(&Ok(Value::Nil)), "yes");

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
