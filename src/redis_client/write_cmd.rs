//! The Redis commands each write sends, built in one place. `client.rs` runs
//! them, and `RedisEvent::preview` shows them before they are sent, so
//! the preview is exactly what runs.
//!
//! Guards that only read (`EXISTS` before creating a key, `SISMEMBER`
//! before replacing a SET member) are not writes and stay in `client.rs` /
//! `storage.rs`.

use redis::Cmd;

use super::{
    bytes::Raw,
    types::{ItemDelete, ItemEdit, KeyItem, MemberKind, NewKeySpec, Ttl},
};

/// Sentinel used to delete a LIST element by index (Redis has no
/// delete-by-index): overwrite the slot with it via `LSET`, then remove it
/// with `LREM`.
pub const LIST_DELETE_SENTINEL: &str = "__rrover_deleted__\u{0}";

/// A LIST index as Redis takes it. Indexes past `isize::MAX` can't exist,
/// and the server answers "index out of range".
fn list_index(index: usize) -> isize {
    isize::try_from(index).unwrap_or(isize::MAX)
}

/// What the list scripts answer when the element at the index is no longer
/// the one shown (another client pushed, popped or changed it).
pub const LIST_CHANGED: &str = "the list changed at this index, reload it";

/// `ARGV`: index, the value shown, the new value.
const LIST_SET_SCRIPT: &str = "-- LSET if the element is unchanged
if redis.call('LINDEX', KEYS[1], ARGV[1]) ~= ARGV[2] then
  return redis.error_reply('ERR the list changed at this index, reload it')
end
return redis.call('LSET', KEYS[1], ARGV[1], ARGV[3])";

/// `ARGV`: index, the value shown, the tombstone.
const LIST_DELETE_SCRIPT: &str = "-- LSET tombstone + LREM if the element is unchanged
if redis.call('LINDEX', KEYS[1], ARGV[1]) ~= ARGV[2] then
  return redis.error_reply('ERR the list changed at this index, reload it')
end
redis.call('LSET', KEYS[1], ARGV[1], ARGV[3])
return redis.call('LREM', KEYS[1], 1, ARGV[3])";

/// `EVAL script 1 key index expected arg`.
fn list_script(script: &str, key: &str, index: usize, expected: &str, arg: &str) -> Cmd {
    let mut cmd = redis::cmd("EVAL");
    cmd.arg(script)
        .arg(1)
        .arg(Raw(key))
        .arg(list_index(index))
        .arg(Raw(expected))
        .arg(Raw(arg));
    cmd
}

/// Every element as its exact bytes.
fn raws(items: &[String]) -> Vec<Raw<'_>> {
    items.iter().map(|item| Raw(item)).collect()
}

/// `SET key value KEEPTTL`: an edit changes the value, not the expiry. A
/// plain `SET` would drop the TTL and make a session or cache key permanent.
#[must_use]
pub fn set_string(key: &str, value: &str) -> Vec<Cmd> {
    let mut cmd = Cmd::set(Raw(key), Raw(value));
    cmd.arg("KEEPTTL");
    vec![cmd]
}

/// The same edit for a server without `KEEPTTL` (before Redis 6.0):
/// `SET key value PX ttl_ms` with the TTL read just before, or a plain `SET`
/// when the key has none (`PTTL` answers -1, or -2 for a missing key).
#[must_use]
pub fn set_string_px(key: &str, value: &str, ttl_ms: i64) -> Vec<Cmd> {
    let mut cmd = Cmd::set(Raw(key), Raw(value));
    if ttl_ms > 0 {
        cmd.arg("PX").arg(ttl_ms);
    }
    vec![cmd]
}

/// `DEL key`.
#[must_use]
pub fn delete_key(key: &str) -> Vec<Cmd> {
    vec![Cmd::del(Raw(key))]
}

