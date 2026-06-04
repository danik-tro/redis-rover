use clap::Parser;
use color_eyre::eyre::{eyre, Result};
use redis::{ConnectionInfo, IntoConnectionInfo};

use crate::utils::version;

#[derive(Parser)]
#[command(author, version = version(), about)]
pub struct Cli {
    #[arg(
        short,
        long,
        value_name = "FLOAT",
        help = "Tick rate, i.e. number of ticks per second",
        default_value_t = 1.0
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
