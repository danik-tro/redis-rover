use std::{collections::HashMap, time::Instant};

use color_eyre::eyre::Result;

use redis::{aio::ConnectionManager, AsyncCommands};

use super::metrics::{parse_cluster_info, ClusterSummary, InfoSnapshot};
use super::types::{
    json_path, ItemDelete, ItemEdit, JsonField, JsonPreview, JsonPreviewKind, KeyItem, KeyValue,
    LoadedValue, NewKeySpec, RedisInfo, RedisType, ScanSweep, ValuePageInfo, ValueState,
    JSON_PREVIEW_ITEMS, STRING_PREVIEW_BYTES,
};

/// Elements per command when fetching a whole collection for copying.
const FULL_FETCH_CHUNK: usize = 1000;

/// Sentinel used to delete a LIST element by index (Redis has no delete-by-index):
/// overwrite the slot with this marker via `LSET`, then remove it with `LREM`.
const LIST_DELETE_SENTINEL: &str = "__rrover_deleted__\u{0}";

/// Parse the text of an `INFO` reply into a `field -> value` map. Section
/// headers (`# Server`) and lines without a `:` are skipped; values are trimmed.
pub fn parse_info(raw: &str) -> HashMap<&str, &str> {
    let mut map = HashMap::new();

    for c in raw.split_terminator('\n') {
        if c.starts_with('#') || c.is_empty() {
            continue;
        }

        let Some((header, value)) = c.split_once(':') else {
            continue;
        };

        map.insert(header, value.trim());
    }

    map
}

/// Run `INFO` once and build both the footer view and the numeric snapshot.
///
/// # Errors
///
/// Returns an error if the command fails.
pub async fn redis_info_with_snapshot(
    manager: &mut ConnectionManager,
) -> Result<(RedisInfo, InfoSnapshot)> {
    let raw: String = redis::cmd("INFO").query_async(manager).await?;
    let taken_at = Instant::now();
    let map = parse_info(&raw);
    let snapshot = InfoSnapshot::from_map(&map, taken_at);

    Ok((RedisInfo::from(map), snapshot))
}

