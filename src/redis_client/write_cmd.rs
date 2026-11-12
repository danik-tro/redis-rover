//! The Redis commands each write sends, built in one place. `client.rs` runs
//! them, and `RedisEvent::preview` shows them before they are sent, so
//! the preview is exactly what runs.
//!
//! Guards that only read (`EXISTS` before creating a key, `SISMEMBER`
//! before replacing a SET member) are not writes and stay in `client.rs` /
//! `storage.rs`.

use redis::Cmd;

use super::types::{ItemDelete, ItemEdit, KeyItem, NewKeySpec};

/// Sentinel used to delete a LIST element by index (Redis has no
/// delete-by-index): overwrite the slot with it via `LSET`, then remove it
/// with `LREM`.
pub const LIST_DELETE_SENTINEL: &str = "__rrover_deleted__\u{0}";

/// A LIST index as Redis takes it. Indexes past `isize::MAX` can't exist,
/// and the server answers "index out of range".
fn list_index(index: usize) -> isize {
    isize::try_from(index).unwrap_or(isize::MAX)
}

/// `SET key value`.
#[must_use]
pub fn set_string(key: &str, value: &str) -> Vec<Cmd> {
    vec![Cmd::set(key, value)]
}

/// `DEL key`.
#[must_use]
pub fn delete_key(key: &str) -> Vec<Cmd> {
    vec![Cmd::del(key)]
}

/// `EXPIRE key secs`, or `PERSIST key` when `secs <= 0`.
#[must_use]
pub fn set_ttl(key: &str, secs: i64) -> Vec<Cmd> {
    if secs > 0 {
        vec![Cmd::expire(key, secs)]
    } else {
        vec![Cmd::persist(key)]
    }
}

/// `SET`, or one `RPUSH` / `SADD` / `HMSET` / `ZADD` with every seed element.
#[must_use]
pub fn create_key(key: &str, spec: &NewKeySpec) -> Vec<Cmd> {
    let cmd = match spec {
        NewKeySpec::String(value) => Cmd::set(key, value),
        NewKeySpec::List(items) => Cmd::rpush(key, items),
        NewKeySpec::Set(members) => Cmd::sadd(key, members),
        NewKeySpec::Hash(pairs) => Cmd::hset_multiple(key, pairs),
        NewKeySpec::Zset(pairs) => {
            // The redis crate takes (score, member); our pairs are (member, score).
            let scored: Vec<(f64, &str)> = pairs.iter().map(|(m, s)| (*s, m.as_str())).collect();
            Cmd::zadd_multiple(key, &scored)
        }
    };
    vec![cmd]
}

/// `RPUSH` / `SADD` / `HSET` / `ZADD` of one element.
#[must_use]
pub fn add_item(key: &str, item: &KeyItem) -> Vec<Cmd> {
    let cmd = match item {
        KeyItem::ListValue(value) => Cmd::rpush(key, value),
        KeyItem::SetMember(member) => Cmd::sadd(key, member),
        KeyItem::HashField { field, value } => Cmd::hset(key, field, value),
        KeyItem::ZsetMember { member, score } => Cmd::zadd(key, member, *score),
    };
    vec![cmd]
}

/// `LSET`, `HSET`, `ZADD`, or `SREM` + `SADD` for a SET member (nothing
/// when the member doesn't change).
#[must_use]
pub fn edit_item(key: &str, edit: &ItemEdit) -> Vec<Cmd> {
    match edit {
        ItemEdit::ListSet { index, value } => vec![Cmd::lset(key, list_index(*index), value)],
        ItemEdit::SetReplace { old, new } if old == new => Vec::new(),
        ItemEdit::SetReplace { old, new } => vec![Cmd::srem(key, old), Cmd::sadd(key, new)],
        ItemEdit::HashSet { field, value } => vec![Cmd::hset(key, field, value)],
        ItemEdit::ZsetScore { member, score } => vec![Cmd::zadd(key, member, *score)],
    }
}

/// `SREM` / `HDEL` / `ZREM`, or for a LIST element the `LSET` tombstone +
/// `LREM`.
#[must_use]
pub fn delete_item(key: &str, delete: &ItemDelete) -> Vec<Cmd> {
    match delete {
        ItemDelete::ListIndex(index) => vec![
            Cmd::lset(key, list_index(*index), LIST_DELETE_SENTINEL),
            Cmd::lrem(key, 1, LIST_DELETE_SENTINEL),
        ],
        ItemDelete::SetMember(member) => vec![Cmd::srem(key, member)],
        ItemDelete::HashField(field) => vec![Cmd::hdel(key, field)],
        ItemDelete::ZsetMember(member) => vec![Cmd::zrem(key, member)],
    }
}

/// `PUBLISH channel payload`.
#[must_use]
pub fn publish(channel: &str, payload: &str) -> Vec<Cmd> {
    vec![Cmd::publish(channel, payload)]
}
