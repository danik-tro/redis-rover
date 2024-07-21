use color_eyre::eyre::Result;

use redis::{aio::ConnectionManager, AsyncCommands};

use super::types::RedisInfo;

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
