use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use redis::aio::ConnectionManager;
use redis::{FromRedisValue, ScanOptions};

use super::{
    client,
    pubsub_store::ServerChannels,
    types::{
        ItemDelete, ItemEdit, KeyItem, KeyMeta, KeyValue, LoadedValue, NewKeySpec, RedisType,
        SweepRequest, SweepResult, ValueState,
    },
};

/// `SCAN ... COUNT` hint for the match counter: bigger steps than browsing,
/// since it walks the whole keyspace.
const COUNT_SCAN_HINT: usize = 1000;

/// Most server channels listed in the Pub/Sub tab (and asked for in `NUMSUB`).
const PUBSUB_CHANNELS_MAX: usize = 1000;

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

    /// Answer a [`SweepRequest`]: run `SCAN` steps from its cursor until they
    /// returned at least `need` keys or the sweep ended. Keys are returned raw
    /// (possibly repeating earlier ones); the caller merges them.
    ///
    /// # Errors
    ///
    /// Returns an error if a SCAN step fails.
    pub async fn scan_until(
        &self,
        request: &SweepRequest,
    ) -> Result<SweepResult, Box<dyn std::error::Error + Sync + Send>> {
        let mut result = SweepResult {
            cursor: request.cursor,
            batch: Vec::new(),
        };
        loop {
            let (cursor, keys) = self
                .scan_batch(
                    result.cursor,
                    request.pattern.as_deref(),
                    request.scan_count,
                )
                .await?;
            result.cursor = cursor;
            result.batch.extend(keys);
            if result.done() || result.batch.len() >= request.need {
                return Ok(result);
            }
        }
    }

    /// Count the keys matching `pattern` with a full `SCAN` sweep, stopping
    /// early after `max_steps` steps or once `budget` has passed. Returns the
    /// count and whether the sweep finished (if not, the count is a lower
    /// bound). An empty or `*` pattern uses `DBSIZE`, which is exact.
    ///
    /// # Errors
    ///
    /// Returns an error if `SCAN` / `DBSIZE` fails.
    pub async fn count_matches(
        &self,
        pattern: &str,
        max_steps: usize,
        budget: Duration,
    ) -> Result<(usize, bool), Box<dyn std::error::Error + Sync + Send>> {
        if pattern.is_empty() || pattern == "*" {
            let mut manager = self.manager.clone();
            let total: usize = redis::cmd("DBSIZE").query_async(&mut manager).await?;
            return Ok((total, true));
        }
        let started = Instant::now();
        let mut seen = HashSet::new();
        let mut cursor = 0;
        for _ in 0..max_steps {
            let (next, batch) = self
                .scan_batch(cursor, Some(pattern), COUNT_SCAN_HINT)
                .await?;
            seen.extend(batch);
            if next == 0 {
                return Ok((seen.len(), true));
            }
            cursor = next;
            if started.elapsed() >= budget {
                break;
            }
        }
        Ok((seen.len(), false))
    }

    /// Number of logical databases (`CONFIG GET databases`). `None` if the
    /// reply has no usable value.
    ///
    /// # Errors
    ///
    /// Returns an error if `CONFIG GET` fails (often disabled on managed Redis).
    pub async fn database_count(
        &self,
    ) -> Result<Option<usize>, Box<dyn std::error::Error + Sync + Send>> {
        let mut manager = self.manager.clone();
        let reply: Vec<String> = redis::cmd("CONFIG")
            .arg("GET")
            .arg("databases")
            .query_async(&mut manager)
            .await?;
        Ok(reply
            .get(1)
            .and_then(|v| v.parse().ok())
            .filter(|&n: &usize| n > 0))
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

    /// Fetch the current page of the selected value. Delegates to
    /// [`client::fetch_value_page`].
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying Redis command fails.
    pub async fn fetch_value_page(
        &self,
        state: ValueState,
    ) -> Result<(ValueState, LoadedValue), Box<dyn std::error::Error + Sync + Send>> {
        client::fetch_value_page(self.manager.clone(), state).await
    }

    /// Fetch a key's whole value for copying. Delegates to
    /// [`client::fetch_full_value`].
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported types or if a Redis command fails.
    pub async fn fetch_full_value(
        &self,
        key: &str,
        r_type: &RedisType,
    ) -> Result<KeyValue, Box<dyn std::error::Error + Sync + Send>> {
        client::fetch_full_value(self.manager.clone(), key, r_type).await
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

    /// Active channels of the server: the first [`PUBSUB_CHANNELS_MAX`] names
    /// of `PUBSUB CHANNELS` (sorted) with their `PUBSUB NUMSUB` counts, plus
    /// `PUBSUB NUMPAT`. In cluster mode this is the connected node's view.
    ///
    /// # Errors
    ///
    /// Returns an error if a `PUBSUB` command fails.
    pub async fn pubsub_channels(
        &self,
    ) -> Result<ServerChannels, Box<dyn std::error::Error + Sync + Send>> {
        let mut manager = self.manager.clone();
        let mut names: Vec<String> = redis::cmd("PUBSUB")
            .arg("CHANNELS")
            .query_async(&mut manager)
            .await?;
        let total = names.len();
        names.sort_unstable();
        names.truncate(PUBSUB_CHANNELS_MAX);

        let channels = if names.is_empty() {
            Vec::new()
        } else {
            redis::cmd("PUBSUB")
                .arg("NUMSUB")
                .arg(&names)
                .query_async(&mut manager)
                .await?
        };
        let patterns: u64 = redis::cmd("PUBSUB")
            .arg("NUMPAT")
            .query_async(&mut manager)
            .await?;
        Ok(ServerChannels {
            channels,
            total,
            patterns,
        })
    }

    /// `PUBLISH channel payload`; returns how many subscribers got it.
    ///
    /// # Errors
    ///
    /// Returns an error if the command fails.
    pub async fn publish(
        &self,
        channel: &str,
        payload: &str,
    ) -> Result<i64, Box<dyn std::error::Error + Sync + Send>> {
        let mut manager = self.manager.clone();
        Ok(redis::cmd("PUBLISH")
            .arg(channel)
            .arg(payload)
            .query_async(&mut manager)
            .await?)
    }
}
