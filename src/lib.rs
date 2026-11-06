//! `rrover`, a terminal UI Redis client.
//!
//! This crate is published for the `rrover` binary. The library exists so the
//! benches and integration tests can reach internal types; it has no stable API.

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

pub use cli::Cli;

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

/// Connect to Redis and run the TUI until the user quits.
///
/// # Errors
///
/// Fails if logging or the panic handler can't be set up, the connection
/// settings are invalid, the first connection to Redis fails, or the app loop
/// returns an error.
pub async fn run(args: Cli) -> Result<()> {
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

/// Internals for the benches and integration tests. Not a stable API: anything
/// here can change in any release.
#[doc(hidden)]
pub mod internals {
    pub use crate::action::Action;
    pub use crate::redis_client::event::RedisEvent;
    pub use crate::redis_client::pubsub::PubSubCmd;
    pub use crate::redis_client::pubsub_store::{PubSubStore, Target};
    pub use crate::redis_client::runner::Runner;
    pub use crate::redis_client::types::{
        ItemDelete, ItemEdit, KeyItem, KeyMeta, KeyValue, KeyspaceState, LoadedValue, NewKeySpec,
        RedisType,
    };
    pub use crate::state::SharedState;
    pub use crate::widgets::keyspace::{KeySpace, KeySpaceWidget};
    pub use crate::widgets::text_view::{render as render_text, TextCache, TextLabels, TextView};
}
