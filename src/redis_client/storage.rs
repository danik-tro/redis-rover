use redis::aio::ConnectionManager;
use redis::FromRedisValue;

use super::{
    client::{self, fetch_value},
    types::{KeyItem, KeyMeta, KeyValue, KeysList, NewKeySpec, RedisType},
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

    /// Overwrite a STRING key. Delegates to [`client::set_string`].
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying Redis command fails.
    pub async fn set_string(
        &self,
        key: &str,
        value: &str,
    ) -> Result<(), Box<dyn std::error::Error + Sync + Send>> {
        client::set_string(self.manager.clone(), key, value).await
    }

    /// Delete a key. Delegates to [`client::delete_key`].
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying Redis command fails.
    pub async fn delete_key(
        &self,
        key: &str,
    ) -> Result<(), Box<dyn std::error::Error + Sync + Send>> {
        client::delete_key(self.manager.clone(), key).await
    }

    /// Set or clear a key's TTL. Delegates to [`client::set_ttl`].
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying Redis command fails.
    pub async fn set_ttl(
        &self,
        key: &str,
        secs: i64,
    ) -> Result<(), Box<dyn std::error::Error + Sync + Send>> {
        client::set_ttl(self.manager.clone(), key, secs).await
    }

    /// Create a new key, rejecting the write if the key already exists. Delegates
    /// to [`client::create_key`] after a [`client::key_exists`] guard.
    ///
    /// # Errors
    ///
    /// Returns an error if the key already exists or the underlying Redis
    /// command fails.
    pub async fn create_key(
        &self,
        key: &str,
        spec: &NewKeySpec,
    ) -> Result<(), Box<dyn std::error::Error + Sync + Send>> {
        if client::key_exists(self.manager.clone(), key).await? {
            return Err(format!("Key '{key}' already exists").into());
        }
        client::create_key(self.manager.clone(), key, spec).await
    }

    /// Append a single element to an existing collection. Delegates to
    /// [`client::add_item`].
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying Redis command fails.
    pub async fn add_item(
        &self,
        key: &str,
        item: &KeyItem,
    ) -> Result<(), Box<dyn std::error::Error + Sync + Send>> {
        client::add_item(self.manager.clone(), key, item).await
    }
}
