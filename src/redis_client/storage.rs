use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use futures::future::{join_all, try_join_all};
use parking_lot::Mutex;
use redis::{FromRedisValue, ScanOptions};

use super::{
    bytes::{strings, Raw, Text},
    client,
    cluster::{group_by_node, merge_groups, parse_shards, parse_slots, MasterNode},
    conn::Conn,
    metrics::InfoSnapshot,
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

/// One `SCAN` step on `conn`.
async fn scan_step(
    conn: &mut Conn,
    cursor: u64,
    pattern: Option<&str>,
    count: usize,
    scan_type: Option<&str>,
) -> redis::RedisResult<(u64, Vec<String>)> {
    // Key names are bytes: one that isn't UTF-8 must not fail the page.
    let (cursor, keys): (u64, Vec<Text>) = scan_cmd(cursor, pattern, count, scan_type)
        .query_async(conn)
        .await?;
    Ok((cursor, strings(keys)))
}

/// `TYPE` of every key on `conn`, in one pipeline (`none` for a key gone
/// meanwhile).
async fn types_on(conn: &mut Conn, keys: &[String]) -> redis::RedisResult<Vec<String>> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let mut pipe = redis::pipe();
    for key in keys {
        pipe.cmd("TYPE").arg(Raw(key));
    }
    pipe.query_async(conn).await
}

/// The masters as one node sees them: `CLUSTER SHARDS`, or `CLUSTER SLOTS`
/// on a server before Redis 7.
async fn masters_from(mut conn: Conn) -> redis::RedisResult<Vec<MasterNode>> {
    let shards = redis::cmd("CLUSTER")
        .arg("SHARDS")
        .query_async::<redis::Value>(&mut conn)
        .await;
    let masters = match shards {
        Ok(reply) => parse_shards(&reply),
        // An older server, or an ACL without it.
        Err(err) if err.code().is_some() => Vec::new(),
        Err(err) => return Err(err),
    };
    if !masters.is_empty() {
        return Ok(masters);
    }
    let reply = redis::cmd("CLUSTER")
        .arg("SLOTS")
        .query_async::<redis::Value>(&mut conn)
        .await?;
    let masters = parse_slots(&reply);
    if masters.is_empty() {
        return Err(redis::RedisError::from((
            redis::ErrorKind::UnexpectedReturnType,
            "the cluster reports no masters",
        )));
    }
    Ok(masters)
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
    manager: Conn,
    /// The server refused `MEMORY USAGE` (an ACL without it, a managed
    /// service or a fork that lacks it): stop asking on this connection and
    /// show sizes as unknown. Shared by the clones of this `Storage`.
    memory_usage_refused: Arc<AtomicBool>,
    /// The cluster's masters (empty until read). Shared with every
    /// connection of the session through [`Self::sharing_masters`], so a
    /// topology change reroutes them all at once.
    masters: Arc<Mutex<Arc<Vec<MasterNode>>>>,
}

impl Storage {
    pub fn new(manager: impl Into<Conn>) -> Self {
        Self {
            manager: manager.into(),
            memory_usage_refused: Arc::new(AtomicBool::new(false)),
            masters: Arc::default(),
        }
    }

    /// Keep the list of masters in `cell` (`SharedState::masters`).
    #[must_use]
    pub fn sharing_masters(mut self, cell: Arc<Mutex<Arc<Vec<MasterNode>>>>) -> Self {
        self.masters = cell;
        self
    }

    /// This storage with everything sent to the node at `host:port`.
    #[must_use]
    pub fn at(&self, host: &str, port: u16) -> Self {
        let mut storage = self.clone();
        storage.manager = self.manager.on_address(host, port);
        storage
    }

    /// This session browses a whole cluster.
    #[must_use]
    pub fn is_cluster(&self) -> bool {
        !matches!(self.manager, Conn::Single(_))
    }

