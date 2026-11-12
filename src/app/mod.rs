mod actions;
mod clipboard;
mod dashboard;
mod db;
mod input;
mod keyspace;
mod pubsub;
mod render;

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};

use crate::{
    clock::{Clock, SystemClock},
    config,
    redis_client::{event::RedisEvent, pubsub::PubSubCmd},
    state::{FrameData, SharedState},
    target::Environment,
};
use color_eyre::eyre::Result;
use ratatui::crossterm::event::KeyEvent;
use ratatui::prelude::Rect;

use tokio::sync::{
    broadcast,
    mpsc::{self, Receiver, Sender, UnboundedSender},
};
use tokio_util::sync::CancellationToken;

use crate::{
    action::{self, Action},
    clipboard::Clipboard,
    mode::Mode,
    tui,
    widgets::{
        dashboard::Dashboard, db_picker::DbPicker, info::Info, keyspace::KeySpace, pubsub::PubSub,
        toast::Toast,
    },
};

pub struct App {
    state: SharedState,

    tick_rate: f64,
    frame_rate: f64,

    mode: Mode,
    previous_mode: Option<Mode>,
    error_message: Option<String>,
    /// When the `?` help overlay was opened. `None` means it's hidden. The
    /// overlay auto-dismisses after [`HELP_TIMEOUT`] or on the next command.
    help_opened_at: Option<Instant>,
    /// After a non-destructive edit (value/TTL), the name of the key to keep
    /// selected once the post-write keyspace refresh lands, so the cursor stays
    /// put instead of snapping back to the top.
    reselect_key: Option<String>,
    /// After an in-collection add-item, the value-table row to restore focus to
    /// once the (now larger) collection value reloads, so the user stays inside
    /// the collection instead of bouncing back to the key list.
    restore_value_row: Option<usize>,
    /// Armed by `load_new_keys` after the post-add-item refresh re-requests the
    /// value, so the *final* value load (not an earlier racing one) restores
    /// value focus.
    restore_value_pending: bool,
    /// After turning a value page with `j` / `k` / `n` / `p`, the row to select
    /// once the new page loads (`usize::MAX` = last row; clamped).
    page_turn_row: Option<usize>,
    /// For each JSON field opened from a preview (#49), the preview row it
    /// was opened from, so Esc lands back on it.
    json_rows: Vec<usize>,
    /// A JSON field was opened or closed: once its value loads, focus it
    /// (its text, or this row of its field table).
    json_focus_row: Option<usize>,

    should_quit: bool,
    /// Keys of a partially typed chord (a strict prefix of some binding).
    pending_keys: Vec<KeyEvent>,
    /// When the last key went into `pending_keys`, for [`CHORD_TIMEOUT`].
    last_key_at: Option<Instant>,
    /// `FetchKeys` requests sent but not yet answered; drives the loading
    /// indicator.
    keys_in_flight: usize,

    tx: mpsc::Sender<Action>,
    rx: mpsc::Receiver<Action>,

    redis_tx: broadcast::Sender<RedisEvent>,
    summary: Info,
    keyspace: KeySpace,
    dashboard: Dashboard,
    db_picker: DbPicker,
    pubsub: PubSub,
    /// Control channel of the Pub/Sub task.
    pubsub_tx: UnboundedSender<PubSubCmd>,
    /// Where `b` / Esc in the Pub/Sub tab goes back to. Kept apart from
    /// `previous_mode`, which an error popup inside the tab overwrites.
    pubsub_return: Mode,
    /// Read-only: refuse writes before any input is asked for. The same
    /// flag the runner checks (`Runner::read_only_flag`).
    read_only: Arc<AtomicBool>,
    /// Sets the protections (preview, typed confirms) and the colours.
    environment: Environment,
    clipboard: Clipboard,
    /// Short message drawn above every mode and popup.
    toast: Toast,
    /// What the widgets draw from, rebuilt at the start of every frame.
    frame: FrameData,
    /// Read once per action (and per frame) and passed down.
    clock: Arc<dyn Clock>,
    /// When `Render` draws (#50).
    redraw: Redraw,
}

