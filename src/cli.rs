use clap::Parser;
use color_eyre::eyre::{eyre, Result};
use redis::{ConnectionAddr, ConnectionInfo, IntoConnectionInfo};

use crate::utils::version;

#[derive(Parser)]
#[command(author, version = version(), about)]
pub struct Cli {
    #[arg(
        short,
        long,
        value_name = "FLOAT",
        help = "Tick rate, i.e. number of ticks per second",
        default_value_t = 4.0
    )]
    pub tick_rate: f64,

    #[arg(
        short,
        long,
        value_name = "FLOAT",
        help = "Frame rate, i.e. number of frames per second",
        default_value_t = 4.0
    )]
    pub frame_rate: f64,

    #[arg(
        short,
        long,
        value_name = "URL",
        help = "Redis connection URL (e.g. redis://localhost:6379)",
        default_value = "redis://localhost:6379",
        value_parser = parse_redis_url,
    )]
    pub url: String,

    #[arg(
        long,
        help = "Prompt for an auth token securely (input is hidden). The token is used as the connection password."
    )]
    pub token: bool,
}

/// Validates a Redis URL at parse time so a malformed `--url` fails fast with a
/// descriptive error instead of a generic "something went wrong" later. Returns
/// the original string unchanged on success.
fn parse_redis_url(s: &str) -> Result<String, String> {
    s.into_connection_info()
        .map(|_| s.to_string())
        .map_err(|e| format!("invalid Redis URL: {e}"))
}

/// Human-readable connection target for display, e.g. `redis://localhost:6379/0`.
/// Built from the parsed address (never the raw `--url`), so credentials
/// embedded in the URL are never shown.
pub fn connection_label(info: &ConnectionInfo) -> String {
    let addr = match info.addr() {
        ConnectionAddr::Tcp(host, port) => format!("redis://{host}:{port}"),
        ConnectionAddr::TcpTls { host, port, .. } => format!("rediss://{host}:{port}"),
        ConnectionAddr::Unix(path) => format!("unix://{}", path.display()),
        #[allow(unreachable_patterns)]
        other => other.to_string(),
    };
    format!("{addr}/{}", info.redis_settings().db())
}

impl Cli {
    /// Builds typed [`ConnectionInfo`] from the parsed args, prompting for a
    /// token if `--token` was passed.
    ///
    /// The prompt runs before the TUI is initialised so the terminal is still
    /// in normal mode and the password can be entered safely. The token is set
    /// on the typed `redis` settings, whose `Debug` impl redacts the password —
    /// so it never leaks into logs or panic dumps (unlike embedding it in the
    /// URL string).
    pub fn connection_info(&self) -> Result<ConnectionInfo> {
        let conn_info = self
            .url
            .as_str()
            .into_connection_info()
            .map_err(|e| eyre!("Invalid Redis URL: {e}"))?;

        if !self.token {
            return Ok(conn_info);
        }

        let token = rpassword::prompt_password("Redis auth token: ")
            .map_err(|e| eyre!("Failed to read token: {e}"))?;
        let redis = conn_info.redis_settings().clone().set_password(token);

        Ok(conn_info.set_redis_settings(redis))
    }
}

#[cfg(test)]
mod tests {
    use redis::IntoConnectionInfo;

    use super::connection_label;

    #[test]
    fn label_includes_host_port_and_db() {
        let info = "redis://localhost:6379".into_connection_info().unwrap();
        assert_eq!(connection_label(&info), "redis://localhost:6379/0");

        let info = "redis://example.com:6380/3".into_connection_info().unwrap();
        assert_eq!(connection_label(&info), "redis://example.com:6380/3");
    }

    #[test]
    fn label_never_contains_credentials() {
        let info = "redis://user:s3cret@localhost:6379/1"
            .into_connection_info()
            .unwrap();
        let label = connection_label(&info);
        assert_eq!(label, "redis://localhost:6379/1");
        assert!(!label.contains("s3cret"));
    }
}