    /// [`Self::masters`], read once. No lock is held across the query.
    ///
    /// # Errors
    ///
    /// As [`Self::masters`].
    pub async fn cached_masters(&self) -> redis::RedisResult<Arc<Vec<MasterNode>>> {
        let known = Arc::clone(&self.masters.lock());
        if !known.is_empty() {
            return Ok(known);
        }
        let masters = Arc::new(self.masters().await?);
        *self.masters.lock() = Arc::clone(&masters);
        Ok(masters)
    }

    /// A connection to the master that owns `slot`, by its address.
    fn master_conn(&self, masters: &[MasterNode], slot: u16) -> Conn {
        masters
            .iter()
            .find(|master| {
                master
                    .slots
                    .iter()
                    .any(|&(start, end)| (start..=end).contains(&slot))
            })
            .map_or_else(
                || self.manager.on_slot(slot),
                |master| self.manager.on_address(&master.host, master.port),
            )
    }

    /// One connection per master, or this connection on one server.
    async fn node_conns(&self) -> redis::RedisResult<Vec<Conn>> {
        if !self.is_cluster() {
            return Ok(vec![self.manager.clone()]);
        }
        let masters = self.cached_masters().await?;
        Ok(masters
            .iter()
            .map(|master| self.manager.on_address(&master.host, master.port))
            .collect())
    }

    /// One `INFO` sample per master, taken at the same time (`None` for a
    /// master that didn't answer).
    ///
    /// # Errors
    ///
    /// The list of masters can't be read.
    pub async fn node_snapshots(&self) -> redis::RedisResult<Vec<(String, Option<InfoSnapshot>)>> {
        let masters = self.cached_masters().await?;
        let samples = masters.iter().map(|master| {
            let mut conn = self.manager.on_address(&master.host, master.port);
            let id = master.id.clone();
            async move {
                let raw = redis::cmd("INFO").query_async::<String>(&mut conn).await;
                let snapshot = raw
                    .ok()
                    .map(|raw| InfoSnapshot::from_map(&client::parse_info(&raw), Instant::now()));
                (id, snapshot)
            }
        });
        Ok(join_all(samples).await)
    }

