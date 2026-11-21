//! `SLOWLOG GET` and the two settings behind it, parsed from raw replies so
//! arguments that aren't UTF-8 stay exact.

use redis::{FromRedisValue, Value};

use super::bytes::Text;

/// Entries asked for. The server keeps `slowlog-max-len` (128 by default).
pub const SLOWLOG_FETCH: usize = 128;

/// One `SLOWLOG GET` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlowlogEntry {
    pub id: u64,
    /// Unix time the command ran.
    pub ts_secs: u64,
    pub micros: u64,
    /// The command and its arguments, as the server cut them.
    pub args: Vec<String>,
    /// `ip:port`; empty before Redis 4.0.
    pub client: String,
    /// `CLIENT SETNAME`; empty before Redis 4.0.
    pub name: String,
}

/// One load of the screen. A refused `CONFIG` leaves its value `None`
/// (shown as `unknown`); a refused `SLOWLOG` sets `error`, shown inline.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlowlogData {
    pub entries: Vec<SlowlogEntry>,
    /// `slowlog-log-slower-than`, in microseconds.
    pub slower_than: Option<i64>,
    /// `slowlog-max-len`.
    pub max_len: Option<i64>,
    pub error: Option<String>,
}

/// The entries of a `SLOWLOG GET` reply. One with another shape is skipped.
#[must_use]
pub fn parse_entries(reply: Value) -> Vec<SlowlogEntry> {
    let Value::Array(items) = reply else {
        return Vec::new();
    };
    items.into_iter().filter_map(parse_entry).collect()
}

fn parse_entry(item: Value) -> Option<SlowlogEntry> {
    let Value::Array(fields) = item else {
        return None;
    };
    let mut fields = fields.into_iter();
    let id = u64::from_redis_value(fields.next()?).ok()?;
    let ts_secs = u64::from_redis_value(fields.next()?).ok()?;
    let micros = u64::from_redis_value(fields.next()?).ok()?;
    let Value::Array(raw_args) = fields.next()? else {
        return None;
    };
    let args = raw_args
        .into_iter()
        .map(|arg| Text::from_redis_value(arg).ok().map(|t| t.0))
        .collect::<Option<Vec<_>>>()?;
    let mut text = || {
        fields
            .next()
            .and_then(|v| Text::from_redis_value(v).ok())
            .map_or_else(String::new, |t| t.0)
    };
    let client = text();
    let name = text();
    Some(SlowlogEntry {
        id,
        ts_secs,
        micros,
        args,
        client,
        name,
    })
}

/// The value in a one-setting `CONFIG GET` reply (an array on RESP2, a map
/// on RESP3).
#[must_use]
pub fn parse_config_int(reply: Value) -> Option<i64> {
    let value = match reply {
        Value::Array(mut items) if items.len() == 2 => items.pop()?,
        Value::Map(mut pairs) if pairs.len() == 1 => pairs.pop()?.1,
        _ => return None,
    };
    i64::from_redis_value(value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(text: &[u8]) -> Value {
        Value::BulkString(text.to_vec())
    }

    fn entry(fields: Vec<Value>) -> Value {
        Value::Array(vec![Value::Array(fields)])
    }

    #[test]
    fn parses_a_full_entry() {
        let parsed = parse_entries(entry(vec![
            Value::Int(14),
            Value::Int(1_700_000_000),
            Value::Int(1520),
            Value::Array(vec![bulk(b"SET"), bulk(b"a"), bulk(&[0xff, b'x'])]),
            bulk(b"127.0.0.1:5000"),
            bulk(b"worker"),
        ]));
        assert_eq!(parsed.len(), 1);
        let e = &parsed[0];
        assert_eq!((e.id, e.ts_secs, e.micros), (14, 1_700_000_000, 1520));
        assert_eq!(e.args[..2], ["SET", "a"]);
        assert_eq!(
            e.args[2],
            crate::redis_client::bytes::text_from_bytes(&[0xff, b'x'])
        );
        assert_eq!(
            (e.client.as_str(), e.name.as_str()),
            ("127.0.0.1:5000", "worker")
        );
    }

    #[test]
    fn an_old_server_has_no_client_fields() {
        let parsed = parse_entries(entry(vec![
            Value::Int(1),
            Value::Int(2),
            Value::Int(3),
            Value::Array(vec![bulk(b"GET")]),
        ]));
        assert_eq!(parsed[0].client, "");
        assert_eq!(parsed[0].name, "");
    }

    #[test]
    fn a_bad_entry_is_skipped_and_the_rest_kept() {
        let good = Value::Array(vec![
            Value::Int(1),
            Value::Int(2),
            Value::Int(3),
            Value::Array(vec![bulk(b"GET")]),
        ]);
        let reply = Value::Array(vec![
            Value::Nil,
            Value::Array(vec![Value::Int(1)]),
            Value::Array(vec![
                Value::Int(1),
                Value::Int(2),
                Value::Int(3),
                Value::Nil,
            ]),
            good,
        ]);
        assert_eq!(parse_entries(reply).len(), 1);
        assert!(parse_entries(Value::Nil).is_empty());
    }

    #[test]
    fn config_replies_on_both_protocols() {
        let array = Value::Array(vec![bulk(b"slowlog-max-len"), bulk(b"128")]);
        assert_eq!(parse_config_int(array), Some(128));
        let map = Value::Map(vec![(bulk(b"slowlog-log-slower-than"), bulk(b"-1"))]);
        assert_eq!(parse_config_int(map), Some(-1));
        assert_eq!(parse_config_int(Value::Array(vec![])), None);
        let text = Value::Array(vec![bulk(b"x"), bulk(b"nope")]);
        assert_eq!(parse_config_int(text), None);
    }
}
