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
mod external_editor;
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
#[doc(hidden)]
pub use redis_client::escape::printable_lines;
pub use target::TargetError;

use color_eyre::eyre::Result;
use redis::cluster::ClusterClient;
use redis_client::{
    conn::{reports_cluster, Conn, Connector},
    runner::{connect_at_startup, connect_cluster, Link, Runner},
};
use state::SharedState;
use target::ClusterMode;
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
        let target = resolve_target(&args)?;
        let client = target.client()?;
        let label = target.label(client.get_connection_info());
        print!("{}", probe::report(&client, &label).await?);
        return Ok(());
    }

    initialize_logging()?;
    initialize_panic_handler()?;

    let cancellation_token = CancellationToken::new();
    let (tx, rx) = mpsc::channel(ACTION_CHANNEL_CAPACITY);

    let state = SharedState::default();

    let target = resolve_target(&args)?;
    let (client, cluster) = target.clients()?;
    let label = target.label(client.get_connection_info());
    *state.db.lock() = client.get_connection_info().redis_settings().db();
    let session = open_session(&target, &client, cluster)
        .await
        .map_err(|e| color_eyre::eyre::eyre!(e))?;
    if let Some(warning) = session.warning {
        action::try_send_action(&tx, action::Action::Notice(warning));
    }

    let mut watcher = Runner::new(client, session.conn, state.clone(), tx.clone())
        .connector(session.connector)
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
    .live_tx(watcher.live_tx())
    .monitor_tx(watcher.monitor_tx())
    .environment(target.environment);

    watcher.start();
    let run_result = app.run(cancellation_token).await;
    watcher.shutdown().await;
    run_result?;

    Ok(())
}

/// The session's first connection and how to open the others.
struct Session {
    conn: Conn,
    connector: Connector,
    /// Shown as a toast once the app runs.
    warning: Option<String>,
}