/// Run `CLUSTER INFO` and summarise the reply.
///
/// # Errors
///
/// Returns an error if the command fails (e.g. cluster mode is disabled).
pub async fn cluster_info(manager: &mut ConnectionManager) -> Result<ClusterSummary> {
    let raw: String = redis::cmd("CLUSTER")
        .arg("INFO")
        .query_async(manager)
        .await?;

    Ok(parse_cluster_info(&raw))
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Fetch the current page of the selected key's value, as described by
/// `state`. Every command is bounded: STRING reads at most
/// [`STRING_PREVIEW_BYTES`], LIST / ZSET read one `page_size` window, and HASH /
/// SET advance an `HSCAN` / `SSCAN` sweep only as far as the page needs.
///
/// Returns the advanced state (page possibly clamped if the collection shrank)
/// together with the loaded page.
///
/// # Errors
///
/// Returns an error if no key is selected or any Redis command fails.
pub async fn fetch_value_page(
    mut manager: ConnectionManager,
    mut state: ValueState,
) -> Result<(ValueState, LoadedValue), BoxError> {
    let key = state.key.clone().ok_or("no key selected")?;
    let manager = &mut manager;

    let value = match state.r_type {
        RedisType::String => {
            let (value, total_len) =
                fetch_string_preview(manager, &key, STRING_PREVIEW_BYTES).await?;
            state.total = None;
            return Ok((
                state,
                LoadedValue {
                    value: KeyValue::String { value, total_len },
                    page: ValuePageInfo::default(),
                },
            ));
        }
        RedisType::Json => {
            let value = fetch_json(manager, &key, &mut state).await?;
            let len = match &value {
                KeyValue::JsonPreview(JsonPreview {
                    kind: JsonPreviewKind::Object { fields, .. },
                    ..
                }) => fields.len(),
                _ => 0,
            };
            let page = state.page_info(len);
            return Ok((state, LoadedValue { value, page }));
        }
        RedisType::List => {
            let mut items = fetch_list_window(manager, &key, &mut state).await?;
            if refetch_after_clamp(&mut state) {
                items = fetch_list_window(manager, &key, &mut state).await?;
            }
            KeyValue::List(items)
        }
        RedisType::Zset => {
            let mut items = fetch_zset_window(manager, &key, &mut state).await?;
            if refetch_after_clamp(&mut state) {
                items = fetch_zset_window(manager, &key, &mut state).await?;
            }
            KeyValue::Zset(items)
        }
        RedisType::Hash | RedisType::Set => {
            let is_hash = state.r_type == RedisType::Hash;
            let len_cmd = if is_hash { "HLEN" } else { "SCARD" };
            let total: usize = redis::cmd(len_cmd).arg(&key).query_async(manager).await?;
            state.total = Some(total);

            while state.needs_scan() {
                let (cursor, batch) = scan_step(manager, &key, is_hash, &state).await?;
                if is_hash {
                    state.sweep.merge_fields(cursor, pair_up(batch));
                } else {
                    state.sweep.merge_members(cursor, batch);
                }
            }
            state.clamp_page();

            let range = state.page_range();
            let names = &state.sweep.names[range.clone()];
            if is_hash {
                let values = &state.sweep.values[range];
                KeyValue::Hash(names.iter().cloned().zip(values.iter().cloned()).collect())
            } else {
                KeyValue::Set(names.to_vec())
            }
        }
        RedisType::Stream | RedisType::Unknown(_) => KeyValue::Unknown,
    };

    let len = match &value {
        KeyValue::List(items) | KeyValue::Set(items) => items.len(),
        KeyValue::Hash(entries) => entries.len(),
        KeyValue::Zset(entries) => entries.len(),
        KeyValue::String { .. }
        | KeyValue::Json(_)
        | KeyValue::JsonPreview(_)
        | KeyValue::Unknown => 0,
    };
    let page = state.page_info(len);
    Ok((state, LoadedValue { value, page }))
}

/// Load the JSON value `state` points at: the document, or the field
/// `state.json_path` opened. Up to `state.json_cap` bytes it is read whole;
/// above, only its shape (#49):
///
/// - object: its top-level field names (`JSON.OBJKEYS`, once per sweep, kept
///   in `state.sweep.names`), then type and size of the current page's fields;
/// - array: its length and first [`JSON_PREVIEW_ITEMS`] items;
/// - anything else: just its type.
///
/// Uses `$` paths (`RedisJSON` 2.x). The root is read without a path, so a
/// document under the cap loads on `RedisJSON` 1.x too; above the cap, a server
/// that rejects `$` paths gets [`JsonPreviewKind::TooLarge`].
async fn fetch_json(
    manager: &mut ConnectionManager,
    key: &str,
    state: &mut ValueState,
) -> Result<KeyValue, BoxError> {
    state.total = None;
    let at_root = state.json_path.is_empty();
    let path = json_path(&state.json_path);
    let cap = u64::try_from(state.json_cap).unwrap_or(u64::MAX);

    let bytes = if let Some(bytes) = state.size.filter(|_| at_root) {
        bytes
    } else {
        let mut cmd = redis::cmd("JSON.DEBUG");
        cmd.arg("MEMORY").arg(key);
        if !at_root {
            cmd.arg(&path);
        }
        first_u64(&cmd.query_async(manager).await?).unwrap_or(0)
    };

    if bytes <= cap {
        let mut cmd = redis::cmd("JSON.GET");
        cmd.arg(key);
        if !at_root {
            cmd.arg(&path);
        }
        let raw: String = cmd.query_async(manager).await?;
        let mut json: serde_json::Value = serde_json::from_str(&raw)?;
        if !at_root {
            // `$` paths answer with the list of matches.
            json = first_match(json).ok_or("JSON field not found")?;
        }
        return Ok(KeyValue::Json(json));
    }

    let json_type: redis::Value = match redis::cmd("JSON.TYPE")
        .arg(key)
        .arg(&path)
        .query_async(manager)
        .await
    {
        Ok(reply) => reply,
        // RedisJSON 1.x has no `$` paths: show the size only.
        Err(err) if is_server_error(&err) => {
            log::debug!("JSON.TYPE {key} {path} failed, no preview: {err}");
            return Ok(json_preview(
                bytes,
                JsonPreviewKind::TooLarge { json_type: None },
            ));
        }
        Err(err) => return Err(err.into()),
    };
    let json_type = first_string(&json_type).ok_or("JSON value not found")?;

    let kind = match json_type.as_str() {
        "object" => {
            if !state.sweep.done {
                let names: redis::Value = redis::cmd("JSON.OBJKEYS")
                    .arg(key)
                    .arg(&path)
                    .query_async(manager)
                    .await?;
                state.sweep.names = first_strings(&names);
                state.sweep.done = true;
            }
            state.total = Some(state.sweep.names.len());
            state.clamp_page();
            let start = state.page_start().min(state.sweep.names.len());
            let end = (start + state.page_size).min(state.sweep.names.len());
            let names = &state.sweep.names[start..end];
            JsonPreviewKind::Object {
                len: state.sweep.names.len(),
                fields: fetch_json_fields(manager, key, &state.json_path, names).await?,
            }
        }
        "array" => {
            let (len, items): (redis::Value, String) = redis::pipe()
                .cmd("JSON.ARRLEN")
                .arg(key)
                .arg(&path)
                .cmd("JSON.GET")
                .arg(key)
                .arg(format!("{path}[0:{JSON_PREVIEW_ITEMS}]"))
                .query_async(manager)
                .await?;
            JsonPreviewKind::Array {
                len: usize::try_from(first_u64(&len).unwrap_or(0)).unwrap_or(usize::MAX),
                items: serde_json::from_str(&items)?,
            }
        }
        _ => JsonPreviewKind::TooLarge {
            json_type: Some(json_type),
        },
    };
    Ok(json_preview(bytes, kind))
}

fn json_preview(bytes: u64, kind: JsonPreviewKind) -> KeyValue {
    KeyValue::JsonPreview(JsonPreview { bytes, kind })
}

/// Type and size of the fields `names` of the object at `parent`, in one
/// pipeline.
async fn fetch_json_fields(
    manager: &mut ConnectionManager,
    key: &str,
    parent: &[String],
    names: &[String],
) -> Result<Vec<JsonField>, BoxError> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let mut pipe = redis::pipe();
    let mut path = parent.to_vec();
    for name in names {
        path.push(name.clone());
        let field = json_path(&path);
        path.pop();
        pipe.cmd("JSON.TYPE")
            .arg(key)
            .arg(&field)
            .cmd("JSON.DEBUG")
            .arg("MEMORY")
            .arg(key)
            .arg(&field);
    }
    let replies: Vec<redis::Value> = pipe.query_async(manager).await?;
    let mut replies = replies.iter();
    let mut fields = Vec::with_capacity(names.len());
    for name in names {
        let (Some(json_type), Some(bytes)) = (replies.next(), replies.next()) else {
            return Err("pipeline response truncated".into());
        };
        fields.push(JsonField {
            name: name.clone(),
            json_type: first_string(json_type).unwrap_or_else(|| "?".into()),
            bytes: first_u64(bytes).unwrap_or(0),
        });
    }
    Ok(fields)
}

