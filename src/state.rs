use std::sync::Arc;

use parking_lot::Mutex;

use crate::{
    config,
    redis_client::{
        metrics::MetricsHistory,
        types::{KeyMeta, KeyValue, KeyspaceState, RedisInfo},
    },
};

#[derive(Clone, Debug)]
pub struct SharedState {
    pub info: Arc<Mutex<Option<RedisInfo>>>,
    pub keys: Arc<Mutex<Vec<KeyMeta>>>,
    pub keyspace_state: Arc<Mutex<KeyspaceState>>,
    pub selected_value: Arc<Mutex<Option<KeyValue>>>,
    pub metrics: Arc<Mutex<MetricsHistory>>,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            info: Arc::new(Mutex::new(None)),
            keys: Arc::new(Mutex::new(Vec::new())),
            keyspace_state: Arc::new(Mutex::new(KeyspaceState::default())),
            selected_value: Arc::new(Mutex::new(None)),
            metrics: Arc::new(Mutex::new(MetricsHistory::new(
                config::get().dashboard.history(),
            ))),
        }
    }
}
