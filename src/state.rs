use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use parking_lot::Mutex;

use crate::{
    config,
    redis_client::{
        metrics::MetricsHistory,
        pubsub_store::PubSubStore,
        types::{KeyMeta, KeyspaceState, LoadedValue, PageRow, RedisInfo, ValueState},
    },
    widgets::{info::FooterText, pubsub::PubSubView},
};

#[derive(Clone, Debug)]
pub struct SharedState {
    pub info: Arc<Mutex<Option<RedisInfo>>>,
    /// Bumped by the info task after each `INFO` sample (or failure) has been
    /// written to `info` and `metrics`, so the footer rebuilds its text only
    /// then.
    pub info_seq: Arc<AtomicU64>,
    /// The keyspace page the runner loaded last, until the App moves it into
    /// the key table (`None` once taken).
    pub keys: Arc<Mutex<Option<Vec<KeyMeta>>>>,
    /// In tree mode, the rows of the page in `keys` (groups and one `Leaf`
    /// per key). Written with `keys` under the keyspace lock; taken with it.
    pub page_rows: Arc<Mutex<Option<Vec<PageRow>>>>,
    pub keyspace_state: Arc<Mutex<KeyspaceState>>,
    /// Paging state for the selected key's value.
    pub value_state: Arc<Mutex<ValueState>>,
    pub selected_value: Arc<Mutex<Option<LoadedValue>>>,
    pub metrics: Arc<Mutex<MetricsHistory>>,
    /// Logical database the keyspace browser is connected to.
    pub db: Arc<Mutex<i64>>,
    /// Number of logical databases (`CONFIG GET databases`, default 16).
    pub db_count: Arc<Mutex<usize>>,
    /// A whole value fetched for copying: key name and the text to copy. Kept
    /// out of `Action` so huge values never reach the action log.
    pub yank: Arc<Mutex<Option<(String, String)>>>,
    /// Pub/Sub subscriptions and their messages. Written by the Pub/Sub task.
    pub pubsub: Arc<Mutex<PubSubStore>>,
}

/// Redis's default `databases` setting.
pub const DEFAULT_DB_COUNT: usize = 16;

impl Default for SharedState {
    fn default() -> Self {
        Self {
            info: Arc::new(Mutex::new(None)),
            info_seq: Arc::new(AtomicU64::new(0)),
            keys: Arc::new(Mutex::new(None)),
            page_rows: Arc::new(Mutex::new(None)),
            keyspace_state: Arc::new(Mutex::new(KeyspaceState::default())),
            value_state: Arc::new(Mutex::new(ValueState::default())),
            selected_value: Arc::new(Mutex::new(None)),
            metrics: Arc::new(Mutex::new(MetricsHistory::new(
                config::get().dashboard.history(),
            ))),
            db: Arc::new(Mutex::new(0)),
            db_count: Arc::new(Mutex::new(DEFAULT_DB_COUNT)),
            yank: Arc::new(Mutex::new(None)),
            pubsub: Arc::new(Mutex::new(PubSubStore::new(config::get().pubsub.limits()))),
        }
    }
}

/// What the widgets read from [`SharedState`], copied once per frame by
/// `App::draw` under short locks, so no widget locks anything while drawing.
/// Owned by the App and updated in place: the footer text is rebuilt only
/// when a new `INFO` sample lands or the DB changes.
#[derive(Default)]
pub struct FrameData {
    /// Footer text for the current `(info_seq, db)`.
    pub footer: FooterText,
    /// The server runs in cluster mode.
    pub cluster: bool,
    /// Metrics window for the dashboard's width; `None` outside the
    /// dashboard and while it is paused (it draws its frozen copy then).
    pub metrics: Option<MetricsHistory>,
    /// The Pub/Sub tab's rows; `None` outside the tab. Taken by the widget
    /// when it draws.
    pub pubsub: Option<PubSubView>,
}

impl FrameData {
    /// Rebuild the footer text if a new `INFO` sample landed or the DB
    /// changed since the last frame. `info_seq` is bumped by the info task
    /// after each sample. Returns whether the text changed.
    pub fn refresh_footer(&mut self, state: &SharedState) -> bool {
        let key = (state.info_seq.load(Ordering::Acquire), *state.db.lock());
        if self.footer.key() == Some(key) {
            return false;
        }
        let footer = {
            let info = state.info.lock();
            let metrics = state.metrics.lock();
            FooterText::build(key, info.as_ref(), &metrics)
        };
        let changed = !footer.same_text(&self.footer);
        self.footer = footer;
        changed
    }
}