/// Connect as `target` asks. In [`ClusterMode::Auto`] one node is asked
/// (`INFO cluster`) and, if it is in a cluster, the cluster client takes
/// over with it as the seed. If that fails (nodes announce addresses this
/// machine can't reach, as behind a port forward) the session stays on the
/// one node, as it always did, and says so.
async fn open_session(
    target: &target::Target,
    client: &redis::Client,
    cluster: Option<ClusterClient>,
) -> std::result::Result<Session, Box<dyn std::error::Error + Sync + Send>> {
    let single = Connector::single(client.clone());
    let cluster = match (target.cluster, cluster) {
        (ClusterMode::On, Some(cluster)) => {
            let conn = connect_cluster(&cluster, Link::Interactive).await?;
            return Ok(Session {
                conn,
                connector: Connector::cluster(client.clone(), cluster),
                warning: None,
            });
        }
        (_, cluster) => cluster,
    };
    let mut conn = connect_at_startup(client.clone()).await?;
    let Some(cluster) = cluster else {
        return Ok(Session {
            conn,
            connector: single,
            warning: None,
        });
    };
    let info: redis::RedisResult<String> = redis::cmd("INFO")
        .arg("cluster")
        .query_async(&mut conn)
        .await;
    if !info.is_ok_and(|info| reports_cluster(&info)) {
        return Ok(Session {
            conn,
            connector: single,
            warning: None,
        });
    }
    match connect_cluster(&cluster, Link::Interactive).await {
        Ok(conn) => Ok(Session {
            conn,
            connector: Connector::cluster(client.clone(), cluster),
            warning: None,
        }),
        Err(err) => {
            log::warn!("Cluster client failed, staying on one node: {err}");
            Ok(Session {
                conn,
                connector: single,
                warning: Some("cluster nodes unreachable · showing this node only".to_owned()),
            })
        }
    }
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
    pub use crate::redis_client::bytes::text_from_bytes;
    pub use crate::redis_client::cluster::MasterNode;
    pub use crate::redis_client::conn::{Conn, Connector};
    pub use crate::redis_client::event::RedisEvent;
    pub use crate::redis_client::live::{KeyChange, LiveCmd, Liveness};
    pub use crate::redis_client::metrics::{
        ClusterMetrics, ClusterSummary, InfoSnapshot, MetricsHistory,
    };
    pub use crate::redis_client::monitor::MonitorCmd;
    pub use crate::redis_client::monitor_store::{
        parse_line, MonitorLimits, MonitorStatus, MonitorStore, ParsedLine,
    };
    pub use crate::redis_client::pubsub::PubSubCmd;
    pub use crate::redis_client::pubsub_store::{ConnStatus, EntryKind, PubSubStore, Target};
    pub use crate::redis_client::runner::{
        connect, connect_cluster, Link, Runner, VALUE_FETCH_DELAY_MS,
    };
    pub use crate::redis_client::slowlog::{SlowlogData, SlowlogEntry};
    pub use crate::redis_client::storage::Storage;
    pub use crate::redis_client::types::{
        ConsumerInfo, GroupDetail, GroupInfo, ItemDelete, ItemEdit, JsonField, JsonPreview,
        JsonPreviewKind, KeyItem, KeyMeta, KeyValue, KeyspaceState, LoadedValue, MemberKind,
        NewKeySpec, PageRow, PendingEntry, RedisInfo, RedisType, Sort, SortKey, StreamEntry,
        StreamValue, SweepApply, SweepResult, Ttl, TypeFilter, TypeScan, ValuePageInfo,
    };
    pub use crate::redis_client::value_edit::{diff, Diff, ValueEdit};
    pub use crate::state::{FrameData, SharedState};
    pub use crate::target::Environment;
    pub use crate::widgets::confirm::ConfirmPopup;
    pub use crate::widgets::dashboard::{Dashboard, DashboardWidget};
    pub use crate::widgets::info::{Info, InfoWidget};
    pub use crate::widgets::keyspace::{KeySpace, KeySpaceWidget, LoadProgress, PageInfo};
    pub use crate::widgets::monitor::{Monitor, MonitorWidget};
    pub use crate::widgets::pubsub::{PubSub, PubSubWidget};
    pub use crate::widgets::slowlog::{Slowlog, SlowlogWidget};
    pub use crate::widgets::text_view::{render as render_text, TextCache, TextLabels, TextView};
    pub use crate::widgets::toast::{Toast, ToastWidget};

    /// The MONITOR start confirm on `env`, `typed` in its input on Prod.
    pub fn render_monitor_confirm(
        area: ratatui::layout::Rect,
        buf: &mut ratatui::buffer::Buffer,
        env: Environment,
        typed: &str,
    ) {
        use crate::widgets::confirm::{typed_input_titled, TypedText};
        use ratatui::widgets::Widget;
        let popup = ConfirmPopup::start_monitor(env);
        if env == Environment::Prod {
            let mut input = typed_input_titled("Type monitor", "monitor");
            input.insert_str(typed);
            let text = TypedText {
                expected: "monitor",
                action: "start",
                object: "MONITOR",
                what: "monitor",
            };
            popup
                .typed_confirm(&text, &input, typed == "monitor")
                .render(area, buf);
        } else {
            popup.render(area, buf);
        }
    }

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

    /// The bulk delete's typed confirm on Prod (the count), with its
    /// warning, as the App draws it.
    pub fn render_typed_count(
        area: ratatui::layout::Rect,
        buf: &mut ratatui::buffer::Buffer,
        lines: &[String],
        warning: &str,
        count: &str,
        typed: &str,
    ) {
        use ratatui::widgets::Widget;
        let mut input = crate::widgets::confirm::typed_input_titled("Type the count", count);
        input.insert_str(typed);
        let text = crate::widgets::confirm::TypedText {
            expected: count,
            action: "delete",
            object: "them",
            what: "the count",
        };
        ConfirmPopup::write_preview(lines, Environment::Prod)
            .warning(Some(warning))
            .typed_confirm(&text, &input, typed == count)
            .render(area, buf);
    }

    /// The `?` help overlay for `mode`, as the App draws it.
    pub fn render_help(area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer, mode: Mode) {
        crate::app::App::render_help_overlay(area, buf, mode);
    }

    /// Page `page` of the `?` help; returns the page count.
    pub fn render_help_page(
        area: ratatui::layout::Rect,
        buf: &mut ratatui::buffer::Buffer,
        mode: Mode,
        page: usize,
    ) -> usize {
        crate::app::App::render_help_page(area, buf, mode, page)
    }
}
