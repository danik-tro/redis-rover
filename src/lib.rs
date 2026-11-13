//! `rrover`, a terminal UI Redis client.
//!
//! This crate is published for the `rrover` binary. The library exists so the
//! benches and integration tests can reach internal types; it has no stable API.

mod action;
mod app;
mod cli;
mod clipboard;
mod clock;
mod command;
mod config;
mod keybindings;
mod mappings;
mod mode;
mod probe;
mod pubsub_notify;
mod redis_client;
mod state;
mod target;
mod tui;
mod utils;
mod widgets;

pub use cli::Cli;
pub use target::TargetError;

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
/// A configuration problem (unknown profile, failing `password_command`,
/// invalid config file) is a [`TargetError`]; `main` exits with code 3 on
/// it. Also fails if logging or the panic handler can't be set up, the connection
/// settings are invalid, the first connection to Redis fails, or the app loop
/// returns an error.
pub async fn run(args: Cli) -> Result<()> {
    // `--print-target` opens no socket and writes no log file (creating the
    // log would truncate a running session's).
    if args.print_target {
        print!("{}", resolve_target(&args)?.report()?);
        return Ok(());
    }
    // rustls needs a process-wide crypto provider. This errors only if one is
    // already installed, which is fine.
    let _ = rustls::crypto::ring::default_provider().install_default();
    if args.probe {
        let client = resolve_target(&args)?.client()?;
        let label = cli::connection_label(client.get_connection_info());
        print!("{}", probe::report(&client, &label).await?);
        return Ok(());
    }

    initialize_logging()?;
    initialize_panic_handler()?;

    let cancellation_token = CancellationToken::new();
    let (tx, rx) = mpsc::channel(ACTION_CHANNEL_CAPACITY);

    let state = SharedState::default();

    let target = resolve_target(&args)?;
    let client = target.client()?;
    let label = cli::connection_label(client.get_connection_info());
    *state.db.lock() = client.get_connection_info().redis_settings().db();
    let manager: ConnectionManager = ConnectionManager::new(client.clone())
        .await
        .map_err(|e| color_eyre::eyre::eyre!(e))?;

    let mut watcher = Runner::new(client, manager.clone(), state.clone(), tx.clone())
        .cancellation_token(cancellation_token.clone())
        .read_only(target.read_only);

    let mut app = App::new(
        state,
        tx,
        rx,
        watcher.tx(),
        watcher.pubsub_tx(),
        &label,
        args.tick_rate,
        args.frame_rate,
    )
    .read_only_flag(watcher.read_only_flag())
    .environment(target.environment);

    watcher.start();
    let run_result = app.run(cancellation_token).await;
    watcher.shutdown().await;
    run_result?;

    Ok(())
}

/// The connection target from the flags, the config's profiles and
/// `REDIS_URL`.
fn resolve_target(args: &Cli) -> Result<target::Target> {
    let cfg = config::try_get().map_err(|e| TargetError::Config(e.to_string()))?;
    let redis_url = std::env::var("REDIS_URL").ok();
    Ok(target::resolve(args, &cfg.profiles, redis_url.as_deref())?)
}

/// Internals for the benches and integration tests. Not a stable API: anything
/// here can change in any release.
#[doc(hidden)]
pub mod internals {
    pub use crate::action::Action;
    pub use crate::clock::{Clock, FixedClock, SystemClock};
    pub use crate::mode::Mode;
    pub use crate::probe::report as probe_report;
    pub use crate::redis_client::event::RedisEvent;
    pub use crate::redis_client::metrics::{InfoSnapshot, MetricsHistory};
    pub use crate::redis_client::pubsub::PubSubCmd;
    pub use crate::redis_client::pubsub_store::{ConnStatus, EntryKind, PubSubStore, Target};
    pub use crate::redis_client::runner::{Runner, VALUE_FETCH_DELAY_MS};
    pub use crate::redis_client::types::{
        ItemDelete, ItemEdit, JsonField, JsonPreview, JsonPreviewKind, KeyItem, KeyMeta, KeyValue,
        KeyspaceState, LoadedValue, NewKeySpec, PageRow, RedisInfo, RedisType, Sort, SortKey,
        SweepApply, SweepResult, TypeFilter, TypeScan, ValuePageInfo,
    };
    pub use crate::state::{FrameData, SharedState};
    pub use crate::target::Environment;
    pub use crate::widgets::confirm::ConfirmPopup;
    pub use crate::widgets::dashboard::{Dashboard, DashboardWidget};
    pub use crate::widgets::info::{Info, InfoWidget};
    pub use crate::widgets::keyspace::{KeySpace, KeySpaceWidget, PageInfo};
    pub use crate::widgets::pubsub::{PubSub, PubSubWidget};
    pub use crate::widgets::text_view::{render as render_text, TextCache, TextLabels, TextView};
    pub use crate::widgets::toast::{Toast, ToastWidget};

    /// The typed confirm on Prod with `typed` in its input, as the App
    /// draws it.
    pub fn render_typed_confirm(
        area: ratatui::layout::Rect,
        buf: &mut ratatui::buffer::Buffer,
        lines: &[String],
        expected: &str,
        typed: &str,
    ) {
        use ratatui::widgets::Widget;
        let mut input = crate::widgets::confirm::typed_input(expected);
        input.insert_str(typed);
        ConfirmPopup::typed_delete(
            lines,
            Environment::Prod,
            expected,
            &input,
            typed == expected,
        )
        .render(area, buf);
    }

    /// The `?` help overlay for `mode`, as the App draws it.
    pub fn render_help(area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer, mode: Mode) {
        crate::app::App::render_help_overlay(area, buf, mode);
    }
}
