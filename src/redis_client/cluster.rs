//! The cluster's masters, from `CLUSTER SHARDS` (Redis 7+) or `CLUSTER SLOTS`.
//!
//! Replies are read from raw [`Value`]s, since RESP2 and RESP3 shape them
//! differently and a node may leave fields out.

use redis::Value;

/// One master and the slots it serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MasterNode {
    /// Node id; `host:port` on a server too old to report one.
    pub id: String,
    pub host: String,
    pub port: u16,
    /// Inclusive slot ranges, sorted.
    pub slots: Vec<(u16, u16)>,
}

impl MasterNode {
    /// A slot this node owns, which routes a command to it and keeps doing
    /// so whatever address the client knows the node by.
    #[must_use]
    pub fn route_slot(&self) -> Option<u16> {
        self.slots.first().map(|&(start, _)| start)
    }
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::BulkString(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        Value::SimpleString(text) | Value::VerbatimString { text, .. } => Some(text.clone()),
        _ => None,
    }
}

fn int(value: &Value) -> Option<i64> {
    match value {
        Value::Int(n) => Some(*n),
        other => text(other)?.parse().ok(),
    }
}

/// The name / value pairs of a RESP3 map or a RESP2 flat array.
fn fields(value: &Value) -> Vec<(String, &Value)> {
    match value {
        Value::Map(pairs) => pairs
            .iter()
            .filter_map(|(name, value)| Some((text(name)?, value)))
            .collect(),
        Value::Array(items) => items
            .chunks_exact(2)
            .filter_map(|pair| Some((text(&pair[0])?, &pair[1])))
            .collect(),
        _ => Vec::new(),
    }
}

