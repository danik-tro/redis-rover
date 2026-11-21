//! The cluster's masters, from `CLUSTER SHARDS` (Redis 7+) or `CLUSTER SLOTS`.
//!
//! Replies are read from raw [`Value`]s, since RESP2 and RESP3 shape them
//! differently and a node may leave fields out.

use redis::Value;

use super::bytes::bytes_from_text;

/// Number of hash slots in a cluster.
const SLOTS: u16 = 16384;

/// CRC16-CCITT (XMODEM), the checksum Redis Cluster hashes keys with.
fn crc16(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in bytes {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 == 0 {
                crc << 1
            } else {
                (crc << 1) ^ 0x1021
            };
        }
    }
    crc
}

/// The part of a key that is hashed: the first non-empty `{tag}`, else all.
fn hash_tag(key: &[u8]) -> &[u8] {
    let Some(open) = key.iter().position(|&b| b == b'{') else {
        return key;
    };
    let rest = &key[open + 1..];
    match rest.iter().position(|&b| b == b'}') {
        Some(close) if close > 0 => &rest[..close],
        _ => key,
    }
}

/// The hash slot of `key`, as `CLUSTER KEYSLOT` computes it.
#[must_use]
pub fn key_slot(key: &str) -> u16 {
    crc16(hash_tag(&bytes_from_text(key))) % SLOTS
}

/// The keys one master holds, by their position in the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeGroup {
    /// Routes the group's pipeline to the master.
    pub slot: u16,
    pub indices: Vec<usize>,
}

/// Group `keys` by the master that owns their slot, masters in `masters`
/// order. A key whose slot no master claims gets a group of its own slot, so
/// the client still routes it and the server's answer is what is reported.
#[must_use]
pub fn group_by_node<K: AsRef<str>>(keys: &[K], masters: &[MasterNode]) -> Vec<NodeGroup> {
    let mut groups: Vec<NodeGroup> = Vec::new();
    for (index, key) in keys.iter().enumerate() {
        let slot = key_slot(key.as_ref());
        let route = masters
            .iter()
            .find(|master| {
                master
                    .slots
                    .iter()
                    .any(|&(start, end)| (start..=end).contains(&slot))
            })
            .and_then(MasterNode::route_slot)
            .unwrap_or(slot);
        match groups.iter_mut().find(|group| group.slot == route) {
            Some(group) => group.indices.push(index),
            None => groups.push(NodeGroup {
                slot: route,
                indices: vec![index],
            }),
        }
    }
    groups
}

/// `from` and `to` can't be renamed / copied into each other: the refusal,
/// with both slot numbers and how to fix it. `None` when they share a slot.
#[must_use]
pub fn cross_slot_refusal(from: &str, to: &str) -> Option<String> {
    let (a, b) = (key_slot(from), key_slot(to));
    (a != b).then(|| {
        format!(
            "keys are in different slots ({a} vs {b}): a hash tag puts them in one, \
             like {{tag}}:a and {{tag}}:b"
        )
    })
}

/// The preview's line naming where a write goes: `node host:port · slot n`
/// for one key, or one `host:port (n keys)` per node for several. `None`
/// without keys or before the masters are known.
#[must_use]
pub fn target_note(keys: &[&str], masters: &[MasterNode]) -> Option<String> {
    if keys.is_empty() || masters.is_empty() {
        return None;
    }
    let addr = |group: &NodeGroup| {
        masters
            .iter()
            .find(|master| master.route_slot() == Some(group.slot))
            .map_or_else(
                || "unassigned slot".to_owned(),
                |master| format!("{}:{}", master.host, master.port),
            )
    };
    let groups = group_by_node(keys, masters);
    match groups.as_slice() {
        [group] if keys.len() == 1 => {
            Some(format!("node {} · slot {}", addr(group), key_slot(keys[0])))
        }
        [group] => Some(format!("node {}", addr(group))),
        groups => {
            let nodes: Vec<String> = groups
                .iter()
                .map(|group| {
                    let count = group.indices.len();
                    let noun = if count == 1 { "key" } else { "keys" };
                    format!("{} ({count} {noun})", addr(group))
                })
                .collect();
            Some(format!("nodes {}", nodes.join(", ")))
        }
    }
}