/// The server answered with an error (as opposed to a lost connection).
fn is_server_error(err: &redis::RedisError) -> bool {
    !(err.is_io_error() || err.is_connection_dropped() || err.is_timeout())
}

/// First match of a `$` path reply: the reply itself for a plain value
/// (no path), the first element of a list of matches.
fn first_reply(value: &redis::Value) -> &redis::Value {
    match value {
        redis::Value::Array(items) => items.first().unwrap_or(value),
        other => other,
    }
}

fn first_u64(value: &redis::Value) -> Option<u64> {
    match first_reply(value) {
        redis::Value::Int(n) => u64::try_from(*n).ok(),
        other => <u64 as redis::FromRedisValue>::from_redis_value_ref(other).ok(),
    }
}

fn first_string(value: &redis::Value) -> Option<String> {
    match first_reply(value) {
        redis::Value::Array(_) | redis::Value::Nil => None,
        other => <String as redis::FromRedisValue>::from_redis_value_ref(other).ok(),
    }
}

/// `JSON.OBJKEYS key $` answers `[[name, …]]`; `JSON.OBJKEYS key` `[name, …]`.
fn first_strings(value: &redis::Value) -> Vec<String> {
    let list = match value {
        redis::Value::Array(items) if matches!(items.first(), Some(redis::Value::Array(_))) => {
            &items[0]
        }
        other => other,
    };
    <Vec<String> as redis::FromRedisValue>::from_redis_value_ref(list).unwrap_or_default()
}