    /// `DBSIZE`, summed over every master.
    ///
    /// # Errors
    ///
    /// A master can't be asked.
    pub async fn total_keys(&self) -> redis::RedisResult<u64> {
        let conns = self.node_conns().await?;
        let sizes = try_join_all(conns.into_iter().map(|mut conn| async move {
            redis::cmd("DBSIZE").query_async::<u64>(&mut conn).await
        }))
        .await?;
        Ok(sizes.into_iter().sum())
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
        scan_step(&mut manager, cursor, pattern, count, scan_type).await
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
        // On a cluster the cursor only means something on its own master.
        let mut conn = match request.node {
            Some(slot) => {
                let masters = self.cached_masters().await?;
                self.master_conn(&masters, slot)
            }
            None => self.manager.clone(),
        };
        let started = Instant::now();
        let mut count = request.scan_count;
        let mut steps = 0;
        let scan_type = match &request.types {
            TypeScan::Server(scan_type) => Some(scan_type.as_str()),
            TypeScan::All | TypeScan::Client(_) => None,
        };
        loop {
            let step = scan_step(
                &mut conn,
                result.cursor,
                request.pattern.as_deref(),
                count,
                scan_type,
            )
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
                    let types = types_on(&mut conn, &keys).await?;
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

    /// The cluster's masters, in slot order, asked of the home node and
    /// then of each other known master, since any of them may be the one
    /// that just failed.
    ///
    /// # Errors
    ///
    /// No node answers, or none names a master.
    pub async fn masters(&self) -> redis::RedisResult<Vec<MasterNode>> {
        let known = Arc::clone(&self.masters.lock());
        let others = known
            .iter()
            .map(|master| self.manager.on_address(&master.host, master.port));
        let mut last = None;
        for conn in std::iter::once(self.manager.home()).chain(others) {
            match masters_from(conn).await {
                Ok(masters) => return Ok(masters),
                Err(err) => last = Some(err),
            }
        }
        Err(last.unwrap_or_else(|| {
            redis::RedisError::from((
                redis::ErrorKind::UnexpectedReturnType,
                "the cluster reports no masters",
            ))
        }))
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
            let total = self.total_keys().await?;
            return Ok((usize::try_from(total).unwrap_or(usize::MAX), true));
        }
        let started = Instant::now();
        let mut seen = HashSet::new();
        let mut steps = 0;
        // The step and time budgets cover all the masters together.
        for mut conn in self.node_conns().await? {
            let mut cursor = 0;
            loop {
                if steps >= max_steps {
                    return Ok((seen.len(), false));
                }
                steps += 1;
                let (next, batch) =
                    scan_step(&mut conn, cursor, Some(pattern), COUNT_SCAN_HINT, None).await?;
                seen.extend(batch);
                if next == 0 {
                    break;
                }
                cursor = next;
                if started.elapsed() >= budget {
                    return Ok((seen.len(), false));
                }
            }
        }
        Ok((seen.len(), true))
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

    /// Fetch TYPE / MEMORY USAGE / TTL for every key: one pipeline, or on a
    /// cluster one per master, run at the same time and merged in `keys`
    /// order.
    ///
    /// A server that refuses `MEMORY USAGE` still lists its keys: the size
    /// is left unknown (0) and, after the first refusal, no longer asked
    /// for on this connection. `TYPE` and `TTL` errors stay errors.
    ///
    /// # Errors
    ///
    /// Returns an error if a pipeline fails or any reply has an unexpected
    /// shape.
    pub async fn fetch_meta(
        &self,
        keys: &[String],
    ) -> Result<Vec<KeyMeta>, Box<dyn std::error::Error + Sync + Send>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        if !self.is_cluster() {
            return self.meta_on(self.manager.clone(), keys).await;
        }
        let masters = self.cached_masters().await?;
        let groups = group_by_node(keys, &masters);
        let replies = try_join_all(groups.iter().map(|group| {
            let names: Vec<&String> = group.indices.iter().map(|&i| &keys[i]).collect();
            let conn = self.master_conn(&masters, group.slot);
            async move { self.meta_on(conn, &names).await }
        }))
        .await?;
        merge_groups(keys.len(), &groups, replies)
            .ok_or_else(|| "pipeline response truncated".into())
    }

    /// The metadata of `keys`, all on the node `conn` reaches.
    async fn meta_on<K: AsRef<str> + Sync>(
        &self,
        mut manager: Conn,
        keys: &[K],
    ) -> Result<Vec<KeyMeta>, Box<dyn std::error::Error + Sync + Send>> {
        let with_size = !self.memory_usage_refused.load(Ordering::Relaxed);
        let mut pipe = redis::pipe();
        // Each reply comes back on its own, a refused command as
        // `Value::ServerError`, instead of one error for the whole page.
        pipe.ignore_errors();
        for key in keys {
            let key = key.as_ref();
            pipe.cmd("TYPE").arg(Raw(key));
            if with_size {
                pipe.cmd("MEMORY").arg("USAGE").arg(Raw(key));
            }
            pipe.cmd("TTL").arg(Raw(key));
        }
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
                key: key.as_ref().to_owned(),
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
    pub fn manager(&self) -> Conn {
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

    /// `SLOWLOG RESET`.
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

    /// `SPUBLISH channel payload`; returns how many subscribers got it.
    ///
    /// # Errors
    ///
    /// Returns an error if the command fails.
    pub async fn spublish(&self, channel: &str, payload: &str) -> redis::RedisResult<i64> {
        let mut manager = self.manager.clone();
        let mut receivers = 0;
        for cmd in super::write_cmd::spublish(channel, payload) {
            receivers = cmd.query_async(&mut manager).await?;
        }
        Ok(receivers)
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
