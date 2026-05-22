use redis::aio::ConnectionManager;
use redis::FromRedisValue;

use super::{
    client::fetch_value,
    types::{KeyMeta, KeyValue, KeysList, RedisType},
};

pub struct FetchKeysWithMeta<'a> {
    manager: ConnectionManager,
    cursor: Option<usize>,
    pattern: Option<&'a str>,
}

impl<'a> FetchKeysWithMeta<'a> {
    pub fn new(manager: ConnectionManager) -> Self {
        Self {
            manager,
            cursor: None,
            pattern: None,
        }
    }

    #[must_use]
    pub fn cursor(mut self, cursor: Option<usize>) -> Self {
        self.cursor = cursor;
        self
    }

    #[must_use]
    pub fn pattern(mut self, pattern: Option<&'a str>) -> Self {
        self.pattern = pattern;
        self
    }

    /// Run a single SCAN followed by a TYPE / MEMORY USAGE / TTL
    /// pipeline for every key in the batch.
    ///
    /// # Errors
    ///
    /// Returns an error if SCAN fails, the pipeline fails, or any
    /// pipeline reply has an unexpected shape.
    pub async fn execute(mut self) -> Result<KeysList, Box<dyn std::error::Error + Sync + Send>> {
        let cursor = self.cursor.unwrap_or_default();
        let pattern = self.pattern.unwrap_or("*");
        let (cursor, keys): (usize, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(pattern)
            .query_async(&mut self.manager)
            .await?;

        if keys.is_empty() {
            return Ok(KeysList::Empty);
        }

        // Single pipeline: TYPE / MEMORY USAGE / TTL for each key in the batch.
        let mut pipe = redis::pipe();
        for key in &keys {
            pipe.cmd("TYPE")
                .arg(key)
                .cmd("MEMORY")
                .arg("USAGE")
                .arg(key)
                .cmd("TTL")
                .arg(key);
        }
        let raw: Vec<redis::Value> = pipe.query_async(&mut self.manager).await?;

        let mut metas = Vec::with_capacity(keys.len());
        let mut iter = raw.into_iter();
        for key in keys {
            let r_type_val = iter.next().ok_or("pipeline response truncated")?;
            let size_val = iter.next().ok_or("pipeline response truncated")?;
            let ttl_val = iter.next().ok_or("pipeline response truncated")?;

            let r_type = String::from_redis_value(r_type_val)?;
            let size = Option::<u128>::from_redis_value(size_val)?;
            let ttl = isize::from_redis_value(ttl_val)?;

            metas.push(KeyMeta {
                key,
                r_type: RedisType::from(r_type),
                size: size.unwrap_or_default(),
                ttl,
                value: KeyValue::Unknown,
            });
        }

        Ok(KeysList::Keys {
            cursor,
            keys: metas,
        })
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

    pub fn fetch_keys_with_meta(&self) -> FetchKeysWithMeta<'_> {
        FetchKeysWithMeta::new(self.manager.clone())
    }

    /// Fetch the value for a key. Delegates to [`fetch_value`].
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying Redis command fails.
    pub async fn fetch_value(
        &self,
        key: &str,
        r_type: RedisType,
    ) -> Result<KeyValue, Box<dyn std::error::Error + Sync + Send>> {
        fetch_value(self.manager.clone(), key, r_type).await
    }
}
