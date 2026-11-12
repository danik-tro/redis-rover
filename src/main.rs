use std::process::ExitCode;

use clap::Parser;
use redis_rover::{Cli, TargetError};

/// Exit code for a configuration error: unknown profile, failing
/// `password_command`, invalid config file.
const EXIT_CONFIG: u8 = 3;

#[tokio::main]
async fn main() -> ExitCode {
    let args = Cli::parse();
    match redis_rover::run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}: {e:#}", env!("CARGO_PKG_NAME"));
            if e.downcast_ref::<TargetError>().is_some() {
                ExitCode::from(EXIT_CONFIG)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}