/// The first match in a `$` path `JSON.GET` reply (a JSON array of matches).
fn first_match(json: serde_json::Value) -> Option<serde_json::Value> {
    match json {
        serde_json::Value::Array(matches) => matches.into_iter().next(),
        _ => None,
    }
}

/// Fetch a key's whole value, for copying it (#27). Collections are read in
/// chunks of [`FULL_FETCH_CHUNK`] (`LRANGE` / `ZRANGE`, or a full `HSCAN` /
/// `SSCAN` sweep), so no single reply is huge.
///
/// # Errors
///
/// Returns an error for STREAM / unknown types or if a Redis command fails.
pub async fn fetch_full_value(
    mut manager: ConnectionManager,
    key: &str,
    r_type: &RedisType,
) -> Result<KeyValue, BoxError> {
    let manager = &mut manager;
    let value = match r_type {
        RedisType::String => {
            let bytes: Vec<u8> = redis::cmd("GET").arg(key).query_async(manager).await?;
            let total_len = bytes.len();
            KeyValue::String {
                value: String::from_utf8_lossy(&bytes).into_owned(),
                total_len,
            }
        }
        RedisType::Json => {
            let raw: String = redis::cmd("JSON.GET").arg(key).query_async(manager).await?;
            KeyValue::Json(serde_json::from_str(&raw)?)
        }
        RedisType::List => {
            let mut items = Vec::new();
            loop {
                let (start, stop) = chunk_bounds(items.len())?;
                let chunk: Vec<String> = redis::cmd("LRANGE")
                    .arg(key)
                    .arg(start)
                    .arg(stop)
                    .query_async(manager)
                    .await?;
                let done = chunk.len() < FULL_FETCH_CHUNK;
                items.extend(chunk);
                if done {
                    break KeyValue::List(items);
                }
            }
        }
        RedisType::Zset => {
            let mut items = Vec::new();
            loop {
                let (start, stop) = chunk_bounds(items.len())?;
                let chunk: Vec<(String, f64)> = redis::cmd("ZRANGE")
                    .arg(key)
                    .arg(start)
                    .arg(stop)
                    .arg("WITHSCORES")
                    .query_async(manager)
                    .await?;
                let done = chunk.len() < FULL_FETCH_CHUNK;
                items.extend(chunk);
                if done {
                    break KeyValue::Zset(items);
                }
            }
        }
        RedisType::Hash | RedisType::Set => {
            let is_hash = *r_type == RedisType::Hash;
            let mut sweep = ScanSweep::default();
            loop {
                let (cursor, batch): (u64, Vec<String>) =
                    redis::cmd(if is_hash { "HSCAN" } else { "SSCAN" })
                        .arg(key)
                        .arg(sweep.cursor)
                        .arg("COUNT")
                        .arg(FULL_FETCH_CHUNK)
                        .query_async(manager)
                        .await?;
                if is_hash {
                    sweep.merge_fields(cursor, pair_up(batch));
                } else {
                    sweep.merge_members(cursor, batch);
                }
                if sweep.done {
                    break;
                }
            }
            if is_hash {
                KeyValue::Hash(sweep.names.into_iter().zip(sweep.values).collect())
            } else {
                KeyValue::Set(sweep.names)
            }
        }
        RedisType::Stream | RedisType::Unknown(_) => {
            return Err(format!("copying {} values isn't supported yet", r_type.name()).into());
        }
    };
    Ok(value)
}

/// `start` / `stop` (inclusive) of the next [`FULL_FETCH_CHUNK`] elements.
fn chunk_bounds(fetched: usize) -> Result<(isize, isize), BoxError> {
    let start = isize::try_from(fetched)?;
    let len = isize::try_from(FULL_FETCH_CHUNK)?;
    Ok((start, start + len - 1))
}