/// `EXPIRE key secs` (`0` deletes the key), or `PERSIST key`.
#[must_use]
pub fn set_ttl(key: &str, ttl: Ttl) -> Vec<Cmd> {
    match ttl {
        Ttl::Expire(secs) => {
            let mut cmd = redis::cmd("EXPIRE");
            cmd.arg(Raw(key)).arg(secs);
            vec![cmd]
        }
        Ttl::Persist => vec![Cmd::persist(Raw(key))],
    }
}

/// Keys per `UNLINK` of a bulk delete: big enough to be quick, small enough
/// that one command never holds the server for long.
pub const DELETE_CHUNK: usize = 500;

/// `UNLINK k1 k2 …`, one command per [`DELETE_CHUNK`] keys. They run one
/// after the other, not in one `MULTI`.
#[must_use]
pub fn delete_keys(keys: &[String]) -> Vec<Cmd> {
    keys.chunks(DELETE_CHUNK)
        .map(|chunk| {
            let mut cmd = redis::cmd("UNLINK");
            for key in chunk {
                cmd.arg(Raw(key));
            }
            cmd
        })
        .collect()
}

/// `DEL k1 k2 …` per chunk, for a server without `UNLINK` (before 4.0).
#[must_use]
pub fn delete_keys_del(keys: &[String]) -> Vec<Cmd> {
    keys.chunks(DELETE_CHUNK)
        .map(|chunk| Cmd::del(raws(chunk)))
        .collect()
}

/// `RENAMENX from to`, or `RENAME from to` once replacing `to` was
/// confirmed. Both keep the TTL.
#[must_use]
pub fn rename_key(from: &str, to: &str, overwrite: bool) -> Cmd {
    let mut cmd = redis::cmd(if overwrite { "RENAME" } else { "RENAMENX" });
    cmd.arg(Raw(from)).arg(Raw(to));
    cmd
}

/// `COPY from to [DB n] [REPLACE]` (Redis 6.2+). Keeps the TTL.
#[must_use]
pub fn copy_key(from: &str, to: &str, db: Option<i64>, overwrite: bool) -> Cmd {
    let mut cmd = redis::cmd("COPY");
    cmd.arg(Raw(from)).arg(Raw(to));
    if let Some(db) = db {
        cmd.arg("DB").arg(db);
    }
    if overwrite {
        cmd.arg("REPLACE");
    }
    cmd
}

/// `DUMP key`: the read half of a copy before Redis 6.2.
#[must_use]
pub fn dump_key(key: &str) -> Cmd {
    let mut cmd = redis::cmd("DUMP");
    cmd.arg(Raw(key));
    cmd
}

/// `RESTORE to ttl_ms payload [REPLACE]`: the write half of a copy before
/// Redis 6.2. `ttl_ms` is the source's `PTTL`; `0` (or the `-1` of a key
/// without one) restores it without a TTL.
#[must_use]
pub fn restore_key(to: &str, ttl_ms: i64, payload: &[u8], overwrite: bool) -> Cmd {
    let mut cmd = redis::cmd("RESTORE");
    cmd.arg(Raw(to)).arg(ttl_ms.max(0)).arg(payload);
    if overwrite {
        cmd.arg("REPLACE");
    }
    cmd
}

/// What the rename scripts answer when the old name is gone.
pub const RENAME_SOURCE_GONE: &str = "is gone, reload the value";

/// `ARGV`: old field, new field.
const RENAME_FIELD_SCRIPT: &str = "-- rename a hash field, never over an existing one
if redis.call('HEXISTS', KEYS[1], ARGV[2]) == 1 then
  return redis.error_reply('ERR the field already exists')
end
local value = redis.call('HGET', KEYS[1], ARGV[1])
if not value then
  return redis.error_reply('ERR the field is gone, reload the value')
end
redis.call('HSET', KEYS[1], ARGV[2], value)
return redis.call('HDEL', KEYS[1], ARGV[1])";

