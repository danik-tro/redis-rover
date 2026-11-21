//! Stream values, parsed from raw replies with [`Text`], so field names and
//! values that aren't UTF-8 stay exact.

use redis::{FromRedisValue, Value};

use crate::redis_client::bytes::Text;

/// One stream entry. A field may repeat, so fields are pairs in stream order.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StreamEntry {
    pub id: String,
    pub fields: Vec<(String, String)>,
}

impl StreamEntry {
    /// The millisecond part of the ID.
    #[must_use]
    pub fn millis(&self) -> Option<u64> {
        self.id.split_once('-')?.0.parse().ok()
    }

    /// `{"id": …, "fields": {…}}`. Fields that repeat can't be object keys,
    /// so such an entry has `[[field, value], …]` instead.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::{json, Map, Value};
        let object: Map<String, Value> = self
            .fields
            .iter()
            .map(|(f, v)| (f.clone(), Value::String(v.clone())))
            .collect();
        if object.len() == self.fields.len() {
            json!({ "id": self.id, "fields": object })
        } else {
            json!({ "id": self.id, "fields": self.fields })
        }
    }
}

/// One `XINFO GROUPS` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupInfo {
    pub name: String,
    pub consumers: u64,
    pub pending: u64,
    pub last_delivered: String,
    /// Redis 7+, and `None` there too when the server can't tell.
    pub lag: Option<u64>,
}

/// One `XINFO CONSUMERS` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumerInfo {
    pub name: String,
    pub pending: u64,
    pub idle_ms: u64,
}

/// One `XPENDING key group - + n` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEntry {
    pub id: String,
    pub consumer: String,
    pub idle_ms: u64,
    pub deliveries: u64,
}

/// One page of a stream, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StreamValue {
    pub entries: Vec<StreamEntry>,
    /// `XLEN`.
    pub length: usize,
    /// `last-generated-id`; empty when `XINFO STREAM` is refused.
    pub last_id: String,
    /// `None` when `XINFO GROUPS` is refused.
    pub groups: Option<Vec<GroupInfo>>,
}

/// A group opened with Enter.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GroupDetail {
    pub key: String,
    pub group: String,
    pub consumers: Vec<ConsumerInfo>,
    pub pending: Vec<PendingEntry>,
    /// A refused `XINFO CONSUMERS` / `XPENDING`, shown inline.
    pub error: Option<String>,
}

/// The ID right below `id`, as an inclusive `XREVRANGE` start: one code path
/// on every server with streams (exclusive ranges need 6.2). `None` below
/// `0-0`, or for something that is not an ID.
#[must_use]
pub fn prev_id(id: &str) -> Option<String> {
    let (ms, seq) = id.split_once('-')?;
    let (ms, seq) = (ms.parse::<u64>().ok()?, seq.parse::<u64>().ok()?);
    match (ms, seq) {
        (0, 0) => None,
        (ms, 0) => Some(format!("{}-{}", ms - 1, u64::MAX)),
        (ms, seq) => Some(format!("{ms}-{}", seq - 1)),
    }
}

fn text(value: Value) -> Option<String> {
    Text::from_redis_value(value).ok().map(|t| t.0)
}

/// A flat `[k, v, …]` array (RESP2) or a map (RESP3) as pairs.
fn pairs(value: Value) -> Option<Vec<(String, Value)>> {
    match value {
        Value::Array(items) => {
            let mut items = items.into_iter();
            let mut out = Vec::with_capacity(items.len() / 2);
            while let (Some(k), Some(v)) = (items.next(), items.next()) {
                out.push((text(k)?, v));
            }
            Some(out)
        }
        Value::Map(entries) => entries
            .into_iter()
            .map(|(k, v)| Some((text(k)?, v)))
            .collect(),
        _ => None,
    }
}

fn take(fields: &mut Vec<(String, Value)>, name: &str) -> Option<Value> {
    let index = fields.iter().position(|(k, _)| k == name)?;
    Some(fields.swap_remove(index).1)
}

fn take_u64(fields: &mut Vec<(String, Value)>, name: &str) -> Option<u64> {
    u64::from_redis_value(take(fields, name)?).ok()
}

