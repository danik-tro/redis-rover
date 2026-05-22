use std::collections::HashMap;

use color_eyre::eyre::Result;

use redis::{aio::ConnectionManager, AsyncCommands};

use super::types::{KeyMeta, KeyValue, RedisInfo, RedisType};

const VALUE_PREVIEW_LIMIT: isize = 100;

// TODO: should be a better solution to handle this.
pub async fn redis_info(manager: &mut ConnectionManager) -> Result<RedisInfo> {
    let info: String = redis::cmd("INFO").query_async(manager).await?;

    let mut map = std::collections::HashMap::new();

    for c in info.split_terminator("\n") {
        if c.starts_with("#") || c == "" {
            continue;
        }

        let pair_op = c.split_once(":");

        let Some((header, value)) = pair_op else {
            continue;
        };

        map.insert(header, value.trim());
    }

    Ok(serde_json::from_value(serde_json::json!(map))?)
}

pub async fn keys(
    manager: &mut ConnectionManager,
    cursor: Option<usize>,
    pattern: Option<String>,
) -> Result<(usize, Vec<String>)> {
    let (cursor, keys): (usize, Vec<String>) = redis::cmd("SCAN")
        .arg(cursor.unwrap_or_default())
        .arg("MATCH")
        .arg(pattern.unwrap_or_else(|| "*".into()))
        .query_async(manager)
        .await?;

    Ok((cursor, keys))
}

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

pub async fn fetch_meta_light(
    manager: ConnectionManager,
    key: &str,
) -> Result<KeyMeta, Box<dyn std::error::Error + Sync + Send>> {
    let mut conn = manager;
    let (r_type, size, ttl): (String, Option<u128>, isize) = redis::pipe()
        .cmd("TYPE")
        .arg(key)
        .cmd("MEMORY")
        .arg("USAGE")
        .arg(key)
        .cmd("TTL")
        .arg(key)
        .query_async(&mut conn)
        .await?;

    Ok(KeyMeta {
        key: key.into(),
        r_type: RedisType::from(r_type),
        size: size.unwrap_or_default(),
        ttl,
        value: KeyValue::Unknown,
    })
}
