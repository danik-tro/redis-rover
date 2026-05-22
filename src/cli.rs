use clap::Parser;
use color_eyre::eyre::{eyre, Result};

use crate::utils::{inject_token, version};

#[derive(Parser, Debug)]
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
        default_value = "redis://localhost:6379"
    )]
    pub url: String,

    #[arg(
        long,
        help = "Prompt for an auth token securely (input is hidden). The token is appended to the URL as the password."
    )]
    pub token: bool,
}

impl Cli {
    /// Returns the final Redis URL, prompting for a token if --token was passed.
    /// The prompt runs before the TUI is initialised so the terminal is still in
    /// normal mode and the password can be entered safely.
    pub fn redis_url(&self) -> Result<String> {
        if !self.token {
            return Ok(self.url.clone());
        }

        let token = rpassword::prompt_password("Redis auth token: ")
            .map_err(|e| eyre!("Failed to read token: {e}"))?;

        inject_token(&self.url, &token)
    }
}
