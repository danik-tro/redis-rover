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

/// Capacity of the bounded action channel. Renders and ticks are pushed at the
/// frame/tick rate; bounding the channel prevents unbounded growth (and OOM) if
/// the UI loop ever stalls. Render/Tick are idempotent, so a dropped one is a
/// no-op — it re-fires on the next interval.
const ACTION_CHANNEL_CAPACITY: usize = 256;

async fn tokio_main(args: Cli) -> Result<()> {
    initialize_logging()?;
    initialize_panic_handler()?;

    let cancellation_token = CancellationToken::new();
    let (tx, rx) = mpsc::channel(ACTION_CHANNEL_CAPACITY);

    let state = SharedState::default();

    let conn_info = args.connection_info()?;
    let client = redis::Client::open(conn_info).map_err(|e| color_eyre::eyre::eyre!(e))?;
    let manager: ConnectionManager = ConnectionManager::new(client)
        .await
        .map_err(|e| color_eyre::eyre::eyre!(e))?;

    let mut watcher = Runner::new(manager.clone(), state.clone(), tx.clone())
        .cancelation_token(cancellation_token.clone());

    let mut app = App::new(state, tx, rx, watcher.tx(), args.tick_rate, args.frame_rate);

    watcher.start();
    let run_result = app.run(cancellation_token).await;
    watcher.shutdown().await;
    run_result?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Cli::parse();
    if let Err(e) = tokio_main(args).await {
        eprintln!("{}: {e:#}", env!("CARGO_PKG_NAME"));
        Err(e)
    } else {
        Ok(())
    }
}
