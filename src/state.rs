use std::sync::{Arc, Mutex};

use crate::redis_client::types::RedisInfo;

#[derive(Clone, Debug)]
pub struct SharedState {
    pub info: Arc<Mutex<Option<RedisInfo>>>,
    pub keys: Arc<Mutex<Vec<String>>>,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            info: Arc::new(Mutex::new(None)),
            keys: Arc::new(Mutex::new(Vec::new())),
        }
    }
}
