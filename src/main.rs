mod action;
mod app;
mod cli;
mod clipboard;
mod command;
mod config;
mod keybindings;
mod mappings;
mod mode;
mod pubsub_notify;
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

    // rustls needs a process-wide crypto provider. This errors only if one is
    // already installed, which is fine.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let client = args.client()?;
    let target = cli::connection_label(client.get_connection_info());
    *state.db.lock() = client.get_connection_info().redis_settings().db();
    let manager: ConnectionManager = ConnectionManager::new(client.clone())
        .await
        .map_err(|e| color_eyre::eyre::eyre!(e))?;

    let mut watcher = Runner::new(client, manager.clone(), state.clone(), tx.clone())
        .cancellation_token(cancellation_token.clone())
        .read_only(args.read_only);

    let mut app = App::new(
        state,
        tx,
        rx,
        watcher.tx(),
        watcher.pubsub_tx(),
        target,
        args.tick_rate,
        args.frame_rate,
    )
    .read_only(args.read_only);

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
