use futures::future::join_all;
use redis::aio::ConnectionManager;

use super::{
    client::fetch_meta,
    types::{KeyMeta, KeysList},
};

pub struct FetchKeysWithMeta<'a> {
    manager: redis::aio::ConnectionManager,
    size: Option<usize>,
    cursor: Option<usize>,
    pattern: Option<&'a str>,
}

impl<'a> FetchKeysWithMeta<'a> {
    pub fn new(manager: redis::aio::ConnectionManager) -> Self {
        Self {
            manager,
            cursor: None,
            pattern: None,
            size: None,
        }
    }

    pub fn size(mut self, size: Option<usize>) -> Self {
        self.size = size;
        self
    }

    pub fn cursor(mut self, cursor: Option<usize>) -> Self {
        self.cursor = cursor;
        self
    }

    pub fn pattern(mut self, pattern: Option<&'a str>) -> Self {
        self.pattern = pattern;
        self
    }

    pub async fn execute(mut self) -> Result<KeysList, Box<dyn std::error::Error + Sync + Send>> {
        let cursor = self.cursor.unwrap_or_default();
        let pattern = self.pattern.unwrap_or_else(|| "*");
        let (cursor, keys): (usize, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(pattern)
            .query_async(&mut self.manager)
            .await?;

        if keys.is_empty() {
            return Ok(KeysList::Empty);
        }

        let keys = join_all(keys.iter().map(|key| fetch_meta(self.manager.clone(), key)))
            .await
            .into_iter()
            .collect::<Result<_, _>>()?;

        Ok(KeysList::Keys { cursor, keys })
    }
}

#[derive(Clone)]
pub struct Storage {
    manager: ConnectionManager,
}

impl Storage {
    pub fn new(manager: ConnectionManager) -> Self {
        Self { manager }
    }

    pub fn fetch_keys_with_meta(&self) -> FetchKeysWithMeta {
        FetchKeysWithMeta::new(self.manager.clone())
    }
}