/// `ARGV`: old member, new member.
const RENAME_SET_MEMBER_SCRIPT: &str = "-- rename a set member, never over an existing one
if redis.call('SISMEMBER', KEYS[1], ARGV[2]) == 1 then
  return redis.error_reply('ERR the member already exists')
end
if redis.call('SREM', KEYS[1], ARGV[1]) == 0 then
  return redis.error_reply('ERR the member is gone, reload the value')
end
return redis.call('SADD', KEYS[1], ARGV[2])";

/// `ARGV`: old member, new member. The score is kept.
const RENAME_ZSET_MEMBER_SCRIPT: &str = "-- rename a zset member, keeping its score
if redis.call('ZSCORE', KEYS[1], ARGV[2]) then
  return redis.error_reply('ERR the member already exists')
end
local score = redis.call('ZSCORE', KEYS[1], ARGV[1])
if not score then
  return redis.error_reply('ERR the member is gone, reload the value')
end
redis.call('ZREM', KEYS[1], ARGV[1])
return redis.call('ZADD', KEYS[1], score, ARGV[2])";

/// `EVAL script 1 key from to`.
fn rename_script(script: &str, key: &str, from: &str, to: &str) -> Cmd {
    let mut cmd = redis::cmd("EVAL");
    cmd.arg(script)
        .arg(1)
        .arg(Raw(key))
        .arg(Raw(from))
        .arg(Raw(to));
    cmd
}

/// Rename a HASH field in one script: refused if `to` exists.
#[must_use]
pub fn rename_field(key: &str, from: &str, to: &str) -> Cmd {
    rename_script(RENAME_FIELD_SCRIPT, key, from, to)
}

/// Rename a SET / ZSET member in one script (a ZSET keeps the score):
/// refused if `to` exists.
#[must_use]
pub fn rename_member(key: &str, kind: MemberKind, from: &str, to: &str) -> Cmd {
    let script = match kind {
        MemberKind::Set => RENAME_SET_MEMBER_SCRIPT,
        MemberKind::Zset => RENAME_ZSET_MEMBER_SCRIPT,
    };
    rename_script(script, key, from, to)
}

/// The plain commands a rename script stands for, as `redis-cli` lines;
/// the value read in between shows as `<value>` / `<score>`. On a server
/// that refuses `EVAL` the runner sends these one by one (not atomic).
#[must_use]
pub fn rename_item_plain(key: &str, kind: Option<MemberKind>, from: &str, to: &str) -> Vec<String> {
    use super::escape::quote_arg;
    let q = |name: &str| quote_arg(&super::bytes::bytes_from_text(name));
    let (k, from, to) = (q(key), q(from), q(to));
    match kind {
        None => vec![
            format!("HGET {k} {from}"),
            format!("HSETNX {k} {to} <value>"),
            format!("HDEL {k} {from}"),
        ],
        Some(MemberKind::Set) => vec![
            format!("SISMEMBER {k} {from}"),
            format!("SADD {k} {to}"),
            format!("SREM {k} {from}"),
        ],
        Some(MemberKind::Zset) => vec![
            format!("ZSCORE {k} {from}"),
            format!("ZADD {k} NX <score> {to}"),
            format!("ZREM {k} {from}"),
        ],
    }
}

/// `SET … NX` (never over a key created meanwhile), or one `RPUSH` / `SADD`
/// / `HMSET` / `ZADD` with every seed element.
#[must_use]
pub fn create_key(key: &str, spec: &NewKeySpec) -> Vec<Cmd> {
    let cmd = match spec {
        NewKeySpec::String(value) => {
            let mut cmd = Cmd::set(Raw(key), Raw(value));
            cmd.arg("NX");
            cmd
        }
        NewKeySpec::List(items) => Cmd::rpush(Raw(key), raws(items)),
        NewKeySpec::Set(members) => Cmd::sadd(Raw(key), raws(members)),
        NewKeySpec::Hash(pairs) => {
            let pairs: Vec<(Raw, Raw)> = pairs.iter().map(|(f, v)| (Raw(f), Raw(v))).collect();
            Cmd::hset_multiple(Raw(key), &pairs)
        }
        NewKeySpec::Zset(pairs) => {
            // The redis crate takes (score, member); our pairs are (member, score).
            let scored: Vec<(f64, Raw)> = pairs.iter().map(|(m, s)| (*s, Raw(m))).collect();
            Cmd::zadd_multiple(Raw(key), &scored)
        }
    };
    vec![cmd]
}

