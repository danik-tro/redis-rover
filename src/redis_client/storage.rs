use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use redis::aio::ConnectionManager;
use redis::{FromRedisValue, ScanOptions};

use super::{
    bytes::{strings, Raw, Text},
    client,
    pubsub_store::ServerChannels,
    slowlog::{parse_config_int, parse_entries, SlowlogData, SLOWLOG_FETCH},
    types::{
        ItemDelete, ItemEdit, KeyItem, KeyMeta, KeyValue, LoadedValue, NewKeySpec, RedisType,
        SweepRequest, SweepResult, Ttl, TypeScan, ValueState,
    },
};

/// The server rejected `SCAN … TYPE` (it predates Redis 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypeScanRejected;

impl std::fmt::Display for TypeScanRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the server doesn't support SCAN ... TYPE")
    }
}

impl std::error::Error for TypeScanRejected {}

/// `SCAN cursor MATCH pattern COUNT count [TYPE t]`.
fn scan_cmd(
    cursor: u64,
    pattern: Option<&str>,
    count: usize,
    scan_type: Option<&str>,
) -> redis::Cmd {
    let mut options = ScanOptions::default()
        .with_pattern(pattern.unwrap_or("*"))
        .with_count(count);
    if let Some(scan_type) = scan_type {
        options = options.with_type(scan_type);
    }
    let mut cmd = redis::cmd("SCAN");
    cmd.arg(cursor).arg(options);
    cmd
}

/// Most `SCAN` steps one [`Storage::scan_until`] call runs.
const SCAN_SLICE_STEPS: usize = 50;
/// How long one [`Storage::scan_until`] call keeps starting new steps.
const SCAN_SLICE_TIME: Duration = Duration::from_millis(200);
/// Upper end of the `COUNT` hint while steps find nothing.
const SCAN_COUNT_MAX: usize = 10_000;

/// `SCAN ... COUNT` hint for the match counter: bigger steps than browsing,
/// since it walks the whole keyspace.
const COUNT_SCAN_HINT: usize = 1000;

/// Most server channels listed in the Pub/Sub tab (and asked for in `NUMSUB`).
const PUBSUB_CHANNELS_MAX: usize = 1000;

#[derive(Clone)]
pub struct Storage {
    manager: ConnectionManager,
    /// The server refused `MEMORY USAGE` (an ACL without it, a managed
    /// service or a fork that lacks it): stop asking on this connection and
    /// show sizes as unknown. Shared by the clones of this `Storage`.
    memory_usage_refused: Arc<AtomicBool>,
}