fn field<'a>(fields: &'a [(String, &'a Value)], name: &str) -> Option<&'a Value> {
    fields
        .iter()
        .find(|(field, _)| field == name)
        .map(|(_, value)| *value)
}

fn slot(value: &Value) -> Option<u16> {
    int(value).and_then(|n| u16::try_from(n).ok())
}

fn sorted(mut masters: Vec<MasterNode>) -> Vec<MasterNode> {
    for master in &mut masters {
        master.slots.sort_unstable();
    }
    masters.retain(|master| !master.slots.is_empty());
    masters.sort_by_key(MasterNode::route_slot);
    masters
}

/// The healthy master of each shard in a `CLUSTER SHARDS` reply.
#[must_use]
pub fn parse_shards(reply: &Value) -> Vec<MasterNode> {
    let Value::Array(shards) = reply else {
        return Vec::new();
    };
    let masters = shards.iter().filter_map(|shard| {
        let shard = fields(shard);
        let Some(Value::Array(raw_slots)) = field(&shard, "slots") else {
            return None;
        };
        let slots: Vec<(u16, u16)> = raw_slots
            .chunks_exact(2)
            .filter_map(|range| Some((slot(&range[0])?, slot(&range[1])?)))
            .collect();
        let Some(Value::Array(nodes)) = field(&shard, "nodes") else {
            return None;
        };
        nodes.iter().find_map(|node| {
            let node = fields(node);
            let role = field(&node, "role").and_then(text)?;
            let health = field(&node, "health").and_then(text);
            if role != "master" || health.as_deref() == Some("fail") {
                return None;
            }
            let port = field(&node, "port")
                .or_else(|| field(&node, "tls-port"))
                .and_then(int)
                .and_then(|port| u16::try_from(port).ok())?;
            let host = field(&node, "endpoint")
                .and_then(text)
                .filter(|host| !host.is_empty() && host != "?")
                .or_else(|| field(&node, "ip").and_then(text))?;
            let id = field(&node, "id")
                .and_then(text)
                .unwrap_or_else(|| format!("{host}:{port}"));
            Some(MasterNode {
                id,
                host,
                port,
                slots: slots.clone(),
            })
        })
    });
    sorted(masters.collect())
}

/// The masters in a `CLUSTER SLOTS` reply, one entry per node.
#[must_use]
pub fn parse_slots(reply: &Value) -> Vec<MasterNode> {
    let Value::Array(ranges) = reply else {
        return Vec::new();
    };
    let mut masters: Vec<MasterNode> = Vec::new();
    for range in ranges {
        let Value::Array(range) = range else {
            continue;
        };
        let [start, end, Value::Array(node), ..] = range.as_slice() else {
            continue;
        };
        let (Some(start), Some(end)) = (slot(start), slot(end)) else {
            continue;
        };
        let Some(port) = node
            .get(1)
            .and_then(int)
            .and_then(|port| u16::try_from(port).ok())
        else {
            continue;
        };
        let host = node.first().and_then(text).unwrap_or_default();
        let id = node
            .get(2)
            .and_then(text)
            .unwrap_or_else(|| format!("{host}:{port}"));
        match masters.iter_mut().find(|master| master.id == id) {
            Some(master) => master.slots.push((start, end)),
            None => masters.push(MasterNode {
                id,
                host,
                port,
                slots: vec![(start, end)],
            }),
        }
    }
    sorted(masters)
}

#[cfg(test)]
mod tests {
    use redis::Value;

    use super::{parse_shards, parse_slots, MasterNode};

    fn bulk(text: &str) -> Value {
        Value::BulkString(text.as_bytes().to_vec())
    }

    fn node(id: &str, port: i64, role: &str, health: &str) -> Value {
        Value::Array(vec![
            bulk("id"),
            bulk(id),
            bulk("port"),
            Value::Int(port),
            bulk("ip"),
            bulk("10.0.0.1"),
            bulk("endpoint"),
            bulk("db.example"),
            bulk("role"),
            bulk(role),
            bulk("health"),
            bulk(health),
        ])
    }

    fn shard(slots: &[i64], nodes: Vec<Value>) -> Value {
        Value::Array(vec![
            bulk("slots"),
            Value::Array(slots.iter().map(|&s| Value::Int(s)).collect()),
            bulk("nodes"),
            Value::Array(nodes),
        ])
    }

    fn master(id: &str, port: u16, slots: Vec<(u16, u16)>) -> MasterNode {
        MasterNode {
            id: id.into(),
            host: "db.example".into(),
            port,
            slots,
        }
    }

    #[test]
    fn shards_keep_the_healthy_master_in_slot_order() {
        let reply = Value::Array(vec![
            shard(
                &[5461, 10922],
                vec![
                    node("b-replica", 7004, "replica", "online"),
                    node("b", 7001, "master", "online"),
                ],
            ),
            shard(
                &[0, 100, 200, 5460],
                vec![node("a", 7000, "master", "online")],
            ),
            // Its master failed and no replica took over yet.
            shard(&[10923, 16383], vec![node("c", 7002, "master", "fail")]),
        ]);
        assert_eq!(
            parse_shards(&reply),
            vec![
                master("a", 7000, vec![(0, 100), (200, 5460)]),
                master("b", 7001, vec![(5461, 10922)]),
            ]
        );
    }

    #[test]
    fn shards_read_a_resp3_map() {
        let reply = Value::Array(vec![Value::Map(vec![
            (
                bulk("slots"),
                Value::Array(vec![Value::Int(0), Value::Int(9)]),
            ),
            (
                bulk("nodes"),
                Value::Array(vec![Value::Map(vec![
                    (bulk("id"), bulk("a")),
                    (bulk("tls-port"), Value::Int(7000)),
                    (bulk("endpoint"), bulk("")),
                    (bulk("ip"), bulk("db.example")),
                    (bulk("role"), bulk("master")),
                ])]),
            ),
        ])]);
        assert_eq!(parse_shards(&reply), vec![master("a", 7000, vec![(0, 9)])]);
    }

    #[test]
    fn slots_group_ranges_by_node() {
        let range = |start, end, id: &str, port| {
            Value::Array(vec![
                Value::Int(start),
                Value::Int(end),
                Value::Array(vec![bulk("db.example"), Value::Int(port), bulk(id)]),
                Value::Array(vec![bulk("replica"), Value::Int(9), bulk("r")]),
            ])
        };
        let reply = Value::Array(vec![
            range(200, 300, "b", 7001),
            range(0, 99, "a", 7000),
            range(100, 199, "b", 7001),
        ]);
        assert_eq!(
            parse_slots(&reply),
            vec![
                master("a", 7000, vec![(0, 99)]),
                master("b", 7001, vec![(100, 199), (200, 300)]),
            ]
        );
    }

    #[test]
    fn unexpected_replies_give_no_masters() {
        for reply in [Value::Nil, Value::Int(1), Value::Array(vec![Value::Nil])] {
            assert!(parse_shards(&reply).is_empty());
            assert!(parse_slots(&reply).is_empty());
        }
    }
}
