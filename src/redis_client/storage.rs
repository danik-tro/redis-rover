use redis::aio::ConnectionManager;
use redis::{FromRedisValue, ScanOptions};

use super::{
    client::{self, fetch_value},
    types::{ItemDelete, ItemEdit, KeyItem, KeyMeta, KeyValue, NewKeySpec, RedisType},
};

#[derive(Clone)]
pub struct Storage {
    manager: ConnectionManager,
}

impl Storage {
    pub fn new(manager: ConnectionManager) -> Self {
        Self { manager }
    }

    /// Run a single `SCAN cursor MATCH pattern COUNT count` step and return the
    /// next cursor plus the keys in this batch (possibly empty, possibly
    /// repeating keys from earlier batches).
    ///
    /// A single step rather than `AsyncCommands::scan_match`: the latter always
    /// restarts at cursor 0 and drains the whole keyspace, while the sweep only
    /// advances as far as the current page needs.
    ///
    /// # Errors
    ///
    /// Returns an error if SCAN fails or replies with an unexpected shape.
    pub async fn scan_batch(
        &self,
        cursor: usize,
        pattern: Option<&str>,
        count: usize,
    ) -> Result<(usize, Vec<String>), Box<dyn std::error::Error + Sync + Send>> {
        let mut manager = self.manager.clone();
        let options = ScanOptions::default()
            .with_pattern(pattern.unwrap_or("*"))
            .with_count(count);
        let batch = redis::cmd("SCAN")
            .arg(cursor)
            .arg(options)
            .query_async(&mut manager)
            .await?;
        Ok(batch)
    }

    /// Fetch TYPE / MEMORY USAGE / TTL for every key in a single pipeline.
    ///
    /// # Errors
    ///
    /// Returns an error if the pipeline fails or any reply has an unexpected
    /// shape.
    pub async fn fetch_meta(
        &self,
        keys: &[String],
    ) -> Result<Vec<KeyMeta>, Box<dyn std::error::Error + Sync + Send>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }

        let mut pipe = redis::pipe();
        for key in keys {
            pipe.cmd("TYPE")
                .arg(key)
                .cmd("MEMORY")
                .arg("USAGE")
                .arg(key)
                .cmd("TTL")
                .arg(key);
        }
        let mut manager = self.manager.clone();
        let raw: Vec<redis::Value> = pipe.query_async(&mut manager).await?;

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
                key: key.clone(),
                r_type: RedisType::from(r_type),
                size: size.unwrap_or_default(),
                ttl,
                value: KeyValue::Unknown,
            });
        }

        Ok(metas)
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

    /// Edit a single collection element in place. Delegates to
    /// [`client::edit_item`].
    ///
    /// # Errors
    ///
    /// Returns an error if the edit is rejected (e.g. duplicate SET member) or
    /// the underlying Redis command fails.
    pub async fn edit_item(
        &self,
        key: &str,
        edit: &ItemEdit,
    ) -> Result<(), Box<dyn std::error::Error + Sync + Send>> {
        client::edit_item(self.manager.clone(), key, edit).await
    }

    /// Delete a single collection element. Delegates to [`client::delete_item`].
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying Redis command fails.
    pub async fn delete_item(
        &self,
        key: &str,
        delete: &ItemDelete,
    ) -> Result<(), Box<dyn std::error::Error + Sync + Send>> {
        client::delete_item(self.manager.clone(), key, delete).await
    }
}
