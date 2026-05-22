use std::collections::HashMap;

use color_eyre::eyre::Result;

use redis::{aio::ConnectionManager, AsyncCommands};

use super::types::{KeyValue, RedisInfo, RedisType};

const VALUE_PREVIEW_LIMIT: isize = 100;

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