impl Storage {
    pub fn new(manager: ConnectionManager) -> Self {
        Self {
            manager,
            memory_usage_refused: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Run a single `SCAN cursor MATCH pattern COUNT count [TYPE t]` step and
    /// return the next cursor plus the keys in this batch (possibly empty,
    /// possibly repeating keys from earlier batches).
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
        cursor: u64,
        pattern: Option<&str>,
        count: usize,
        scan_type: Option<&str>,
    ) -> redis::RedisResult<(u64, Vec<String>)> {
        let mut manager = self.manager.clone();
        // Key names are bytes: one that isn't UTF-8 must not fail the page.
        let (cursor, keys): (u64, Vec<Text>) = scan_cmd(cursor, pattern, count, scan_type)
            .query_async(&mut manager)
            .await?;
        Ok((cursor, strings(keys)))
    }

    /// Answer a [`SweepRequest`]: run `SCAN` steps from its cursor until they
    /// returned at least `need` keys, the sweep ended, or the slice's budget
    /// ([`SCAN_SLICE_STEPS`] steps or [`SCAN_SLICE_TIME`]) is spent; the
    /// caller asks again for the rest, so a pattern that matches little never
    /// keeps one call busy for the whole keyspace (#80). While steps come
    /// back empty the `COUNT` hint grows tenfold, up to [`SCAN_COUNT_MAX`]. Keys are returned raw
    /// (possibly repeating earlier ones); the caller merges them. A type
    /// filter is applied by the server (`TYPE t`) or, for several types, by
    /// asking `TYPE` for each batch and keeping the matching keys.
    ///
    /// # Errors
    ///
    /// Returns [`TypeScanRejected`] if the server doesn't know `SCAN … TYPE`
    /// (before Redis 6), and any other error if a step fails.
    pub async fn scan_until(
        &self,
        request: &SweepRequest,
    ) -> Result<SweepResult, Box<dyn std::error::Error + Sync + Send>> {
        let mut result = SweepResult {
            cursor: request.cursor,
            batch: Vec::new(),
            checked: 0,
        };
        let started = Instant::now();
        let mut count = request.scan_count;
        let mut steps = 0;
        let scan_type = match &request.types {
            TypeScan::Server(scan_type) => Some(scan_type.as_str()),
            TypeScan::All | TypeScan::Client(_) => None,
        };
        loop {
            let step = self
                .scan_batch(result.cursor, request.pattern.as_deref(), count, scan_type)
                .await;
            let (cursor, keys) = match step {
                Ok(step) => step,
                // A server reply error (not I/O): the server can't do TYPE.
                Err(err) if scan_type.is_some() && err.code().is_some() => {
                    log::info!("SCAN TYPE rejected ({err}); filtering types on the client");
                    return Err(Box::new(TypeScanRejected));
                }
                Err(err) => return Err(err.into()),
            };
            result.cursor = cursor;
            result.checked += count as u64;
            steps += 1;
            let found_before = result.batch.len();
            match &request.types {
                TypeScan::Client(filter) => {
                    let types = self.fetch_types(&keys).await?;
                    result.batch.extend(
                        keys.into_iter()
                            .zip(types)
                            .filter(|(_, r_type)| filter.matches(r_type))
                            .map(|(key, _)| key),
                    );
                }
                TypeScan::All | TypeScan::Server(_) => result.batch.extend(keys),
            }
            if result.done()
                || result.batch.len() >= request.need
                || steps >= SCAN_SLICE_STEPS
                || started.elapsed() >= SCAN_SLICE_TIME
            {
                return Ok(result);
            }
            if result.batch.len() == found_before {
                count = count.saturating_mul(10).min(SCAN_COUNT_MAX.max(count));
            }
        }
    }

    /// `TYPE` of every key, in one pipeline (`none` for a key gone meanwhile).
    ///
    /// # Errors
    ///
    /// Returns an error if the pipeline fails.
    pub async fn fetch_types(&self, keys: &[String]) -> redis::RedisResult<Vec<String>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let mut pipe = redis::pipe();
        for key in keys {
            pipe.cmd("TYPE").arg(Raw(key));
        }
        let mut manager = self.manager.clone();
        pipe.query_async(&mut manager).await
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
                .scan_batch(cursor, Some(pattern), COUNT_SCAN_HINT, None)
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
    /// A server that refuses `MEMORY USAGE` still lists its keys: the size
    /// is left unknown (0) and, after the first refusal, no longer asked
    /// for on this connection. `TYPE` and `TTL` errors stay errors.
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

        let with_size = !self.memory_usage_refused.load(Ordering::Relaxed);
        let mut pipe = redis::pipe();
        // Each reply comes back on its own, a refused command as
        // `Value::ServerError`, instead of one error for the whole page.
        pipe.ignore_errors();
        for key in keys {
            pipe.cmd("TYPE").arg(Raw(key));
            if with_size {
                pipe.cmd("MEMORY").arg("USAGE").arg(Raw(key));
            }
            pipe.cmd("TTL").arg(Raw(key));
        }
        let mut manager = self.manager.clone();
        let raw: Vec<redis::Value> = pipe.query_async(&mut manager).await?;

        let mut metas = Vec::with_capacity(keys.len());
        let mut iter = raw.into_iter();
        for key in keys {
            let r_type_val = iter.next().ok_or("pipeline response truncated")?;
            let size_val = if with_size {
                iter.next().ok_or("pipeline response truncated")?
            } else {
                redis::Value::Nil
            };
            let ttl_val = iter.next().ok_or("pipeline response truncated")?;

            let r_type = String::from_redis_value(r_type_val)?;
            let size = match size_val {
                redis::Value::ServerError(err) => {
                    if !self.memory_usage_refused.swap(true, Ordering::Relaxed) {
                        log::warn!(
                            "MEMORY USAGE refused ({}); sizes are not shown",
                            redis::RedisError::from(err)
                        );
                    }
                    None
                }
                value => Option::<u128>::from_redis_value(value)?,
            };
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

    /// A consumer group's consumers and pending entries.
    pub async fn stream_group(&self, key: &str, group: &str) -> super::types::GroupDetail {
        client::fetch_stream_group(self.manager.clone(), key, group).await
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
        ttl: Ttl,
    ) -> Result<(), Box<dyn std::error::Error + Sync + Send>> {
        client::set_ttl(self.manager.clone(), key, ttl).await
    }

    /// The connection, for the `client` helpers of the power edits.
    #[must_use]
    pub fn manager(&self) -> ConnectionManager {
        self.manager.clone()
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
        let names: Vec<Text> = redis::cmd("PUBSUB")
            .arg("CHANNELS")
            .query_async(&mut manager)
            .await?;
        let mut names = strings(names);
        let total = names.len();
        names.sort_unstable();
        names.truncate(PUBSUB_CHANNELS_MAX);

        let channels = if names.is_empty() {
            Vec::new()
        } else {
            let args: Vec<Raw> = names.iter().map(|name| Raw(name)).collect();
            let counts: Vec<(Text, u64)> = redis::cmd("PUBSUB")
                .arg("NUMSUB")
                .arg(args)
                .query_async(&mut manager)
                .await?;
            counts
                .into_iter()
                .map(|(name, count)| (name.0, count))
                .collect()
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
    pub async fn slowlog_reset(&self) -> redis::RedisResult<()> {
        let mut manager = self.manager.clone();
        for cmd in super::write_cmd::slowlog_reset() {
            let _: redis::Value = cmd.query_async(&mut manager).await?;
        }
        Ok(())
    }

    /// The slowlog and its settings. Never fails: managed services often
    /// refuse `CONFIG` or `SLOWLOG`, and the screen says so inline.
    pub async fn slowlog(&self) -> SlowlogData {
        let mut manager = self.manager.clone();
        let reply = redis::cmd("SLOWLOG")
            .arg("GET")
            .arg(SLOWLOG_FETCH)
            .query_async::<redis::Value>(&mut manager)
            .await;
        let (entries, error) = match reply {
            Ok(reply) => (parse_entries(reply), None),
            Err(err) => (Vec::new(), Some(err.to_string())),
        };
        let mut setting = async |name: &str| {
            redis::cmd("CONFIG")
                .arg("GET")
                .arg(name)
                .query_async::<redis::Value>(&mut manager)
                .await
                .ok()
                .and_then(parse_config_int)
        };
        let slower_than = setting("slowlog-log-slower-than").await;
        let max_len = setting("slowlog-max-len").await;
        SlowlogData {
            entries,
            slower_than,
            max_len,
            error,
        }
    }

    pub async fn spublish(&self, channel: &str, payload: &str) -> redis::RedisResult<i64> {
        let mut manager = self.manager.clone();
        let mut receivers = 0;
        for cmd in super::write_cmd::spublish(channel, payload) {
            receivers = cmd.query_async(&mut manager).await?;
        }
        Ok(receivers)
    }

    pub async fn publish(
        &self,
        channel: &str,
        payload: &str,
    ) -> Result<i64, Box<dyn std::error::Error + Sync + Send>> {
        let mut manager = self.manager.clone();
        let mut receivers = 0;
        for cmd in super::write_cmd::publish(channel, payload) {
            receivers = cmd.query_async(&mut manager).await?;
        }
        Ok(receivers)
    }
}

#[cfg(test)]
mod tests {
    use super::scan_cmd;

    fn args(cmd: &redis::Cmd) -> Vec<String> {
        cmd.args_iter()
            .map(|arg| match arg {
                redis::Arg::Simple(bytes) => String::from_utf8_lossy(bytes).into_owned(),
                _ => "<cursor>".into(),
            })
            .collect()
    }

    #[test]
    fn scan_adds_type_only_when_asked() {
        assert_eq!(
            args(&scan_cmd(7, Some("user:*"), 100, None)),
            ["SCAN", "7", "MATCH", "user:*", "COUNT", "100"]
        );
        assert_eq!(
            args(&scan_cmd(0, None, 100, Some("hash"))),
            ["SCAN", "0", "MATCH", "*", "COUNT", "100", "TYPE", "hash"]
        );
    }
}
