mod keyspace;
mod type_filter;
mod value;
mod view;

pub use keyspace::{KeyspaceState, SweepApply, SweepRequest, SweepResult};
pub use type_filter::{TypeFilter, TypeScan, STANDARD_TYPES};
pub use value::{
    json_path, zset_entry_json, JsonField, JsonPreview, JsonPreviewKind, LoadedValue, ScanSweep,
    ValuePageInfo, ValueState, DEFAULT_JSON_PREVIEW_BYTES, JSON_PREVIEW_ITEMS,
    STRING_PREVIEW_BYTES,
};
pub use view::{Sort, SortKey};

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};

use crate::config;
use ratatui::{
    style::Stylize,
    text::{Span, Text},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommonInfo {
    pub os: String,
    pub version: String,
    pub memory: String,
    pub total_memory: String,
    pub clients: String,
    pub max_clients: String,
}

/// Build from the `field -> value` pairs of an `INFO` reply. Missing fields
/// become empty strings so a partial reply still renders.
impl From<HashMap<&str, &str>> for CommonInfo {
    fn from(map: HashMap<&str, &str>) -> Self {
        let get = |field: &str| map.get(field).copied().unwrap_or_default().to_string();
        Self {
            os: get("os"),
            version: get("redis_version"),
            memory: get("used_memory_human"),
            total_memory: get("total_system_memory_human"),
            clients: get("connected_clients"),
            max_clients: get("maxclients"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedisInfo {
    pub common: CommonInfo,
    /// Key count per DB, from the `# Keyspace` section (`db3:keys=120,...`).
    /// Empty DBs are not listed by Redis.
    pub keyspace: BTreeMap<i64, u64>,
}

impl From<HashMap<&str, &str>> for RedisInfo {
    fn from(map: HashMap<&str, &str>) -> Self {
        let keyspace = map
            .iter()
            .filter_map(|(field, value)| Some((parse_db_field(field)?, parse_db_keys(value)?)))
            .collect();
        Self {
            common: CommonInfo::from(map),
            keyspace,
        }
    }
}

/// `db3` → `3`.
fn parse_db_field(field: &str) -> Option<i64> {
    field.strip_prefix("db")?.parse().ok()
}

/// `keys=120,expires=4,avg_ttl=0` → `120`.
fn parse_db_keys(value: &str) -> Option<u64> {
    value
        .split(',')
        .find_map(|part| part.strip_prefix("keys="))?
        .parse()
        .ok()
}

impl RedisInfo {
    /// Number of keys in `db` (0 if `INFO` doesn't list it).
    #[must_use]
    pub fn db_keys(&self, db: i64) -> u64 {
        self.keyspace.get(&db).copied().unwrap_or_default()
    }

    #[must_use]
    pub fn redis_version(&self) -> String {
        format!("Redis version: {}", self.common.version)
    }

    #[must_use]
    pub fn os(&self) -> String {
        format!("OS: {}", self.common.os)
    }

    #[must_use]
    pub fn memory(&self) -> String {
        format!("RAM: {}/{}", self.common.memory, self.common.total_memory)
    }

    #[must_use]
    pub fn clients(&self) -> String {
        format!(
            "Connected clients: {}/{}",
            self.common.clients, self.common.max_clients
        )
    }
}

/// One loaded page of a key's value. Collections hold only the current page,
/// in a stable order, so a row index always points at the element on screen.
#[derive(Debug, Clone)]
pub enum KeyValue {
    /// The first [`STRING_PREVIEW_BYTES`] of the value. Truncated when
    /// `value.len() < total_len` (`total_len` is `STRLEN`, in bytes).
    String {
        value: String,
        total_len: usize,
    },
    List(Vec<String>),
    Set(Vec<String>),
    Hash(Vec<(String, String)>),
    Zset(Vec<(String, f64)>), // Tuple of value and score
    /// A `RedisJSON` document (`JSON.GET`), or the opened field of one.
    Json(serde_json::Value),
    /// The shape of a `RedisJSON` document (or field) above
    /// `keyspace.json_preview_bytes`, read without fetching it whole (#49).
    JsonPreview(JsonPreview),
    Unknown,
}

/// The element payload appended to an existing collection via the in-collection
/// add-item command (`i`). The variant must match the focused key's type.
#[derive(Debug, Clone)]
pub enum KeyItem {
    ListValue(String),
    SetMember(String),
    HashField { field: String, value: String },
    ZsetMember { member: String, score: f64 },
}

/// An in-place edit of a single collection element, produced by the edit-item
/// overlay (`e` inside a value). Edits change value/score only — never the
/// element's identity — except SET, whose member *is* its value (a guarded
/// replace).
#[derive(Debug, Clone)]
pub enum ItemEdit {
    /// `LSET key index value` — replace the LIST element at `index`.
    ListSet { index: usize, value: String },
    /// `SREM old` + `SADD new` — replace a SET member. Rejected if `new` already
    /// exists (no silent clobber).
    SetReplace { old: String, new: String },
    /// `HSET key field value` — set a HASH field's value (field fixed).
    HashSet { field: String, value: String },
    /// `ZADD key score member` — set a ZSET member's score (member fixed).
    ZsetScore { member: String, score: f64 },
}

/// Deletion of a single collection element, produced by the delete-item confirm
/// (`d` inside a value). Removes exactly the selected element, not the key.
#[derive(Debug, Clone)]
pub enum ItemDelete {
    /// Delete the LIST element at `index` (Redis has no delete-by-index, so this
    /// uses the `LSET` tombstone + `LREM` trick).
    ListIndex(usize),
    /// `SREM key member`.
    SetMember(String),
    /// `HDEL key field`.
    HashField(String),
    /// `ZREM key member`.
    ZsetMember(String),
}

/// A fully-specified new key to create, produced by the add-key wizard. Carries
/// the type and at least one seed element (Redis cannot store an empty
/// collection). JSON is intentionally excluded — the wizard cannot create it.
#[derive(Debug, Clone)]
pub enum NewKeySpec {
    String(String),
    List(Vec<String>),
    Set(Vec<String>),
    Hash(Vec<(String, String)>),
    Zset(Vec<(String, f64)>),
}

#[derive(Debug, Clone)]
pub struct KeyMeta {
    pub key: String,
    pub r_type: RedisType,
    pub size: u128,
    pub ttl: isize,
    #[allow(dead_code)]
    pub value: KeyValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedisType {
    Set,
    List,
    Hash,
    Zset,
    Json,
    String,
    Stream,
    /// Any other `TYPE` reply (e.g. a module type such as `MBbloom--`), kept
    /// verbatim so the UI can show the real name.
    Unknown(String),
}

/// Width of the type badge, in characters.
pub const TYPE_BADGE_WIDTH: usize = 6;

impl RedisType {
    fn from_str(value: &str) -> Self {
        match value {
            "string" => Self::String,
            "hash" => Self::Hash,
            "set" => Self::Set,
            "zset" => Self::Zset,
            // RedisJSON reports its module type name.
            "ReJSON-RL" | "json" => Self::Json,
            "list" => Self::List,
            "stream" => Self::Stream,
            other => Self::Unknown(other.to_owned()),
        }
    }

    /// Full type name, as `TYPE` reports it (`ReJSON-RL` shows as `json`).
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::String => "string",
            Self::Hash => "hash",
            Self::Set => "set",
            Self::Zset => "zset",
            Self::Json => "json",
            Self::List => "list",
            Self::Stream => "stream",
            Self::Unknown(name) if name.is_empty() => "?",
            Self::Unknown(name) => name,
        }
    }

    /// Short label for the type badge, at most [`TYPE_BADGE_WIDTH`] chars.
    /// Long unknown names are cut and end with `…`.
    #[must_use]
    pub fn badge_label(&self) -> Cow<'_, str> {
        match self {
            Self::String => Cow::Borrowed("str"),
            Self::Unknown(_) => {
                let name = self.name();
                if name.chars().count() <= TYPE_BADGE_WIDTH {
                    Cow::Borrowed(name)
                } else {
                    let cut: String = name.chars().take(TYPE_BADGE_WIDTH - 1).collect();
                    Cow::Owned(format!("{cut}…"))
                }
            }
            other => Cow::Borrowed(other.name()),
        }
    }
}

impl From<&RedisType> for Text<'_> {
    /// Lowercase type label in the type's palette color.
    fn from(value: &RedisType) -> Self {
        let palette = &config::get().keyspace;
        let color = match value {
            RedisType::String => palette.string,
            RedisType::Json => palette.json,
            RedisType::List => palette.list,
            RedisType::Set => palette.set,
            RedisType::Zset => palette.zset,
            RedisType::Hash => palette.hash,
            RedisType::Stream => palette.stream,
            RedisType::Unknown(_) => palette.unknown,
        };
        Span::raw(value.badge_label().into_owned())
            .fg(color)
            .bold()
            .into()
    }
}

impl<'a, T> From<T> for RedisType
where
    T: Into<Cow<'a, str>>,
{
    #[inline]
    fn from(value: T) -> Self {
        Self::from_str(value.into().as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redis_info_from_map_reads_all_fields() {
        let map = HashMap::from([
            ("os", "Linux 6.1"),
            ("redis_version", "7.2.4"),
            ("used_memory_human", "1.5M"),
            ("total_system_memory_human", "16G"),
            ("connected_clients", "3"),
            ("maxclients", "10000"),
            ("uptime_in_seconds", "120"),
        ]);

        let info = RedisInfo::from(map);

        assert_eq!(
            info.common,
            CommonInfo {
                os: "Linux 6.1".into(),
                version: "7.2.4".into(),
                memory: "1.5M".into(),
                total_memory: "16G".into(),
                clients: "3".into(),
                max_clients: "10000".into(),
            }
        );
    }

    #[test]
    fn redis_info_from_map_defaults_missing_fields() {
        let map = HashMap::from([("redis_version", "7.2.4")]);

        let info = RedisInfo::from(map);

        assert_eq!(info.common.version, "7.2.4");
        assert_eq!(info.common.os, "");
        assert_eq!(info.common.max_clients, "");
    }

    #[test]
    fn redis_info_reads_keyspace_section() {
        let raw =
            "# Keyspace\r\ndb0:keys=12,expires=1,avg_ttl=0\r\ndb3:keys=7,expires=0,avg_ttl=0\r\n";
        let info = RedisInfo::from(crate::redis_client::client::parse_info(raw));
        assert_eq!(info.keyspace, BTreeMap::from([(0, 12), (3, 7)]));
        assert_eq!(info.db_keys(3), 7);
        assert_eq!(info.db_keys(5), 0);
    }

    #[test]
    fn type_replies_map_to_variants() {
        assert_eq!(RedisType::from("ReJSON-RL"), RedisType::Json);
        assert_eq!(RedisType::from("json"), RedisType::Json);
        assert_eq!(RedisType::from("stream"), RedisType::Stream);
        assert_eq!(RedisType::from("hash"), RedisType::Hash);
        assert_eq!(
            RedisType::from("MBbloom--"),
            RedisType::Unknown("MBbloom--".into())
        );
    }

    #[test]
    fn badge_label_fits_and_keeps_short_names() {
        assert_eq!(RedisType::String.badge_label(), "str");
        assert_eq!(RedisType::Stream.badge_label(), "stream");
        assert_eq!(RedisType::Unknown("none".into()).badge_label(), "none");
        assert_eq!(RedisType::Unknown(String::new()).badge_label(), "?");
        let long = RedisType::Unknown("MBbloom--".into());
        assert_eq!(long.badge_label(), "MBblo…");
        assert_eq!(long.name(), "MBbloom--");
        assert!(long.badge_label().chars().count() <= TYPE_BADGE_WIDTH);
    }

    pub(super) fn keys(range: std::ops::Range<usize>) -> Vec<String> {
        range.map(|i| format!("k{i}")).collect()
    }
}
