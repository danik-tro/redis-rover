//! Live updates of the open key: which way the app follows it
//! (`CLIENT TRACKING`, polling, or not at all).

use std::time::Duration;

use redis::{aio::MultiplexedConnection, AsyncConnectionConfig, ProtocolVersion, PushInfo};
use tokio::sync::mpsc::UnboundedSender;

use crate::config::{LiveConfig, LiveMode};

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