/// Clamp an index-paged state to the (just fetched) collection length.
/// Returns `true` if the page moved, meaning the window must be fetched again.
fn refetch_after_clamp(state: &mut ValueState) -> bool {
    let before = state.page;
    state.clamp_page();
    state.page != before
}

/// `start` / `stop` (inclusive) arguments for the current page window.
fn window_bounds(state: &ValueState) -> Result<(isize, isize), BoxError> {
    let start = isize::try_from(state.page_start())?;
    let len = isize::try_from(state.page_size.max(1))?;
    Ok((start, start + len - 1))
}

/// `LLEN` + `LRANGE` for the current page, in one pipeline. Stores the length
/// in `state.total`.
async fn fetch_list_window(
    manager: &mut ConnectionManager,
    key: &str,
    state: &mut ValueState,
) -> Result<Vec<String>, BoxError> {
    let (start, stop) = window_bounds(state)?;
    let (total, items): (usize, Vec<String>) = redis::pipe()
        .cmd("LLEN")
        .arg(key)
        .cmd("LRANGE")
        .arg(key)
        .arg(start)
        .arg(stop)
        .query_async(manager)
        .await?;
    state.total = Some(total);
    Ok(items)
}

/// `ZCARD` + `ZRANGE … WITHSCORES` for the current page, in one pipeline.
/// Stores the length in `state.total`.
async fn fetch_zset_window(
    manager: &mut ConnectionManager,
    key: &str,
    state: &mut ValueState,
) -> Result<Vec<(String, f64)>, BoxError> {
    let (start, stop) = window_bounds(state)?;
    let (total, items): (usize, Vec<(String, f64)>) = redis::pipe()
        .cmd("ZCARD")
        .arg(key)
        .cmd("ZRANGE")
        .arg(key)
        .arg(start)
        .arg(stop)
        .arg("WITHSCORES")
        .query_async(manager)
        .await?;
    state.total = Some(total);
    Ok(items)
}

/// One `HSCAN` / `SSCAN key cursor COUNT n` step. HSCAN replies with a flat
/// `field, value, …` list; pair it up with [`pair_up`].
async fn scan_step(
    manager: &mut ConnectionManager,
    key: &str,
    is_hash: bool,
    state: &ValueState,
) -> Result<(u64, Vec<String>), BoxError> {
    let reply = redis::cmd(if is_hash { "HSCAN" } else { "SSCAN" })
        .arg(key)
        .arg(state.sweep.cursor)
        .arg("COUNT")
        .arg(state.scan_count)
        .query_async(manager)
        .await?;
    Ok(reply)
}

/// Turn a flat `field, value, field, value` list into pairs. A trailing odd
/// element (never sent by Redis) is dropped.
fn pair_up(flat: Vec<String>) -> Vec<(String, String)> {
    let mut iter = flat.into_iter();
    let mut pairs = Vec::with_capacity(iter.len() / 2);
    while let (Some(field), Some(value)) = (iter.next(), iter.next()) {
        pairs.push((field, value));
    }
    pairs
}

/// `STRLEN` + `GETRANGE 0 max-1` in one pipeline. Returns the decoded preview
/// and the full length in bytes.
async fn fetch_string_preview(
    manager: &mut ConnectionManager,
    key: &str,
    max: usize,
) -> Result<(String, usize), BoxError> {
    let (total, bytes): (usize, Vec<u8>) = redis::pipe()
        .cmd("STRLEN")
        .arg(key)
        .cmd("GETRANGE")
        .arg(key)
        .arg(0)
        .arg(max.saturating_sub(1))
        .query_async(manager)
        .await?;
    let truncated = bytes.len() < total;
    Ok((decode_preview(&bytes, truncated), total))
}

