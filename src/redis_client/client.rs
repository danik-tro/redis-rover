use std::collections::HashMap;

use color_eyre::eyre::Result;

use redis::{aio::ConnectionManager, AsyncCommands};

use super::types::{ItemDelete, ItemEdit, KeyItem, KeyValue, NewKeySpec, RedisInfo, RedisType};

const VALUE_PREVIEW_LIMIT: isize = 100;

/// Sentinel used to delete a LIST element by index (Redis has no delete-by-index):
/// overwrite the slot with this marker via `LSET`, then remove it with `LREM`.
const LIST_DELETE_SENTINEL: &str = "__rrover_deleted__\u{0}";

/// Run `INFO` against Redis and parse the response into [`RedisInfo`].
///
/// # Errors
///
/// Returns an error if the command fails or the response cannot be parsed.
// TODO: should be a better solution to handle this.
pub async fn redis_info(manager: &mut ConnectionManager) -> Result<RedisInfo> {
    let info: String = redis::cmd("INFO").query_async(manager).await?;

    let mut map = std::collections::HashMap::new();

    for c in info.split_terminator('\n') {
        if c.starts_with('#') || c.is_empty() {
            continue;
        }

        let pair_op = c.split_once(':');

        let Some((header, value)) = pair_op else {
            continue;
        };

        map.insert(header, value.trim());
    }

    Ok(serde_json::from_value(serde_json::json!(map))?)
}

/// Fetch the value behind a key, bounded to the first
/// [`VALUE_PREVIEW_LIMIT`] items for collection types.
///
/// # Errors
///
/// Returns an error if any of the underlying Redis commands fails.
pub async fn fetch_value(
    mut manager: ConnectionManager,
    key: &str,
    r_type: RedisType,
) -> Result<KeyValue, Box<dyn std::error::Error + Send + Sync>> {
    match r_type {
        RedisType::String => {
            let value: String = manager.get(key).await?;
            Ok(KeyValue::String(value))
        }
        RedisType::List => {
            let value: Vec<String> = manager.lrange(key, 0, VALUE_PREVIEW_LIMIT - 1).await?;
            Ok(KeyValue::List(value))
        }
        RedisType::Set => {
            let members: Vec<String> = redis::cmd("SRANDMEMBER")
                .arg(key)
                .arg(VALUE_PREVIEW_LIMIT)
                .query_async(&mut manager)
                .await?;
            Ok(KeyValue::Set(members.into_iter().collect()))
        }
        RedisType::Hash => {
            let value: HashMap<String, String> = manager.hgetall(key).await?;
            Ok(KeyValue::Hash(value))
        }
        RedisType::Zset => {
            let value: Vec<(String, f64)> = manager
                .zrange_withscores(key, 0, VALUE_PREVIEW_LIMIT - 1)
                .await?;
            Ok(KeyValue::Zset(value))
        }
        RedisType::Json | RedisType::Unknown => Ok(KeyValue::Unknown),
    }
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