fn rows(reply: Value) -> Vec<Value> {
    match reply {
        Value::Array(items) => items,
        _ => Vec::new(),
    }
}

/// The entries of an `XRANGE` / `XREVRANGE` reply. A malformed one is skipped.
#[must_use]
pub fn parse_entries(reply: Value) -> Vec<StreamEntry> {
    rows(reply).into_iter().filter_map(parse_entry).collect()
}

fn parse_entry(item: Value) -> Option<StreamEntry> {
    let Value::Array(parts) = item else {
        return None;
    };
    let mut parts = parts.into_iter();
    let id = text(parts.next()?)?;
    let fields = pairs(parts.next()?)?
        .into_iter()
        .map(|(field, value)| Some((field, text(value)?)))
        .collect::<Option<Vec<_>>>()?;
    Some(StreamEntry { id, fields })
}

/// `last-generated-id` of an `XINFO STREAM` reply.
#[must_use]
pub fn parse_last_id(reply: Value) -> Option<String> {
    let mut fields = pairs(reply)?;
    text(take(&mut fields, "last-generated-id")?)
}

/// The rows of `XINFO GROUPS`.
#[must_use]
pub fn parse_groups(reply: Value) -> Vec<GroupInfo> {
    rows(reply)
        .into_iter()
        .filter_map(|row| {
            let mut f = pairs(row)?;
            Some(GroupInfo {
                name: text(take(&mut f, "name")?)?,
                consumers: take_u64(&mut f, "consumers")?,
                pending: take_u64(&mut f, "pending")?,
                last_delivered: text(take(&mut f, "last-delivered-id")?)?,
                lag: take_u64(&mut f, "lag"),
            })
        })
        .collect()
}

/// The rows of `XINFO CONSUMERS`.
#[must_use]
pub fn parse_consumers(reply: Value) -> Vec<ConsumerInfo> {
    rows(reply)
        .into_iter()
        .filter_map(|row| {
            let mut f = pairs(row)?;
            Some(ConsumerInfo {
                name: text(take(&mut f, "name")?)?,
                pending: take_u64(&mut f, "pending")?,
                idle_ms: take_u64(&mut f, "idle")?,
            })
        })
        .collect()
}