/// `keys` by slot: what one multi-key command may carry on a cluster.
#[must_use]
pub fn by_slot(keys: &[String]) -> Vec<Vec<String>> {
    let mut slots = std::collections::BTreeMap::<u16, Vec<String>>::new();
    for key in keys {
        slots.entry(key_slot(key)).or_default().push(key.clone());
    }
    slots.into_values().collect()
}

/// What a fresh list of masters says about the one in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopologyUpdate {
    Same,
    /// Other masters or slots: the sweep restarts.
    Changed,
    /// Fewer slots covered: a master failed and no replica took over yet.
    /// Wait for the promotion rather than sweep part of the keyspace.
    Partial,
}

fn slots_covered(masters: &[MasterNode]) -> u32 {
    masters
        .iter()
        .flat_map(|master| &master.slots)
        .map(|&(start, end)| u32::from(end) - u32::from(start) + 1)
        .sum()
}

#[must_use]
pub fn topology_update(old: &[MasterNode], new: &[MasterNode]) -> TopologyUpdate {
    if old == new {
        TopologyUpdate::Same
    } else if slots_covered(new) < slots_covered(old) {
        TopologyUpdate::Partial
    } else {
        TopologyUpdate::Changed
    }
}

/// An error a topology change explains: a redirect, a cluster that is
/// electing a new master, or a node that went away.
#[must_use]
pub fn is_topology_error(err: &(dyn std::error::Error + Send + Sync + 'static)) -> bool {
    if let Some(err) = err.downcast_ref::<redis::RedisError>() {
        if err.is_io_error() || err.is_connection_dropped() || err.is_timeout() {
            return true;
        }
    }
    let text = err.to_string();
    ["MOVED", "ASK", "CLUSTERDOWN", "TRYAGAIN"]
        .iter()
        .any(|word| {
            text.split(|c: char| !c.is_ascii_alphabetic())
                .any(|w| w == *word)
        })
}

/// Put each group's replies back at their keys' positions. `None` if a group
/// answered with the wrong number of replies.
#[must_use]
pub fn merge_groups<T>(len: usize, groups: &[NodeGroup], replies: Vec<Vec<T>>) -> Option<Vec<T>> {
    let mut merged: Vec<Option<T>> = std::iter::repeat_with(|| None).take(len).collect();
    for (group, reply) in groups.iter().zip(replies) {
        if group.indices.len() != reply.len() {
            return None;
        }
        for (&index, value) in group.indices.iter().zip(reply) {
            *merged.get_mut(index)? = Some(value);
        }
    }
    merged.into_iter().collect()
}

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

    use super::{
        by_slot, cross_slot_refusal, group_by_node, is_topology_error, key_slot, merge_groups,
        parse_shards, parse_slots, target_note, topology_update, MasterNode, NodeGroup,
        TopologyUpdate,
    };

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

    /// Values from the Redis Cluster specification and `CLUSTER KEYSLOT`.
    #[test]
    fn key_slots_match_redis() {
        assert_eq!(super::crc16(b"123456789"), 0x31C3);
        for (key, slot) in [
            ("foo", 12182),
            ("bar", 5061),
            ("{user1000}.following", 3443),
            ("{user1000}.followers", 3443),
            // An empty tag doesn't count: the whole key is hashed.
            ("foo{}{bar}", 8363),
            // Only the first tag counts.
            ("foo{{bar}}zap", 4015),
            ("foo{bar}{zap}", 5061),
        ] {
            assert_eq!(key_slot(key), slot, "{key}");
        }
    }

    #[test]
    fn a_non_utf8_key_hashes_its_real_bytes() {
        let key = crate::redis_client::bytes::text_from_bytes(b"\xff\xfe");
        assert_eq!(key_slot(&key), super::crc16(b"\xff\xfe") % 16384);
    }

    fn three_masters() -> Vec<MasterNode> {
        vec![
            master("a", 7000, vec![(0, 5460)]),
            master("b", 7001, vec![(5461, 8000), (9000, 10922)]),
            master("c", 7002, vec![(10923, 16383)]),
        ]
    }

    #[test]
    fn keys_are_grouped_by_the_master_of_their_slot() {
        // foo: 12182 (c), bar: 5061 (a), {user1000}: 3443 (a), hello: 866 (a),
        // a: 15495 (c), b: 3300 (a), c: 7365 (b).
        let keys = ["foo", "bar", "{user1000}.x", "c", "a"];
        assert_eq!(
            group_by_node(&keys, &three_masters()),
            vec![
                NodeGroup {
                    slot: 10923,
                    indices: vec![0, 4],
                },
                NodeGroup {
                    slot: 0,
                    indices: vec![1, 2],
                },
                NodeGroup {
                    slot: 5461,
                    indices: vec![3],
                },
            ]
        );
    }

    #[test]
    fn a_key_in_an_unclaimed_slot_is_routed_by_its_own_slot() {
        let masters = vec![master("a", 7000, vec![(0, 100)])];
        let groups = group_by_node(&["foo"], &masters);
        assert_eq!(groups[0].slot, 12182);
    }

    #[test]
    fn a_cross_slot_move_is_refused_with_both_slots_and_a_hint() {
        let refusal = cross_slot_refusal("foo", "bar").unwrap();
        assert!(refusal.contains("12182 vs 5061"), "{refusal}");
        assert!(refusal.contains("{tag}"), "{refusal}");
        assert_eq!(cross_slot_refusal("{u1}:a", "{u1}:b"), None);
    }

    #[test]
    fn the_preview_names_the_target_node() {
        let masters = three_masters();
        assert_eq!(
            target_note(&["foo"], &masters).unwrap(),
            "node db.example:7002 · slot 12182"
        );
        assert_eq!(
            target_note(&["{user1000}:a", "{user1000}:b"], &masters).unwrap(),
            "node db.example:7000"
        );
        assert_eq!(
            target_note(&["foo", "bar", "a"], &masters).unwrap(),
            "nodes db.example:7002 (2 keys), db.example:7000 (1 key)"
        );
        assert_eq!(target_note(&[], &masters), None);
        assert_eq!(target_note(&["foo"], &[]), None, "one server");
    }

    #[test]
    fn multi_key_commands_are_split_by_slot() {
        let keys: Vec<String> = ["foo", "{foo}:1", "bar", "{bar}:1", "c"]
            .map(str::to_owned)
            .to_vec();
        let groups = by_slot(&keys);
        assert_eq!(groups.len(), 3);
        for group in &groups {
            let slot = key_slot(&group[0]);
            assert!(group.iter().all(|key| key_slot(key) == slot), "{group:?}");
        }
        assert_eq!(groups.iter().map(Vec::len).sum::<usize>(), keys.len());
    }

    #[test]
    fn a_failover_is_a_change_once_a_replica_took_over() {
        let old = three_masters();
        assert_eq!(topology_update(&old, &old), TopologyUpdate::Same);

        // The master of 0-5460 failed, nobody serves its slots yet.
        let waiting = old[1..].to_vec();
        assert_eq!(topology_update(&old, &waiting), TopologyUpdate::Partial);

        let mut promoted = old.clone();
        promoted[0].id = "a-replica".into();
        promoted[0].port = 7003;
        assert_eq!(topology_update(&old, &promoted), TopologyUpdate::Changed);

        // A reshard moves slots between the same masters.
        let mut resharded = old.clone();
        resharded[0].slots = vec![(0, 5000)];
        resharded[1].slots = vec![(5001, 8000), (9000, 10922)];
        assert_eq!(topology_update(&old, &resharded), TopologyUpdate::Changed);
    }

    #[test]
    fn redirects_and_lost_nodes_are_topology_errors() {
        let err = |text: &str| -> Box<dyn std::error::Error + Send + Sync> { text.into() };
        for text in [
            "MOVED 3999 127.0.0.1:7001",
            "CLUSTERDOWN The cluster is down",
            "TRYAGAIN",
        ] {
            assert!(is_topology_error(err(text).as_ref()), "{text}");
        }
        for text in ["WRONGTYPE Operation", "a key named BASKET failed"] {
            assert!(!is_topology_error(err(text).as_ref()), "{text}");
        }
        let io = redis::RedisError::from((redis::ErrorKind::Io, "gone"));
        assert!(is_topology_error(&io));
    }

    #[test]
    fn replies_come_back_in_page_order() {
        let groups = vec![
            NodeGroup {
                slot: 9,
                indices: vec![0, 3],
            },
            NodeGroup {
                slot: 1,
                indices: vec![2, 1],
            },
        ];
        assert_eq!(
            merge_groups(4, &groups, vec![vec!["k0", "k3"], vec!["k2", "k1"]]),
            Some(vec!["k0", "k1", "k2", "k3"])
        );
        // A short reply is an error, never a shifted page.
        assert_eq!(
            merge_groups(4, &groups, vec![vec!["k0"], vec!["k2", "k1"]]),
            None
        );
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
