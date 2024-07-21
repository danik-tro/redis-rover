mod action;
mod app;
mod cli;
mod command;
mod config;
mod keybindings;
mod mappings;
mod mode;
mod redis_client;
mod state;
mod tui;
mod utils;
mod widgets;

use clap::Parser;
use cli::Cli;
use color_eyre::eyre::Result;
use redis::aio::ConnectionManager;
use redis_client::runner::Runner;
use state::SharedState;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    app::App,
    utils::{initialize_logging, initialize_panic_handler},
};

async fn tokio_main(args: Cli) -> Result<()> {
    initialize_logging()?;
    initialize_panic_handler()?;

    let cancellation_token = CancellationToken::new();
    let (tx, rx) = mpsc::unbounded_channel();

    let state = SharedState::default();

    // TODO: fix error handling. Move to Trait
    let client = redis::Client::open("redis://localhost:6379").unwrap();
    let manager: ConnectionManager = ConnectionManager::new(client).await.unwrap();

    let mut watcher = Runner::new(manager.clone(), state.clone(), tx.clone())
        .cancelation_token(cancellation_token.clone());

    let mut app = App::new(state, tx, rx, watcher.tx(), args.tick_rate, args.frame_rate)?;

    watcher.start();
    app.run(cancellation_token).await?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Cli::parse();
    if let Err(e) = tokio_main(args).await {
        eprintln!("{} error: Something went wrong", env!("CARGO_PKG_NAME"));
        Err(e)
    } else {
        Ok(())
    }
}
