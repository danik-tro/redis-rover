use std::{collections::HashMap, time::Instant};

use color_eyre::eyre::Result;

use redis::AsyncCommands;

use super::bytes::{strings, Raw, Text};
use super::conn::Conn;
use super::metrics::{parse_cluster_info, ClusterSummary, InfoSnapshot};
use super::types::{
    json_path, ItemDelete, ItemEdit, JsonField, JsonPreview, JsonPreviewKind, KeyItem, KeyValue,
    LoadedValue, MemberKind, NewKeySpec, RedisInfo, RedisType, ScanSweep, Ttl, ValuePageInfo,
    ValueState, JSON_PREVIEW_ITEMS, STRING_PREVIEW_BYTES,
};
use super::types::{
    parse_consumers, parse_groups, parse_last_id, parse_pending, parse_stream_entries, prev_id,
    GroupDetail, StreamEntry, StreamValue,
};
use super::write_cmd;

/// Elements per command when fetching a whole collection for copying.
const FULL_FETCH_CHUNK: usize = 1000;
/// Pending entries listed for a consumer group.
const STREAM_PENDING_ROWS: usize = 100;

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
pub async fn redis_info_with_snapshot(manager: &mut Conn) -> Result<(RedisInfo, InfoSnapshot)> {
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
pub async fn cluster_info(manager: &mut Conn) -> Result<ClusterSummary> {
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
    mut manager: Conn,
    mut state: ValueState,
) -> Result<(ValueState, LoadedValue), BoxError> {
    let key = state.key.clone().ok_or("no key selected")?;
    let manager = &mut manager;

    let value = match state.r_type {
        RedisType::String => {
            let (value, total_len, lossy) =
                fetch_string_preview(manager, &key, STRING_PREVIEW_BYTES).await?;
            state.total = None;
            return Ok((
                state,
                LoadedValue {
                    value: KeyValue::String {
                        value,
                        total_len,
                        lossy,
                    },
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
            let total: usize = redis::cmd(len_cmd)
                .arg(Raw(&key))
                .query_async(manager)
                .await?;
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
        RedisType::Stream => KeyValue::Stream(fetch_stream(manager, &key, &mut state).await?),
        RedisType::Unknown(_) => KeyValue::Unknown,
    };

    let len = match &value {
        KeyValue::List(items) | KeyValue::Set(items) => items.len(),
        KeyValue::Hash(entries) => entries.len(),
        KeyValue::Zset(entries) => entries.len(),
        KeyValue::Stream(stream) => stream.entries.len(),
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
    manager: &mut Conn,
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
        cmd.arg("MEMORY").arg(Raw(key));
        if !at_root {
            cmd.arg(&path);
        }
        first_u64(&cmd.query_async(manager).await?).unwrap_or(0)
    };

    if bytes <= cap {
        let mut cmd = redis::cmd("JSON.GET");
        cmd.arg(Raw(key));
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
        .arg(Raw(key))
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
                    .arg(Raw(key))
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
                .arg(Raw(key))
                .arg(&path)
                .cmd("JSON.GET")
                .arg(Raw(key))
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
    manager: &mut Conn,
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
            .arg(Raw(key))
            .arg(&field)
            .cmd("JSON.DEBUG")
            .arg("MEMORY")
            .arg(Raw(key))
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
    mut manager: Conn,
    key: &str,
    r_type: &RedisType,
) -> Result<KeyValue, BoxError> {
    let manager = &mut manager;
    let value = match r_type {
        RedisType::String => {
            let bytes: Vec<u8> = redis::cmd("GET").arg(Raw(key)).query_async(manager).await?;
            let total_len = bytes.len();
            let text = String::from_utf8_lossy(&bytes);
            KeyValue::String {
                lossy: matches!(text, std::borrow::Cow::Owned(_)),
                value: text.into_owned(),
                total_len,
            }
        }
        RedisType::Json => {
            let raw: String = redis::cmd("JSON.GET")
                .arg(Raw(key))
                .query_async(manager)
                .await?;
            KeyValue::Json(serde_json::from_str(&raw)?)
        }
        RedisType::List => {
            let mut items = Vec::new();
            loop {
                let (start, stop) = chunk_bounds(items.len())?;
                let chunk: Vec<Text> = redis::cmd("LRANGE")
                    .arg(Raw(key))
                    .arg(start)
                    .arg(stop)
                    .query_async(manager)
                    .await?;
                let done = chunk.len() < FULL_FETCH_CHUNK;
                items.extend(strings(chunk));
                if done {
                    break KeyValue::List(items);
                }
            }
        }
        RedisType::Zset => {
            let mut items = Vec::new();
            loop {
                let (start, stop) = chunk_bounds(items.len())?;
                let chunk: Vec<(Text, f64)> = redis::cmd("ZRANGE")
                    .arg(Raw(key))
                    .arg(start)
                    .arg(stop)
                    .arg("WITHSCORES")
                    .query_async(manager)
                    .await?;
                let done = chunk.len() < FULL_FETCH_CHUNK;
                items.extend(chunk.into_iter().map(|(member, score)| (member.0, score)));
                if done {
                    break KeyValue::Zset(items);
                }
            }
        }
        RedisType::Hash | RedisType::Set => {
            let is_hash = *r_type == RedisType::Hash;
            let mut sweep = ScanSweep::default();
            loop {
                let (cursor, batch): (u64, Vec<Text>) =
                    redis::cmd(if is_hash { "HSCAN" } else { "SSCAN" })
                        .arg(Raw(key))
                        .arg(sweep.cursor)
                        .arg("COUNT")
                        .arg(FULL_FETCH_CHUNK)
                        .query_async(manager)
                        .await?;
                let batch = strings(batch);
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
        RedisType::Stream => KeyValue::Stream(StreamValue {
            entries: fetch_all_stream_entries(manager, key).await?,
            ..StreamValue::default()
        }),
        RedisType::Unknown(_) => {
            return Err(format!("copying {} values isn't supported yet", r_type.name()).into());
        }
    };
    Ok(value)
}

/// `XREVRANGE key start - COUNT count`.
async fn xrevrange(
    manager: &mut Conn,
    key: &str,
    start: &str,
    count: usize,
) -> Result<Vec<StreamEntry>, BoxError> {
    let reply: redis::Value = redis::cmd("XREVRANGE")
        .arg(Raw(key))
        .arg(start)
        .arg("-")
        .arg("COUNT")
        .arg(count)
        .query_async(manager)
        .await?;
    Ok(parse_stream_entries(reply))
}

/// The page `state` points at, newest first. Pages are walked by ID from
/// the nearest start already known, and a page past the end falls back to
/// the last one that has entries.
async fn fetch_stream(
    manager: &mut Conn,
    key: &str,
    state: &mut ValueState,
) -> Result<StreamValue, BoxError> {
    let length: usize = redis::cmd("XLEN")
        .arg(Raw(key))
        .query_async(&mut *manager)
        .await?;
    state.total = Some(length);
    state.clamp_page();
    let size = state.page_size.max(1);
    if state.stream_starts.is_empty() {
        state.stream_starts.push("+".to_owned());
    }
    while state.stream_starts.len() <= state.page {
        let from = state.stream_starts[state.stream_starts.len() - 1].clone();
        let walked = xrevrange(manager, key, &from, size).await?;
        let next = (walked.len() == size)
            .then(|| walked.last().and_then(|e| prev_id(&e.id)))
            .flatten();
        let Some(next) = next else {
            state.page = state.stream_starts.len() - 1;
            break;
        };
        state.stream_starts.push(next);
    }
    let mut entries = xrevrange(manager, key, &state.stream_starts[state.page], size).await?;
    if entries.is_empty() && state.page > 0 {
        // Everything from here down was deleted: start over from the top.
        state.page = 0;
        state.stream_starts.truncate(1);
        entries = xrevrange(manager, key, "+", size).await?;
    }
    state.stream_starts.truncate(state.page + 1);
    if entries.len() == size {
        state
            .stream_starts
            .extend(entries.last().and_then(|e| prev_id(&e.id)));
    }

    let last_id = redis::cmd("XINFO")
        .arg("STREAM")
        .arg(Raw(key))
        .query_async::<redis::Value>(&mut *manager)
        .await
        .ok()
        .and_then(parse_last_id)
        .unwrap_or_default();
    // Refused on some managed services; the entries still show.
    let groups = redis::cmd("XINFO")
        .arg("GROUPS")
        .arg(Raw(key))
        .query_async::<redis::Value>(&mut *manager)
        .await
        .ok()
        .map(parse_groups);
    Ok(StreamValue {
        entries,
        length,
        last_id,
        groups,
    })
}

/// Every entry of a stream, newest first, in [`FULL_FETCH_CHUNK`] steps.
async fn fetch_all_stream_entries(
    manager: &mut Conn,
    key: &str,
) -> Result<Vec<StreamEntry>, BoxError> {
    let mut all = Vec::new();
    let mut start = "+".to_owned();
    loop {
        let chunk = xrevrange(manager, key, &start, FULL_FETCH_CHUNK).await?;
        let done = chunk.len() < FULL_FETCH_CHUNK;
        let next = chunk.last().and_then(|e| prev_id(&e.id));
        all.extend(chunk);
        match next {
            Some(next) if !done => start = next,
            _ => return Ok(all),
        }
    }
}

/// Consumers and the first pending entries of `group`. A refusal is kept
/// in the result, for the screen to show.
pub async fn fetch_stream_group(mut manager: Conn, key: &str, group: &str) -> GroupDetail {
    let mut detail = GroupDetail {
        key: key.to_owned(),
        group: group.to_owned(),
        ..GroupDetail::default()
    };
    match redis::cmd("XINFO")
        .arg("CONSUMERS")
        .arg(Raw(key))
        .arg(Raw(group))
        .query_async::<redis::Value>(&mut manager)
        .await
    {
        Ok(reply) => detail.consumers = parse_consumers(reply),
        Err(err) => detail.error = Some(err.to_string()),
    }
    match redis::cmd("XPENDING")
        .arg(Raw(key))
        .arg(Raw(group))
        .arg("-")
        .arg("+")
        .arg(STREAM_PENDING_ROWS)
        .query_async::<redis::Value>(&mut manager)
        .await
    {
        Ok(reply) => detail.pending = parse_pending(reply),
        Err(err) => detail.error = detail.error.or(Some(err.to_string())),
    }
    detail
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
    manager: &mut Conn,
    key: &str,
    state: &mut ValueState,
) -> Result<Vec<String>, BoxError> {
    let (start, stop) = window_bounds(state)?;
    let (total, items): (usize, Vec<Text>) = redis::pipe()
        .cmd("LLEN")
        .arg(Raw(key))
        .cmd("LRANGE")
        .arg(Raw(key))
        .arg(start)
        .arg(stop)
        .query_async(manager)
        .await?;
    state.total = Some(total);
    Ok(strings(items))
}

/// `ZCARD` + `ZRANGE … WITHSCORES` for the current page, in one pipeline.
/// Stores the length in `state.total`.
async fn fetch_zset_window(
    manager: &mut Conn,
    key: &str,
    state: &mut ValueState,
) -> Result<Vec<(String, f64)>, BoxError> {
    let (start, stop) = window_bounds(state)?;
    let (total, items): (usize, Vec<(Text, f64)>) = redis::pipe()
        .cmd("ZCARD")
        .arg(Raw(key))
        .cmd("ZRANGE")
        .arg(Raw(key))
        .arg(start)
        .arg(stop)
        .arg("WITHSCORES")
        .query_async(manager)
        .await?;
    state.total = Some(total);
    Ok(items
        .into_iter()
        .map(|(member, score)| (member.0, score))
        .collect())
}

/// One `HSCAN` / `SSCAN key cursor COUNT n` step. HSCAN replies with a flat
/// `field, value, …` list; pair it up with [`pair_up`].
async fn scan_step(
    manager: &mut Conn,
    key: &str,
    is_hash: bool,
    state: &ValueState,
) -> Result<(u64, Vec<String>), BoxError> {
    let (cursor, batch): (u64, Vec<Text>) = redis::cmd(if is_hash { "HSCAN" } else { "SSCAN" })
        .arg(Raw(key))
        .arg(state.sweep.cursor)
        .arg("COUNT")
        .arg(state.scan_count)
        .query_async(manager)
        .await?;
    Ok((cursor, strings(batch)))
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

/// `STRLEN` + `GETRANGE 0 max-1` in one pipeline. Returns the decoded preview,
/// the full length in bytes, and whether decoding replaced invalid bytes.
async fn fetch_string_preview(
    manager: &mut Conn,
    key: &str,
    max: usize,
) -> Result<(String, usize, bool), BoxError> {
    let (total, bytes): (usize, Vec<u8>) = redis::pipe()
        .cmd("STRLEN")
        .arg(Raw(key))
        .cmd("GETRANGE")
        .arg(Raw(key))
        .arg(0)
        .arg(max.saturating_sub(1))
        .query_async(manager)
        .await?;
    let truncated = bytes.len() < total;
    let (text, lossy) = decode_preview(&bytes, truncated);
    Ok((text, total, lossy))
}

/// Decode a value preview as UTF-8. When the preview was cut short, a
/// multi-byte character split at the end is dropped instead of shown as `�`.
/// Other invalid bytes are replaced with `�`, and the flag says so: such a
/// value is binary, and saving the text shown would overwrite it.
fn decode_preview(bytes: &[u8], truncated: bool) -> (String, bool) {
    let mut end = bytes.len();
    if truncated {
        if let Err(err) = std::str::from_utf8(bytes) {
            // `error_len() == None` means the input ended mid-character.
            if err.error_len().is_none() {
                end = err.valid_up_to();
            }
        }
    }
    let text = String::from_utf8_lossy(&bytes[..end]);
    let lossy = matches!(text, std::borrow::Cow::Owned(_));
    (text.into_owned(), lossy)
}

type WriteResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// Run the commands of one write ([`write_cmd`]). Several commands go in
/// one `MULTI` / `EXEC`, so a lost connection or a timeout between them
/// can't leave half a write behind (a tombstone in a list, a SET member
/// removed and never added back).
async fn run_writes(mut manager: Conn, cmds: Vec<redis::Cmd>) -> WriteResult {
    match cmds.as_slice() {
        [] => {}
        [cmd] => {
            let () = cmd.query_async(&mut manager).await?;
        }
        cmds => {
            let mut pipe = redis::pipe();
            pipe.atomic();
            for cmd in cmds {
                pipe.add_command(cmd.clone());
            }
            let () = pipe.query_async(&mut manager).await?;
        }
    }
    Ok(())
}

/// The server refuses `EVAL` itself (an ACL without scripting, a proxy
/// that doesn't know it), as opposed to an error from inside the script.
/// Managed services that turn scripting off answer `NOSCRIPT` or
/// `ERR … disabled`.
fn scripting_refused(err: &(dyn std::error::Error + Send + Sync + 'static)) -> bool {
    err.downcast_ref::<redis::RedisError>().is_some_and(|e| {
        let text = e.to_string().to_lowercase();
        text.contains("noperm")
            || text.contains("unknown command")
            || text.contains("noscript")
            || text.contains("disabled")
    })
}

/// The server doesn't know the command (`COPY` before 6.2, `UNLINK`
/// before 4.0).
fn unknown_command(err: &redis::RedisError) -> bool {
    err.to_string().to_lowercase().contains("unknown command")
}

/// The LIST write without a script: check the element, then run `cmds` in
/// one `MULTI`. Only for servers where [`scripting_refused`].
async fn list_write_unscripted(
    mut manager: Conn,
    key: &str,
    index: usize,
    expected: &str,
    cmds: Vec<redis::Cmd>,
) -> WriteResult {
    let position = isize::try_from(index).unwrap_or(isize::MAX);
    let current: Option<Text> = manager.lindex(Raw(key), position).await?;
    if current.map(|text| text.0).as_deref() != Some(expected) {
        return Err(write_cmd::LIST_CHANGED.into());
    }
    run_writes(manager, cmds).await
}

/// Overwrite a key with a plain string value, keeping its TTL
/// (`SET … KEEPTTL`). A server before Redis 6.0 rejects `KEEPTTL` as a syntax
/// error; there the TTL is read first and sent back with the value
/// (`PTTL`, then `SET … PX`).
///
/// # Errors
///
/// Returns an error if the underlying Redis command fails.
pub async fn set_string(mut manager: Conn, key: &str, value: &str) -> WriteResult {
    match run_writes(manager.clone(), write_cmd::set_string(key, value)).await {
        Err(err) if is_syntax_error(err.as_ref()) => {
            let ttl_ms: i64 = redis::cmd("PTTL")
                .arg(Raw(key))
                .query_async(&mut manager)
                .await?;
            run_writes(manager, write_cmd::set_string_px(key, value, ttl_ms)).await
        }
        result => result,
    }
}

/// The server answered `ERR syntax error`: it doesn't know an option.
fn is_syntax_error(err: &(dyn std::error::Error + Send + Sync + 'static)) -> bool {
    err.downcast_ref::<redis::RedisError>()
        .is_some_and(|e| e.to_string().to_lowercase().contains("syntax error"))
}

/// Delete a key (`DEL`). Type-agnostic — works on any value type.
///
/// # Errors
///
/// Returns an error if the underlying Redis command fails.
pub async fn delete_key(manager: Conn, key: &str) -> WriteResult {
    run_writes(manager, write_cmd::delete_key(key)).await
}

/// Set a key-level TTL (`EXPIRE`; `0` deletes the key) or clear it
/// (`PERSIST`). TTL is key-level in Redis and applies to every value type.
///
/// # Errors
///
/// Returns an error if the underlying Redis command fails.
pub async fn set_ttl(manager: Conn, key: &str, ttl: Ttl) -> WriteResult {
    run_writes(manager, write_cmd::set_ttl(key, ttl)).await
}

/// How a rename or copy of a key ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moved {
    Done,
    /// The target name is taken and `overwrite` was off: nothing changed.
    TargetExists,
}

/// `RENAMENX from to` (`TargetExists` when it answers 0) or `RENAME`.
///
/// # Errors
///
/// Returns an error if the command fails (`from` is gone, cross-slot).
pub async fn rename_key(
    mut manager: Conn,
    from: &str,
    to: &str,
    overwrite: bool,
) -> Result<Moved, BoxError> {
    let cmd = write_cmd::rename_key(from, to, overwrite);
    if overwrite {
        let () = cmd.query_async(&mut manager).await?;
        return Ok(Moved::Done);
    }
    let renamed: i64 = cmd.query_async(&mut manager).await?;
    Ok(if renamed == 1 {
        Moved::Done
    } else {
        Moved::TargetExists
    })
}

/// `COPY` answered "unknown command": the server is older than 6.2.
#[derive(Debug)]
pub struct CopyUnsupported;

impl std::fmt::Display for CopyUnsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("COPY is not supported (Redis before 6.2)")
    }
}

impl std::error::Error for CopyUnsupported {}

/// `COPY from to [DB n] [REPLACE]`.
///
/// # Errors
///
/// [`CopyUnsupported`] before Redis 6.2 (copy with [`copy_key_dump`]
/// instead), or the command's error.
pub async fn copy_key(
    mut manager: Conn,
    from: &str,
    to: &str,
    db: Option<i64>,
    overwrite: bool,
) -> Result<Moved, BoxError> {
    match write_cmd::copy_key(from, to, db, overwrite)
        .query_async::<i64>(&mut manager)
        .await
    {
        Ok(1) => Ok(Moved::Done),
        // COPY answers 0 for a taken target, and for a missing source.
        Ok(_) => {
            let exists: bool = manager.exists(Raw(from)).await?;
            if exists {
                Ok(Moved::TargetExists)
            } else {
                Err(format!("{} is gone", super::bytes::shown(from)).into())
            }
        }
        Err(err) if unknown_command(&err) || (db.is_some() && is_syntax_error(&err)) => {
            Err(Box::new(CopyUnsupported))
        }
        Err(err) => Err(err.into()),
    }
}

/// A copy before Redis 6.2, or to another DB on a server without `COPY … DB`
/// (Dragonfly answers a syntax error): `PTTL` + `DUMP from` on `source`, then
/// `RESTORE to ttl payload [REPLACE]` on `target` (a connection to the
/// target DB), so the TTL is kept.
///
/// # Errors
///
/// Returns an error if `from` is gone or a command fails.
pub async fn copy_key_dump(
    mut source: Conn,
    mut target: Conn,
    from: &str,
    to: &str,
    overwrite: bool,
) -> Result<Moved, BoxError> {
    let mut pipe = redis::pipe();
    pipe.cmd("PTTL")
        .arg(Raw(from))
        .add_command(write_cmd::dump_key(from));
    let (ttl_ms, payload): (i64, Option<Vec<u8>>) = pipe.query_async(&mut source).await?;
    let Some(payload) = payload else {
        return Err(format!("{} is gone", super::bytes::shown(from)).into());
    };
    match write_cmd::restore_key(to, ttl_ms, &payload, overwrite)
        .query_async::<()>(&mut target)
        .await
    {
        Ok(()) => Ok(Moved::Done),
        Err(err) if err.to_string().contains("BUSYKEY") => Ok(Moved::TargetExists),
        Err(err) => Err(err.into()),
    }
}

/// Apply a collection edited whole in `$EDITOR`: its writes in one
/// `MULTI`. A list is `DEL` + `RPUSH`; its `PTTL` is read first and put
/// back with `PEXPIRE` in the same transaction.
///
/// # Errors
///
/// Returns an error if a command fails (the transaction then wrote
/// nothing).
pub async fn apply_value_edit(
    mut manager: Conn,
    key: &str,
    edit: &super::value_edit::ValueEdit,
) -> WriteResult {
    let mut cmds = write_cmd::apply_value_edit(key, edit);
    if matches!(edit, super::value_edit::ValueEdit::List { items } if !items.is_empty()) {
        let ttl_ms: i64 = manager.pttl(Raw(key)).await?;
        if ttl_ms > 0 {
            let mut pexpire = redis::cmd("PEXPIRE");
            pexpire.arg(Raw(key)).arg(ttl_ms);
            cmds.push(pexpire);
        }
    }
    let mut pipe = redis::pipe();
    pipe.atomic();
    for cmd in cmds {
        pipe.add_command(cmd).ignore();
    }
    let () = pipe.query_async(&mut manager).await?;
    Ok(())
}

/// Attempts at a [`shift_ttl`] whose `EXEC` was aborted by a concurrent
/// change, before giving up.
const SHIFT_TTL_ATTEMPTS: usize = 3;

/// `WATCH key`, `TTL key`, then `MULTI` / `EXPIRE key (ttl + delta)` /
/// `EXEC`; an aborted `EXEC` (the key changed meanwhile) is retried.
/// `manager` must be a connection of its own: `WATCH` applies to the whole
/// connection. Returns the new TTL.
///
/// # Errors
///
/// The key is gone or has no TTL, the result would not be positive, the key
/// kept changing, or a command failed.
pub async fn shift_ttl(manager: Conn, key: &str, delta_secs: i64) -> Result<i64, BoxError> {
    // `WATCH` and its `EXEC` must reach the same node.
    let mut manager = manager.on_slot(super::cluster::key_slot(key));
    let name = super::bytes::shown(key);
    for _ in 0..SHIFT_TTL_ATTEMPTS {
        let () = redis::cmd("WATCH")
            .arg(Raw(key))
            .query_async(&mut manager)
            .await?;
        let ttl: i64 = manager.ttl(Raw(key)).await?;
        let refuse = match ttl {
            -2 => Some(format!("{name} is gone")),
            -1 => Some(format!(
                "{name} has no TTL to shift: enter a duration without + / -"
            )),
            _ if ttl.saturating_add(delta_secs) <= 0 => Some(format!(
                "{name} would expire at once (TTL {ttl}s {delta_secs:+}s): enter 0 to delete it"
            )),
            _ => None,
        };
        if let Some(message) = refuse {
            let () = redis::cmd("UNWATCH").query_async(&mut manager).await?;
            return Err(message.into());
        }
        let new_ttl = ttl.saturating_add(delta_secs);
        let mut pipe = redis::pipe();
        pipe.atomic().cmd("EXPIRE").arg(Raw(key)).arg(new_ttl);
        let reply: redis::Value = pipe.query_async(&mut manager).await?;
        if !matches!(reply, redis::Value::Nil) {
            return Ok(new_ttl);
        }
    }
    Err(format!("{name} kept changing, try again").into())
}

/// How a field / member rename ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Renamed {
    /// In one script, atomically.
    Script,
    /// The server refused `EVAL`: plain commands one by one.
    Unscripted,
}

/// Rename a HASH field (`kind: None`) or a SET / ZSET member in one script
/// ([`write_cmd::rename_field`] / [`write_cmd::rename_member`]); where the
/// server refuses `EVAL`, with plain commands that never overwrite or lose
/// an element (`HSETNX`, `SADD` answering 0, `ZADD NX`), but not atomically.
///
/// # Errors
///
/// `to` exists, `from` is gone, or a command fails.
pub async fn rename_item(
    mut manager: Conn,
    key: &str,
    kind: Option<MemberKind>,
    from: &str,
    to: &str,
) -> Result<Renamed, BoxError> {
    let script = match kind {
        None => write_cmd::rename_field(key, from, to),
        Some(kind) => write_cmd::rename_member(key, kind, from, to),
    };
    match script.query_async::<redis::Value>(&mut manager).await {
        Ok(_) => return Ok(Renamed::Script),
        Err(err) if scripting_refused(&err) => {}
        Err(err) => return Err(err.into()),
    }
    let what = if kind.is_none() { "field" } else { "member" };
    let exists = || -> BoxError { format!("the {what} already exists").into() };
    let gone = || -> BoxError { format!("the {what} {}", write_cmd::RENAME_SOURCE_GONE).into() };
    match kind {
        None => {
            let value: Option<Vec<u8>> = manager.hget(Raw(key), Raw(from)).await?;
            let value = value.ok_or_else(gone)?;
            let set: bool = manager.hset_nx(Raw(key), Raw(to), value).await?;
            if !set {
                return Err(exists());
            }
            let () = manager.hdel(Raw(key), Raw(from)).await?;
        }
        Some(MemberKind::Set) => {
            let present: bool = manager.sismember(Raw(key), Raw(from)).await?;
            if !present {
                return Err(gone());
            }
            let added: i64 = manager.sadd(Raw(key), Raw(to)).await?;
            if added == 0 {
                return Err(exists());
            }
            let () = manager.srem(Raw(key), Raw(from)).await?;
        }
        Some(MemberKind::Zset) => {
            let score: Option<f64> = manager.zscore(Raw(key), Raw(from)).await?;
            let score = score.ok_or_else(gone)?;
            let added: i64 = redis::cmd("ZADD")
                .arg(Raw(key))
                .arg("NX")
                .arg(score)
                .arg(Raw(to))
                .query_async(&mut manager)
                .await?;
            if added == 0 {
                return Err(exists());
            }
            let () = manager.zrem(Raw(key), Raw(from)).await?;
        }
    }
    Ok(Renamed::Unscripted)
}

/// `UNLINK` the keys, [`write_cmd::DELETE_CHUNK`] per command (`DEL` on a
/// server before 4.0). Returns how many existed.
///
/// # Errors
///
/// Returns the first failing chunk's error; earlier chunks stay deleted.
pub async fn delete_keys(manager: Conn, keys: &[String]) -> Result<u64, BoxError> {
    if !manager.is_cluster() {
        return delete_chunks(manager, keys).await;
    }
    // On a cluster one command may only name keys of one slot.
    let mut removed = 0;
    for group in super::cluster::by_slot(keys) {
        removed += delete_chunks(manager.clone(), &group).await?;
    }
    Ok(removed)
}

/// [`delete_keys`] for keys one command may name together.
async fn delete_chunks(mut manager: Conn, keys: &[String]) -> Result<u64, BoxError> {
    let mut removed = 0;
    let mut cmds = write_cmd::delete_keys(keys).into_iter().peekable();
    // Only the first chunk can find `UNLINK` missing; then all go as `DEL`.
    if let Some(first) = cmds.next() {
        match first.query_async::<u64>(&mut manager).await {
            Ok(n) => removed += n,
            Err(err) if unknown_command(&err) => {
                cmds = write_cmd::delete_keys_del(keys).into_iter().peekable();
            }
            Err(err) => return Err(err.into()),
        }
    }
    for cmd in cmds {
        removed += cmd.query_async::<u64>(&mut manager).await?;
    }
    Ok(removed)
}

/// Return whether a key already exists (`EXISTS`). Used to guard key creation so
/// the wizard never silently clobbers an existing key.
///
/// # Errors
///
/// Returns an error if the underlying Redis command fails.
pub async fn key_exists(
    mut manager: Conn,
    key: &str,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let exists: bool = manager.exists(Raw(key)).await?;
    Ok(exists)
}

/// Create a key from a fully-specified [`NewKeySpec`]. STRING uses `SET`; the
/// collection types seed every accumulated element in a single command
/// (`RPUSH` / `SADD` / `HMSET` / `ZADD`).
///
/// # Errors
///
/// Returns an error if the underlying Redis command fails.
pub async fn create_key(mut manager: Conn, key: &str, spec: &NewKeySpec) -> WriteResult {
    if let NewKeySpec::String(_) = spec {
        // `SET … NX` answers nil when the key appeared after the `EXISTS`
        // guard: nothing was overwritten.
        for cmd in write_cmd::create_key(key, spec) {
            let set: Option<String> = cmd.query_async(&mut manager).await?;
            if set.is_none() {
                return Err(format!("Key '{key}' already exists").into());
            }
        }
        return Ok(());
    }
    run_writes(manager, write_cmd::create_key(key, spec)).await
}

/// Append a single element to an existing collection (`RPUSH` / `SADD` / `HSET`
/// / `ZADD`). The [`KeyItem`] variant must match the key's type.
///
/// # Errors
///
/// Returns an error if the underlying Redis command fails.
pub async fn add_item(manager: Conn, key: &str, item: &KeyItem) -> WriteResult {
    run_writes(manager, write_cmd::add_item(key, item)).await
}

/// Edit a single collection element in place. Edits change the element's
/// value/score only (the identity is fixed) except [`ItemEdit::SetReplace`],
/// which replaces a member and is rejected if the new member already exists.
///
/// # Errors
///
/// Returns an error if the new SET member already exists, or if the underlying
/// Redis command fails.
pub async fn edit_item(mut manager: Conn, key: &str, edit: &ItemEdit) -> WriteResult {
    if let ItemEdit::SetReplace { old, new } = edit {
        if old != new {
            let exists: bool = manager.sismember(Raw(key), Raw(new)).await?;
            if exists {
                return Err(format!("Member '{new}' already exists").into());
            }
        }
    }
    match run_writes(manager.clone(), write_cmd::edit_item(key, edit)).await {
        Err(err) if scripting_refused(err.as_ref()) => match edit {
            ItemEdit::ListSet {
                index,
                expected,
                value,
            } => {
                let cmds = write_cmd::list_set_unscripted(key, *index, value);
                list_write_unscripted(manager, key, *index, expected, cmds).await
            }
            _ => Err(err),
        },
        result => result,
    }
}

/// Delete a single collection element. LIST elements are removed by index via
/// the `LSET` sentinel + `LREM` trick (Redis has no delete-by-index).
///
/// # Errors
///
/// Returns an error if the underlying Redis command fails.
pub async fn delete_item(manager: Conn, key: &str, delete: &ItemDelete) -> WriteResult {
    match run_writes(manager.clone(), write_cmd::delete_item(key, delete)).await {
        Err(err) if scripting_refused(err.as_ref()) => match delete {
            ItemDelete::ListIndex { index, expected } => {
                let cmds = write_cmd::list_delete_unscripted(key, *index);
                list_write_unscripted(manager, key, *index, expected, cmds).await
            }
            _ => Err(err),
        },
        result => result,
    }
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
        assert_eq!(decode_preview(&bytes[..2], true), ("a".to_string(), false));
        assert_eq!(decode_preview(bytes, false), ("aé".to_string(), false));
    }

    #[test]
    fn decode_preview_replaces_invalid_bytes() {
        assert_eq!(
            decode_preview(&[0x61, 0xFF, 0x62], false),
            ("a\u{FFFD}b".to_string(), true)
        );
        // Invalid byte in the middle of a truncated preview is still replaced.
        assert_eq!(
            decode_preview(&[0xFF, 0x61], true),
            ("\u{FFFD}a".to_string(), true)
        );
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

    async fn manager() -> super::Conn {
        let url = std::env::var("RROVER_TEST_URL").expect("RROVER_TEST_URL not set");
        redis::Client::open(url)
            .unwrap()
            .get_connection_manager()
            .await
            .unwrap()
            .into()
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
        let KeyValue::String {
            value, total_len, ..
        } = &loaded.value
        else {
            panic!()
        };
        assert_eq!((value.len(), *total_len), (64 * 1024, 5_000_000));
        assert!(loaded.value.is_truncated());

        let (_, loaded) = fetch_value_page(manager().await, state("str:utf8", RedisType::String))
            .await
            .unwrap();
        let KeyValue::String {
            value, total_len, ..
        } = &loaded.value
        else {
            panic!()
        };
        assert_eq!(*total_len, 90_000);
        assert_eq!(value.len(), 64 * 1024 - 1, "split 'é' dropped");
        assert!(!value.contains('\u{FFFD}'));
    }
}
