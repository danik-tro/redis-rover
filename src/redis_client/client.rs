use std::collections::{HashMap, HashSet};

use color_eyre::eyre::Result;

use redis::{aio::ConnectionManager, AsyncCommands};

use super::types::{KeyMeta, KeyValue, RedisInfo, RedisType};

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

pub async fn retrieve_type_and_value(
    mut manager: redis::aio::ConnectionManager,
    key: &str,
) -> Result<(RedisType, KeyValue), Box<dyn std::error::Error + Send + Sync>> {
    let r_type: String = manager.key_type(key).await?;
    let r_type = RedisType::from(r_type);

    match r_type {
        RedisType::String => {
            let value: String = manager.get(key).await?;
            Ok((r_type, KeyValue::String(value)))
        }
        RedisType::List => {
            let value: Vec<String> = manager.lrange(key, 0, -1).await?;
            Ok((r_type, KeyValue::List(value)))
        }
        RedisType::Set => {
            let value: HashSet<String> = manager.smembers(key).await?;
            Ok((r_type, KeyValue::Set(value)))
        }
        RedisType::Hash => {
            let value: HashMap<String, String> = manager.hgetall(key).await?;
            Ok((r_type, KeyValue::Hash(value)))
        }
        RedisType::Zset => {
            let value: Vec<(String, f64)> = manager.zrange_withscores(key, 0, -1).await?;
            Ok((r_type, KeyValue::Zset(value)))
        }
        RedisType::Json => {
            // TODO: impleent json type
            // let value: serde_json::Value = manager.get(key).await?;
            Ok((r_type, KeyValue::Unknown))
        }
        RedisType::Unknown => Ok((r_type, KeyValue::Unknown)),
    }
}

pub async fn retrieve_memory_usage(
    mut manager: redis::aio::ConnectionManager,
    key: &str,
) -> Result<u128, Box<dyn std::error::Error + Send + Sync>> {
    let size: Option<u128> = redis::cmd("MEMORY")
        .arg("USAGE")
        .arg(key)
        .query_async(&mut manager)
        .await?;

    Ok(size.unwrap_or_default())
}

pub async fn retrieve_ttl(
    mut manager: redis::aio::ConnectionManager,
    key: &str,
) -> Result<isize, Box<dyn std::error::Error + Send + Sync>> {
    Ok(manager.ttl(key).await?)
}

pub async fn fetch_meta(
    manager: redis::aio::ConnectionManager,
    key: &str,
) -> Result<KeyMeta, Box<dyn std::error::Error + Sync + Send>> {
    let ((r_type, value), size, ttl) = tokio::try_join!(
        retrieve_type_and_value(manager.clone(), key),
        retrieve_memory_usage(manager.clone(), key),
        retrieve_ttl(manager, key),
    )?;

    Ok(KeyMeta {
        value,
        r_type,
        size,
        ttl,
        key: key.into(),
    })
}
