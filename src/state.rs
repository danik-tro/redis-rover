use std::sync::Arc;

use parking_lot::Mutex;

use crate::{
    config,
    redis_client::{
        metrics::MetricsHistory,
        pubsub_store::PubSubStore,
        types::{KeyMeta, KeyspaceState, LoadedValue, RedisInfo, ValueState},
    },
};

#[derive(Clone, Debug)]
pub struct SharedState {
    pub info: Arc<Mutex<Option<RedisInfo>>>,
    pub keys: Arc<Mutex<Vec<KeyMeta>>>,
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
            keys: Arc::new(Mutex::new(Vec::new())),
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