/// What decides whether `Render` draws: `dirty`, or data that changes on
/// its own having moved past what the last frame showed.
#[derive(Debug, Default, Clone, Copy)]
struct Redraw {
    /// Something on screen changed since the last frame.
    dirty: bool,
    /// The footer text (or cluster flag) changed since the last frame.
    footer_stale: bool,
    /// When the last frame was drawn.
    drawn_at: Option<Instant>,
    /// `info_seq` of the `INFO` sample the last frame showed.
    info_seq: u64,
    /// `PubSubStore::version` the Pub/Sub tab showed.
    pubsub: u64,
    /// Wall-clock second of that frame (the tab shows rates and ages).
    wall_sec: u64,
    /// Frames drawn since `since`, logged at debug once a minute.
    frames: u64,
    since: Option<Instant>,
}

/// Shown when a write is attempted in `--read-only` mode.
const READ_ONLY_NOTICE: &str = "Read-only mode: writes are disabled.";

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        state: SharedState,
        tx: Sender<Action>,
        rx: Receiver<Action>,
        redis_tx: broadcast::Sender<RedisEvent>,
        pubsub_tx: UnboundedSender<PubSubCmd>,
        target: &str,
        tick_rate: f64,
        frame_rate: f64,
    ) -> Self {
        let _ = config::get();

        let mode = Mode::KeySpace;

        let summary = Info::new(target);
        let keyspace = KeySpace::new(Vec::new());
        let dashboard = Dashboard::new(state.metrics.clone());
        let pubsub = PubSub::new(state.pubsub.clone());

        let mut app = Self {
            state,
            summary,
            keyspace,
            dashboard,
            tick_rate,
            frame_rate,
            should_quit: false,
            mode,
            previous_mode: None,
            error_message: None,
            help_opened_at: None,
            reselect_key: None,
            restore_value_row: None,
            restore_value_pending: false,
            page_turn_row: None,
            json_rows: Vec::new(),
            json_focus_row: None,
            pending_keys: Vec::new(),
            last_key_at: None,
            keys_in_flight: 0,
            tx,
            rx,
            redis_tx,
            db_picker: DbPicker::default(),
            pubsub,
            pubsub_tx,
            pubsub_return: Mode::KeySpace,
            read_only: Arc::new(AtomicBool::new(false)),
            environment: Environment::Local,
            clipboard: Clipboard::default(),
            toast: Toast::default(),
            frame: FrameData::default(),
            clock: Arc::new(SystemClock),
            redraw: Redraw {
                dirty: true,
                ..Redraw::default()
            },
        };
        app.sync_hint();
        app
    }

    /// Refuse every write (`--read-only`): show the `RO` badge and drop the
    /// write commands from the footer hint.
    #[cfg(test)]
    #[must_use]
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.set_read_only(read_only);
        self
    }

    /// The connection's environment: footer badge and top line colour.
    #[must_use]
    pub fn environment(mut self, environment: Environment) -> Self {
        self.environment = environment;
        self.summary.environment = environment;
        self
    }

    /// Share the runner's read-only flag (`Runner::read_only_flag`), keeping
    /// its current value.
    #[must_use]
    pub fn read_only_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.read_only = flag;
        let read_only = self.is_read_only();
        self.set_read_only(read_only);
        self
    }

    pub(super) fn is_read_only(&self) -> bool {
        self.read_only.load(Ordering::Acquire)
    }

    /// Set the shared flag, the `RO` badge and the footer hint.
    pub(super) fn set_read_only(&mut self, read_only: bool) {
        self.read_only.store(read_only, Ordering::Release);
        self.summary.read_only = read_only;
        self.sync_hint();
    }

    /// Run the main event loop until quit.
    ///
    /// # Errors
    ///
    /// Returns an error if the TUI fails to enter or if any action
    /// dispatch returns an error.
    pub async fn run(&mut self, cancellation_token: CancellationToken) -> Result<()> {
        let mut tui = tui::Tui::new()?
            .tick_rate(self.tick_rate)
            .frame_rate(self.frame_rate)
            .cancellation_token(cancellation_token.clone());

        // tui.mouse(true);
        // `Tui` has no `Drop` teardown: always shut down explicitly, even when
        // entering or the loop fails, so the terminal is restored.
        let result = match tui.enter() {
            Ok(()) => self.event_loop(&mut tui).await,
            Err(err) => Err(err),
        };
        let shutdown = tui.shutdown().await;
        result.and(shutdown)
    }

    async fn event_loop(&mut self, tui: &mut tui::Tui) -> Result<()> {
        loop {
            // TODO: refactor with async_channel crate
            // replace with select multiplex
            if let Some(e) = tui.next().await {
                if let Some(action) = self.handle_event(&e) {
                    action::try_send_action(&self.tx, action);
                }
            }

            while let Ok(action) = self.rx.try_recv() {
                if let Some(next) = self.handle_action(&action, tui)? {
                    action::try_send_action(&self.tx, next);
                }
            }
            if self.should_quit {
                return Ok(());
            }
        }
    }

    fn resize(tui: &mut tui::Tui, (w, h): (u16, u16)) -> Result<()> {
        tui.resize(Rect::new(0, 0, w, h))?;
        Ok(())
    }

    fn handle_event(&mut self, e: &tui::Event) -> Option<Action> {
        // Keys often change the screen without an action (text typed into a
        // popup, the DB picker), so every terminal event marks it dirty.
        if !matches!(e, tui::Event::Tick | tui::Event::Render) {
            self.redraw.dirty = true;
        }
        match e {
            // Emitted once when the event task starts: trigger the initial
            // keyspace load so the screen is populated without a manual action.
            tui::Event::Init => Some(Action::LoadKeySpace),
            tui::Event::Quit => Some(Action::Quit),
            tui::Event::Tick => Some(Action::Tick),
            tui::Event::Render => Some(Action::Render),
            tui::Event::Resize(x, y) => Some(Action::Resize(*x, *y)),
            tui::Event::Key(key) => self.handle_key_event(*key),
            _ => None,
        }
    }

    fn send_redis_event(&self, event: RedisEvent) {
        if let Err(err) = self.redis_tx.send(event) {
            log::error!("Failed to send redis event: {err:?}");
        }
    }

    /// In `--read-only` mode, show a notice and return `true` so the caller
    /// stops before opening any write UI.
    fn deny_write(&mut self) -> bool {
        let read_only = self.is_read_only();
        if read_only {
            self.keyspace.show_notice(READ_ONLY_NOTICE);
        }
        read_only
    }

    /// The latest `INFO` sample says the server runs in cluster mode.
    fn cluster_enabled(&self) -> bool {
        self.state
            .metrics
            .lock()
            .latest_snapshot
            .as_ref()
            .is_some_and(|s| s.cluster_enabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    use crate::{
        redis_client::{
            event::RedisEvent, pubsub::PubSubCmd, pubsub_store::Target, types::RedisType,
        },
        state::SharedState,
    };

    use crossterm::event::KeyCode;
    use ratatui::prelude::Rect;
    use ratatui::{buffer::Buffer, crossterm::event::KeyEvent, widgets::StatefulWidget};

    use tokio::sync::{
        broadcast,
        mpsc::{self},
    };

    use crate::action::Action;
    use crate::app::render::AppWidget;

    pub(super) fn app() -> App {
        app_with_pubsub().0
    }

    pub(super) fn app_with_pubsub() -> (App, mpsc::UnboundedReceiver<PubSubCmd>) {
        let (tx, rx) = mpsc::channel(8);
        let (redis_tx, _) = broadcast::channel(8);
        let (pubsub_tx, pubsub_rx) = mpsc::unbounded_channel();
        let app = App::new(
            SharedState::default(),
            tx,
            rx,
            redis_tx,
            pubsub_tx,
            "redis://localhost:6379",
            4.0,
            30.0,
        );
        (app, pubsub_rx)
    }

    pub(super) fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, crossterm::event::KeyModifiers::empty())
    }

    pub(super) fn meta(key: &str) -> crate::redis_client::types::KeyMeta {
        crate::redis_client::types::KeyMeta {
            key: key.into(),
            r_type: RedisType::String,
            size: 0,
            ttl: -1,
            value: crate::redis_client::types::KeyValue::Unknown,
        }
    }

    pub(super) fn app_with_channels() -> (
        App,
        mpsc::UnboundedReceiver<PubSubCmd>,
        broadcast::Receiver<RedisEvent>,
    ) {
        let (mut app, cmds) = app_with_pubsub();
        let (redis_tx, redis_rx) = broadcast::channel(8);
        app.redis_tx = redis_tx;
        (app, cmds, redis_rx)
    }

    pub(super) fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), crossterm::event::KeyModifiers::CONTROL)
    }

    /// An app with subscriptions `names` (notifications on for each).
    pub(super) fn app_with_subs(names: &[&str]) -> App {
        let app = app();
        {
            let mut store = app.state.pubsub.lock();
            for name in names {
                let (i, _) = store.add(Target::Channel((*name).into()));
                store.subs[i].notify = store.subs[i].notify.toggled();
            }
        }
        app
    }

    pub(super) fn publish(app: &App, channel: &str, payload: &str) {
        app.state
            .pubsub
            .lock()
            .push(channel, None, payload.as_bytes(), SystemTime::now());
    }

    pub(super) fn unread(app: &App) -> Vec<u64> {
        app.state
            .pubsub
            .lock()
            .subs
            .iter()
            .map(|s| s.unread)
            .collect()
    }

    pub(super) fn render_screen(app: &mut App) -> String {
        let area = Rect::new(0, 0, 120, 40);
        let mut buf = Buffer::empty(area);
        app.build_frame(area);
        AppWidget.render(area, &mut buf, app);
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    + "\n"
            })
            .collect()
    }

    /// Press `code` in the app: the key goes through the key map, and the
    /// resulting action runs the same App method `handle_action` would
    /// (that one needs a terminal).
    pub(super) fn press(app: &mut App, code: KeyCode, modifiers: crossterm::event::KeyModifiers) {
        let Some(action) = app.handle_key_event(KeyEvent::new(code, modifiers)) else {
            return;
        };
        match action {
            Action::PubSubFocus => app.pubsub.toggle_focus(),
            Action::PubSubTogglePause => app.pubsub.toggle_pause(),
            Action::PubSubEnter => app.pubsub_enter(),
            Action::ScrollDown => app.scroll_down(),
            Action::ScrollUp => app.scroll_up(),
            Action::TogglePubSub => app.toggle_pubsub(),
            other => panic!("unexpected action {other:?}"),
        }
    }

    pub(super) fn tap(app: &mut App, code: KeyCode) {
        press(app, code, crossterm::event::KeyModifiers::empty());
    }

    /// #50: a key that changes the screen without an action (text typed
    /// into the filter popup) still marks it dirty; ticks don't.
    #[test]
    fn terminal_events_mark_dirty() {
        let mut app = app();
        app.keyspace.enter_filter_pattern(std::time::Instant::now());
        app.redraw.dirty = false;
        assert_eq!(app.handle_event(&tui::Event::Tick), Some(Action::Tick));
        assert!(!app.redraw.dirty);
        assert_eq!(
            app.handle_event(&tui::Event::Key(key(KeyCode::Char('a')))),
            None
        );
        assert!(app.redraw.dirty);
    }

    /// The tab open on `orders:*` with three JSON messages.
    pub(super) fn app_on_orders() -> App {
        let mut app = app();
        app.toggle_pubsub();
        app.subscribe_target(Target::Pattern("orders:*".into()));
        let mut store = app.state.pubsub.lock();
        for i in 1..=3 {
            store.push(
                "orders:new",
                Some("orders:*"),
                format!(r#"{{"id":{i}}}"#).as_bytes(),
                SystemTime::now(),
            );
        }
        drop(store);
        app
    }
}