/// Decode a value preview as UTF-8. When the preview was cut short, a
/// multi-byte character split at the end is dropped instead of shown as `�`.
/// Other invalid bytes are replaced with `�`.
fn decode_preview(bytes: &[u8], truncated: bool) -> String {
    let mut end = bytes.len();
    if truncated {
        if let Err(err) = std::str::from_utf8(bytes) {
            // `error_len() == None` means the input ended mid-character.
            if err.error_len().is_none() {
                end = err.valid_up_to();
            }
        }
    }
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Overwrite a key with a plain string value (`SET`).
///
/// # Errors
///
/// Returns an error if the underlying Redis command fails.
pub async fn set_string(
    mut manager: ConnectionManager,
    key: &str,
    value: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _: () = manager.set(key, value).await?;
    Ok(())
}

/// Delete a key (`DEL`). Type-agnostic — works on any value type.
///
/// # Errors
///
/// Returns an error if the underlying Redis command fails.
pub async fn delete_key(
    mut manager: ConnectionManager,
    key: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _: () = manager.del(key).await?;
    Ok(())
}

/// Set a key-level TTL in seconds (`EXPIRE`). A non-positive `secs` clears the
/// TTL instead (`PERSIST`), making the key permanent. TTL is key-level in Redis
/// and applies to every value type.
///
/// # Errors
///
/// Returns an error if the underlying Redis command fails.
pub async fn set_ttl(
    mut manager: ConnectionManager,
    key: &str,
    secs: i64,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if secs > 0 {
        let _: () = manager.expire(key, secs).await?;
    } else {
        let _: () = manager.persist(key).await?;
    }
    Ok(())
}

/// Return whether a key already exists (`EXISTS`). Used to guard key creation so
/// the wizard never silently clobbers an existing key.
///
/// # Errors
///
/// Returns an error if the underlying Redis command fails.
pub async fn key_exists(
    mut manager: ConnectionManager,
    key: &str,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let exists: bool = manager.exists(key).await?;
    Ok(exists)
}

/// Create a key from a fully-specified [`NewKeySpec`]. STRING uses `SET`; the
/// collection types seed every accumulated element in a single command
/// (`RPUSH` / `SADD` / `HSET` / `ZADD`).
///
/// # Errors
///
/// Returns an error if the underlying Redis command fails.
pub async fn create_key(
    mut manager: ConnectionManager,
    key: &str,
    spec: &NewKeySpec,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match spec {
        NewKeySpec::String(value) => {
            let _: () = manager.set(key, value).await?;
        }
        NewKeySpec::List(items) => {
            let _: () = manager.rpush(key, items).await?;
        }
        NewKeySpec::Set(members) => {
            let _: () = manager.sadd(key, members).await?;
        }
        NewKeySpec::Hash(pairs) => {
            let _: () = manager.hset_multiple(key, pairs).await?;
        }
        NewKeySpec::Zset(pairs) => {
            // redis crate expects (score, member); our pairs are (member, score).
            let scored: Vec<(f64, &str)> = pairs.iter().map(|(m, s)| (*s, m.as_str())).collect();
            let _: () = manager.zadd_multiple(key, &scored).await?;
        }
    }
    Ok(())
}

/// Append a single element to an existing collection (`RPUSH` / `SADD` / `HSET`
/// / `ZADD`). The [`KeyItem`] variant must match the key's type.
///
/// # Errors
///
/// Returns an error if the underlying Redis command fails.
pub async fn add_item(
    mut manager: ConnectionManager,
    key: &str,
    item: &KeyItem,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match item {
        KeyItem::ListValue(value) => {
            let _: () = manager.rpush(key, value).await?;
        }
        KeyItem::SetMember(member) => {
            let _: () = manager.sadd(key, member).await?;
        }
        KeyItem::HashField { field, value } => {
            let _: () = manager.hset(key, field, value).await?;
        }
        KeyItem::ZsetMember { member, score } => {
            let _: () = manager.zadd(key, member, *score).await?;
        }
    }
    Ok(())
}

/// Edit a single collection element in place. Edits change the element's
/// value/score only (the identity is fixed) except [`ItemEdit::SetReplace`],
/// which replaces a member and is rejected if the new member already exists.
///
/// # Errors
///
/// Returns an error if the new SET member already exists, or if the underlying
/// Redis command fails.
pub async fn edit_item(
    mut manager: ConnectionManager,
    key: &str,
    edit: &ItemEdit,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match edit {
        ItemEdit::ListSet { index, value } => {
            let index = isize::try_from(*index)?;
            let _: () = manager.lset(key, index, value).await?;
        }
        ItemEdit::SetReplace { old, new } => {
            if old != new {
                let exists: bool = manager.sismember(key, new).await?;
                if exists {
                    return Err(format!("Member '{new}' already exists").into());
                }
                let _: () = manager.srem(key, old).await?;
                let _: () = manager.sadd(key, new).await?;
            }
        }
        ItemEdit::HashSet { field, value } => {
            let _: () = manager.hset(key, field, value).await?;
        }
        ItemEdit::ZsetScore { member, score } => {
            let _: () = manager.zadd(key, member, *score).await?;
        }
    }
    Ok(())
}

/// Delete a single collection element. LIST elements are removed by index via
/// the `LSET` sentinel + `LREM` trick (Redis has no delete-by-index).
///
/// # Errors
///
/// Returns an error if the underlying Redis command fails.
pub async fn delete_item(
    mut manager: ConnectionManager,
    key: &str,
    delete: &ItemDelete,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match delete {
        ItemDelete::ListIndex(index) => {
            let index = isize::try_from(*index)?;
            let _: () = manager.lset(key, index, LIST_DELETE_SENTINEL).await?;
            let _: () = manager.lrem(key, 1, LIST_DELETE_SENTINEL).await?;
        }
        ItemDelete::SetMember(member) => {
            let _: () = manager.srem(key, member).await?;
        }
        ItemDelete::HashField(field) => {
            let _: () = manager.hdel(key, field).await?;
        }
        ItemDelete::ZsetMember(member) => {
            let _: () = manager.zrem(key, member).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{decode_preview, fetch_full_value, fetch_value_page, pair_up};
    use crate::redis_client::types::{KeyValue, RedisType, ValueState};

    #[test]
    fn pair_up_splits_flat_reply() {
        let flat = vec![
            "f1".into(),
            "1".into(),
            "f2".into(),
            "2".into(),
            "odd".into(),
        ];
        assert_eq!(
            pair_up(flat),
            vec![("f1".into(), "1".into()), ("f2".into(), "2".into())]
        );
    }

    #[test]
    fn decode_preview_drops_split_trailing_char() {
        // "aé" is [0x61, 0xC3, 0xA9]; cutting after 2 bytes splits the "é".
        let bytes = "aé".as_bytes();
        assert_eq!(decode_preview(&bytes[..2], true), "a");
        assert_eq!(decode_preview(bytes, false), "aé");
    }

    #[test]
    fn decode_preview_replaces_invalid_bytes() {
        assert_eq!(decode_preview(&[0x61, 0xFF, 0x62], false), "a\u{FFFD}b");
        // Invalid byte in the middle of a truncated preview is still replaced.
        assert_eq!(decode_preview(&[0xFF, 0x61], true), "\u{FFFD}a");
    }

    /// Live checks against a seeded Redis. Run with
    /// `RROVER_TEST_URL=redis://localhost:6390 cargo test -- --ignored`
    /// after seeding `big:hash` (200k fields), `big:list` / `big:set` /
    /// `big:zset` (50k each), `big:str` (5 MB), `str:utf8` ("éa" × 30000) and
    /// `small:hash` (3 fields).
    /// True (and says so) when `RROVER_TEST_URL` isn't set, so
    /// `cargo test -- --ignored` without that server skips these tests.
    fn skip_live() -> bool {
        let skip = std::env::var_os("RROVER_TEST_URL").is_none();
        if skip {
            eprintln!("skipped: set RROVER_TEST_URL to a seeded Redis to run this test");
        }
        skip
    }

    async fn manager() -> redis::aio::ConnectionManager {
        let url = std::env::var("RROVER_TEST_URL").expect("RROVER_TEST_URL not set");
        redis::Client::open(url)
            .unwrap()
            .get_connection_manager()
            .await
            .unwrap()
    }

    fn state(key: &str, r_type: RedisType) -> ValueState {
        let mut state = ValueState::default();
        state.select(key, r_type);
        state
    }

    #[tokio::test]
    #[ignore = "needs a seeded Redis at RROVER_TEST_URL"]
    async fn live_hash_pages_with_hscan() {
        if skip_live() {
            return;
        }
        let (mut st, loaded) =
            fetch_value_page(manager().await, state("big:hash", RedisType::Hash))
                .await
                .unwrap();
        let KeyValue::Hash(first) = loaded.value else {
            panic!()
        };
        assert_eq!(first.len(), 100);
        assert_eq!(loaded.page.total, Some(200_000));
        assert!(loaded.page.has_next);

        st.next_page();
        let (_, loaded) = fetch_value_page(manager().await, st).await.unwrap();
        let KeyValue::Hash(second) = loaded.value else {
            panic!()
        };
        assert_eq!(second.len(), 100);
        assert_eq!(loaded.page.start, 100);
        assert!(second.iter().all(|row| !first.contains(row)));
    }

    #[tokio::test]
    #[ignore = "needs a seeded Redis at RROVER_TEST_URL"]
    async fn live_list_zset_set_windows() {
        if skip_live() {
            return;
        }
        let mut list = state("big:list", RedisType::List);
        list.page = 3;
        let (_, loaded) = fetch_value_page(manager().await, list).await.unwrap();
        let KeyValue::List(items) = loaded.value else {
            panic!()
        };
        assert_eq!(items.first().map(String::as_str), Some("v301"));
        assert_eq!(items.len(), 100);
        assert_eq!(loaded.page.total, Some(50_000));

        let mut zset = state("big:zset", RedisType::Zset);
        zset.page = 1;
        let (_, loaded) = fetch_value_page(manager().await, zset).await.unwrap();
        let KeyValue::Zset(items) = loaded.value else {
            panic!()
        };
        assert_eq!(items.first(), Some(&("m101".to_string(), 101.0)));

        let (_, loaded) = fetch_value_page(manager().await, state("big:set", RedisType::Set))
            .await
            .unwrap();
        let KeyValue::Set(members) = loaded.value else {
            panic!()
        };
        assert_eq!(members.len(), 100);
        assert_eq!(loaded.page.total, Some(50_000));
    }

    #[tokio::test]
    #[ignore = "needs a seeded Redis at RROVER_TEST_URL"]
    async fn live_full_value_fetches_every_chunk() {
        if skip_live() {
            return;
        }
        let KeyValue::List(items) = fetch_full_value(manager().await, "big:list", &RedisType::List)
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(items.len(), 50_000);

        let KeyValue::Set(members) = fetch_full_value(manager().await, "big:set", &RedisType::Set)
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(members.len(), 50_000);

        let KeyValue::Zset(items) = fetch_full_value(manager().await, "big:zset", &RedisType::Zset)
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(items.first(), Some(&("m1".to_string(), 1.0)));

        assert!(fetch_full_value(manager().await, "s", &RedisType::Stream)
            .await
            .is_err());
    }

    #[tokio::test]
    #[ignore = "needs a seeded Redis at RROVER_TEST_URL"]
    async fn live_page_past_end_is_clamped() {
        if skip_live() {
            return;
        }
        let mut list = state("big:list", RedisType::List);
        list.page = 600;
        let (st, loaded) = fetch_value_page(manager().await, list).await.unwrap();
        assert_eq!(st.page, 499);
        assert_eq!(loaded.page.len, 100);
        assert!(!loaded.page.has_next);
    }

    #[tokio::test]
    #[ignore = "needs a seeded Redis at RROVER_TEST_URL"]
    async fn live_small_hash_and_strings() {
        if skip_live() {
            return;
        }
        let (_, loaded) = fetch_value_page(manager().await, state("small:hash", RedisType::Hash))
            .await
            .unwrap();
        assert_eq!(loaded.page.len, 3);
        assert!(!loaded.page.is_paged());

        let (_, loaded) = fetch_value_page(manager().await, state("big:str", RedisType::String))
            .await
            .unwrap();
        let KeyValue::String { value, total_len } = &loaded.value else {
            panic!()
        };
        assert_eq!((value.len(), *total_len), (64 * 1024, 5_000_000));
        assert!(loaded.value.is_truncated());

        let (_, loaded) = fetch_value_page(manager().await, state("str:utf8", RedisType::String))
            .await
            .unwrap();
        let KeyValue::String { value, total_len } = &loaded.value else {
            panic!()
        };
        assert_eq!(*total_len, 90_000);
        assert_eq!(value.len(), 64 * 1024 - 1, "split 'é' dropped");
        assert!(!value.contains('\u{FFFD}'));
    }
}