/// `RPUSH` / `SADD` / `HSET` / `ZADD` of one element.
#[must_use]
pub fn add_item(key: &str, item: &KeyItem) -> Vec<Cmd> {
    let cmd = match item {
        KeyItem::ListValue(value) => Cmd::rpush(Raw(key), Raw(value)),
        KeyItem::SetMember(member) => Cmd::sadd(Raw(key), Raw(member)),
        KeyItem::HashField { field, value } => Cmd::hset(Raw(key), Raw(field), Raw(value)),
        KeyItem::ZsetMember { member, score } => Cmd::zadd(Raw(key), Raw(member), *score),
    };
    vec![cmd]
}

/// `HSET`, `ZADD`, `SREM` + `SADD` for a SET member (nothing when the
/// member doesn't change), or for a LIST element a script: `LSET` only while
/// the element at the index is still the one shown.
#[must_use]
pub fn edit_item(key: &str, edit: &ItemEdit) -> Vec<Cmd> {
    match edit {
        ItemEdit::ListSet {
            index,
            expected,
            value,
        } => vec![list_script(LIST_SET_SCRIPT, key, *index, expected, value)],
        ItemEdit::SetReplace { old, new } if old == new => Vec::new(),
        ItemEdit::SetReplace { old, new } => {
            vec![Cmd::srem(Raw(key), Raw(old)), Cmd::sadd(Raw(key), Raw(new))]
        }
        ItemEdit::HashSet { field, value } => vec![Cmd::hset(Raw(key), Raw(field), Raw(value))],
        ItemEdit::ZsetScore { member, score } => vec![Cmd::zadd(Raw(key), Raw(member), *score)],
    }
}

/// `SREM` / `HDEL` / `ZREM`, or for a LIST element a script: the `LSET`
/// tombstone + `LREM`, only while the element at the index is still the one
/// shown.
#[must_use]
pub fn delete_item(key: &str, delete: &ItemDelete) -> Vec<Cmd> {
    match delete {
        ItemDelete::ListIndex { index, expected } => vec![list_script(
            LIST_DELETE_SCRIPT,
            key,
            *index,
            expected,
            LIST_DELETE_SENTINEL,
        )],
        ItemDelete::SetMember(member) => vec![Cmd::srem(Raw(key), Raw(member))],
        ItemDelete::HashField(field) => vec![Cmd::hdel(Raw(key), Raw(field))],
        ItemDelete::ZsetMember(member) => vec![Cmd::zrem(Raw(key), Raw(member))],
    }
}

/// The LIST writes for a server that refuses `EVAL` (an ACL without
/// scripting): `LINDEX` is compared by the caller first, then these run in
/// one `MULTI`. Another client can still get in between the two steps.
#[must_use]
pub fn list_set_unscripted(key: &str, index: usize, value: &str) -> Vec<Cmd> {
    vec![Cmd::lset(Raw(key), list_index(index), Raw(value))]
}

/// See [`list_set_unscripted`]: the tombstone + `LREM` pair.
#[must_use]
pub fn list_delete_unscripted(key: &str, index: usize) -> Vec<Cmd> {
    vec![
        Cmd::lset(Raw(key), list_index(index), LIST_DELETE_SENTINEL),
        Cmd::lrem(Raw(key), 1, LIST_DELETE_SENTINEL),
    ]
}

/// `PUBLISH channel payload`.
#[must_use]
pub fn publish(channel: &str, payload: &str) -> Vec<Cmd> {
    vec![Cmd::publish(Raw(channel), payload)]
}
