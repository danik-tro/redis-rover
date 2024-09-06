use std::sync::{Arc, Mutex};

use crate::redis_client::types::{KeyMeta, KeyspaceState, RedisInfo};

#[derive(Clone, Debug)]
pub struct SharedState {
    pub info: Arc<Mutex<Option<RedisInfo>>>,
    pub keys: Arc<Mutex<Vec<KeyMeta>>>,
    pub keyspace_state: Arc<Mutex<KeyspaceState>>,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            info: Arc::new(Mutex::new(None)),
            keys: Arc::new(Mutex::new(Vec::new())),
            keyspace_state: Arc::new(Mutex::new(KeyspaceState::default())),
        }
    }
}