/// The rows of `XPENDING key group - + n`.
#[must_use]
pub fn parse_pending(reply: Value) -> Vec<PendingEntry> {
    rows(reply)
        .into_iter()
        .filter_map(|row| {
            let Value::Array(parts) = row else {
                return None;
            };
            let mut parts = parts.into_iter();
            Some(PendingEntry {
                id: text(parts.next()?)?,
                consumer: text(parts.next()?)?,
                idle_ms: u64::from_redis_value(parts.next()?).ok()?,
                deliveries: u64::from_redis_value(parts.next()?).ok()?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(text: &[u8]) -> Value {
        Value::BulkString(text.to_vec())
    }

    #[test]
    fn prev_id_covers_the_edges() {
        let max = u64::MAX;
        for (id, want) in [
            ("5-3".to_owned(), Some("5-2".to_owned())),
            ("5-1".to_owned(), Some("5-0".to_owned())),
            ("5-0".to_owned(), Some(format!("4-{max}"))),
            ("1-0".to_owned(), Some(format!("0-{max}"))),
            ("0-1".to_owned(), Some("0-0".to_owned())),
            ("0-0".to_owned(), None),
            (format!("{max}-{max}"), Some(format!("{max}-{}", max - 1))),
            (format!("{max}-0"), Some(format!("{}-{max}", max - 1))),
            ("abc".to_owned(), None),
            ("5".to_owned(), None),
            ("5-x".to_owned(), None),
            ("-1-2".to_owned(), None),
            ("18446744073709551616-0".to_owned(), None),
        ] {
            assert_eq!(prev_id(&id), want, "{id}");
        }
    }

    #[test]
    fn entries_keep_non_utf8_and_repeated_fields() {
        let reply = Value::Array(vec![
            Value::Array(vec![
                bulk(b"7-1"),
                Value::Array(vec![bulk(b"a"), bulk(&[0xff]), bulk(b"a"), bulk(b"2")]),
            ]),
            Value::Nil,
            Value::Array(vec![bulk(b"7-0"), Value::Nil]),
        ]);
        let entries = parse_entries(reply);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].millis(), Some(7));
        assert_eq!(
            entries[0].fields[0].1,
            crate::redis_client::bytes::text_from_bytes(&[0xff])
        );
        assert_eq!(entries[0].fields[1], ("a".to_owned(), "2".to_owned()));
    }

    #[test]
    fn json_keeps_repeated_fields() {
        let entry = StreamEntry {
            id: "1-0".into(),
            fields: vec![("a".into(), "1".into()), ("a".into(), "2".into())],
        };
        assert_eq!(
            entry.to_json(),
            serde_json::json!({"id": "1-0", "fields": [["a", "1"], ["a", "2"]]})
        );
        let entry = StreamEntry {
            id: "1-0".into(),
            fields: vec![("a".into(), "1".into())],
        };
        assert_eq!(
            entry.to_json(),
            serde_json::json!({"id": "1-0", "fields": {"a": "1"}})
        );
    }

    #[test]
    fn groups_parse_on_both_protocols_with_and_without_lag() {
        let flat = Value::Array(vec![Value::Array(vec![
            bulk(b"name"),
            bulk(b"g1"),
            bulk(b"consumers"),
            Value::Int(2),
            bulk(b"pending"),
            Value::Int(5),
            bulk(b"last-delivered-id"),
            bulk(b"9-0"),
        ])]);
        assert_eq!(
            parse_groups(flat),
            [GroupInfo {
                name: "g1".into(),
                consumers: 2,
                pending: 5,
                last_delivered: "9-0".into(),
                lag: None,
            }]
        );
        let map = Value::Array(vec![Value::Map(vec![
            (bulk(b"name"), bulk(b"g2")),
            (bulk(b"consumers"), Value::Int(0)),
            (bulk(b"pending"), Value::Int(0)),
            (bulk(b"last-delivered-id"), bulk(b"0-0")),
            (bulk(b"lag"), Value::Int(3)),
        ])]);
        assert_eq!(parse_groups(map)[0].lag, Some(3));
        let nil_lag = Value::Array(vec![Value::Array(vec![
            bulk(b"name"),
            bulk(b"g3"),
            bulk(b"consumers"),
            Value::Int(0),
            bulk(b"pending"),
            Value::Int(0),
            bulk(b"last-delivered-id"),
            bulk(b"0-0"),
            bulk(b"lag"),
            Value::Nil,
        ])]);
        assert_eq!(parse_groups(nil_lag)[0].lag, None);
    }

    #[test]
    fn consumers_and_pending_parse_and_skip_bad_rows() {
        let consumers = Value::Array(vec![
            Value::Array(vec![
                bulk(b"name"),
                bulk(b"c1"),
                bulk(b"pending"),
                Value::Int(1),
                bulk(b"idle"),
                Value::Int(250),
            ]),
            Value::Nil,
        ]);
        assert_eq!(
            parse_consumers(consumers),
            [ConsumerInfo {
                name: "c1".into(),
                pending: 1,
                idle_ms: 250,
            }]
        );
        let pending = Value::Array(vec![
            Value::Array(vec![
                bulk(b"1-0"),
                bulk(b"c1"),
                Value::Int(250),
                Value::Int(3),
            ]),
            Value::Array(vec![bulk(b"1-1")]),
        ]);
        assert_eq!(
            parse_pending(pending),
            [PendingEntry {
                id: "1-0".into(),
                consumer: "c1".into(),
                idle_ms: 250,
                deliveries: 3,
            }]
        );
    }

    #[test]
    fn last_id_comes_from_xinfo_stream() {
        let reply = Value::Array(vec![
            bulk(b"length"),
            Value::Int(1),
            bulk(b"last-generated-id"),
            bulk(b"9-9"),
        ]);
        assert_eq!(parse_last_id(reply).as_deref(), Some("9-9"));
        assert_eq!(parse_last_id(Value::Nil), None);
    }
}
