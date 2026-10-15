use std::collections::HashMap;
use std::{borrow::Cow, collections::HashSet};

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
}

impl From<HashMap<&str, &str>> for RedisInfo {
    fn from(map: HashMap<&str, &str>) -> Self {
        Self {
            common: CommonInfo::from(map),
        }
    }
}

impl RedisInfo {
    pub fn redis_version(&self) -> String {
        format!("Redis version: {}", self.common.version)
    }

    pub fn os(&self) -> String {
        format!("OS: {}", self.common.os)
    }

    pub fn memory(&self) -> String {
        format!("RAM: {}/{}", self.common.memory, self.common.total_memory)
    }

    pub fn clients(&self) -> String {
        format!(
            "Connected clients: {}/{}",
            self.common.clients, self.common.max_clients
        )
    }
}

#[derive(Debug, Clone)]
pub enum KeyValue {
    String(String),
    List(Vec<String>),
    Set(HashSet<String>),
    Hash(HashMap<String, String>),
    Zset(Vec<(String, f64)>), // Tuple of value and score
    #[allow(dead_code)]
    Json(serde_json::Value),
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

#[derive(Debug, Clone, Copy)]
pub enum RedisType {
    Set,
    List,
    Hash,
    Zset,
    Json,
    String,
    Unknown,
}

impl RedisType {
    fn from_str(value: &str) -> Self {
        match value {
            "string" => Self::String,
            "hash" => Self::Hash,
            "set" => Self::Set,
            "zset" => Self::Zset,
            "json" => Self::Json,
            "list" => Self::List,
            _ => Self::Unknown,
        }
    }
}

impl From<RedisType> for Text<'_> {
    /// Fixed-width, lowercase type label in the type's palette color.
    fn from(value: RedisType) -> Self {
        let palette = &config::get().keyspace;
        let (label, color) = match value {
            RedisType::String => ("str", palette.string),
            RedisType::Json => ("json", palette.json),
            RedisType::List => ("list", palette.list),
            RedisType::Set => ("set", palette.set),
            RedisType::Zset => ("zset", palette.zset),
            RedisType::Hash => ("hash", palette.hash),
            RedisType::Unknown => ("?", palette.unknown),
        };
        Span::raw(label).fg(color).bold().into()
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

/// Default number of keys shown per page.
pub const DEFAULT_PAGE_SIZE: usize = 10;
/// Default `SCAN ... COUNT` hint. Larger than the page size so a selective
/// `MATCH` pattern on a big keyspace needs fewer round trips per page.
pub const DEFAULT_SCAN_COUNT: usize = 100;

/// Keyspace browsing state: pattern plus client-side paging over a single
/// `SCAN` sweep.
///
/// Paging is done client-side rather than with a stack of raw SCAN cursors:
/// cursors aren't stable across writes (rehashing can move keys between
/// buckets), so walking back through remembered cursors could skip or repeat
/// keys, and a single SCAN step may return zero or duplicate keys. Instead the
/// sweep accumulates the (deduplicated) key names it has seen so far, and a page
/// is just a `page_size` slice of that list — back-navigation is exact and every
/// page but the last is full. The sweep only advances as far as the current page
/// needs, so memory is bounded by how far the user has paged.
#[derive(Debug, Clone)]
pub struct KeyspaceState {
    pub pattern: Option<String>,
    /// Keys shown per page.
    pub page_size: usize,
    /// `COUNT` hint sent with every `SCAN` step.
    pub scan_count: usize,
    /// Current page, 0-based.
    pub page: usize,
    /// Key names discovered so far, in SCAN order, deduplicated.
    pub scanned: Vec<String>,
    seen: HashSet<String>,
    /// Cursor for the next `SCAN` step of the sweep.
    pub sweep_cursor: usize,
    /// The sweep reached the end (SCAN returned cursor 0).
    pub sweep_done: bool,
    /// Bumped on every page / pattern / rescan change, so the background
    /// fetcher can drop results computed for a state the user already left.
    pub generation: u64,
}

impl KeyspaceState {
    /// Range of `scanned` covered by the current page (clamped to what's known).
    pub fn page_range(&self) -> std::ops::Range<usize> {
        let start = (self.page * self.page_size).min(self.scanned.len());
        let end = (start + self.page_size).min(self.scanned.len());
        start..end
    }

    /// Number of key names the sweep must have found to fill the current page.
    pub fn keys_needed(&self) -> usize {
        (self.page + 1) * self.page_size
    }

    /// The sweep still needs to `SCAN` further to fill the current page.
    pub fn needs_scan(&self) -> bool {
        !self.sweep_done && self.scanned.len() < self.keys_needed()
    }

    pub fn has_next_page(&self) -> bool {
        self.scanned.len() > self.page_range().end || !self.sweep_done
    }

    pub fn has_previous_page(&self) -> bool {
        self.page > 0
    }

    /// Advance one page. Returns `false` (and changes nothing) on the last page.
    pub fn next_page(&mut self) -> bool {
        if !self.has_next_page() {
            return false;
        }
        self.page += 1;
        self.generation += 1;
        true
    }

    /// Go back one page. Returns `false` (and changes nothing) on the first page.
    pub fn previous_page(&mut self) -> bool {
        if !self.has_previous_page() {
            return false;
        }
        self.page -= 1;
        self.generation += 1;
        true
    }

    /// Restart the sweep from scratch, keeping the current page, so writes
    /// (new / deleted keys) are picked up without jumping back to page 1.
    pub fn rescan(&mut self) {
        self.scanned.clear();
        self.seen.clear();
        self.sweep_cursor = 0;
        self.sweep_done = false;
        self.generation += 1;
    }

    pub fn set_pattern(&mut self, pattern: Option<String>) {
        self.pattern = pattern;
        self.page = 0;
        self.rescan();
    }

    pub fn delete_pattern(&mut self) {
        self.set_pattern(None);
    }

    /// Merge one `SCAN` step into the sweep, skipping keys already seen (SCAN
    /// may return a key more than once). A returned cursor of 0 ends the sweep.
    pub fn merge_batch(&mut self, cursor: usize, keys: Vec<String>) {
        for key in keys {
            if self.seen.insert(key.clone()) {
                self.scanned.push(key);
            }
        }
        self.sweep_cursor = cursor;
        self.sweep_done = cursor == 0;
    }

    /// If the finished sweep doesn't reach the current page (keys were deleted
    /// since the user paged there), fall back to the last non-empty page.
    pub fn clamp_page(&mut self) {
        if !self.sweep_done || self.page_size == 0 {
            return;
        }
        let last_page = self.scanned.len().saturating_sub(1) / self.page_size;
        self.page = self.page.min(last_page);
    }

    /// Copy the sweep progress (and the possibly clamped page, but not the
    /// pattern / generation) from a working copy the background fetcher advanced.
    pub fn apply_sweep(&mut self, other: KeyspaceState) {
        self.page = other.page;
        self.scanned = other.scanned;
        self.seen = other.seen;
        self.sweep_cursor = other.sweep_cursor;
        self.sweep_done = other.sweep_done;
    }
}

impl Default for KeyspaceState {
    fn default() -> Self {
        Self {
            pattern: None,
            page_size: DEFAULT_PAGE_SIZE,
            scan_count: DEFAULT_SCAN_COUNT,
            page: 0,
            scanned: Vec::new(),
            seen: HashSet::new(),
            sweep_cursor: 0,
            sweep_done: false,
            generation: 0,
        }
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

    fn keys(range: std::ops::Range<usize>) -> Vec<String> {
        range.map(|i| format!("k{i}")).collect()
    }

    fn state_with(page_size: usize) -> KeyspaceState {
        KeyspaceState {
            page_size,
            ..KeyspaceState::default()
        }
    }

    #[test]
    fn page_range_slices_scanned_keys() {
        let mut state = state_with(10);
        state.merge_batch(0, keys(0..25));

        assert_eq!(state.page_range(), 0..10);
        assert!(state.next_page());
        assert_eq!(state.page_range(), 10..20);
        assert!(state.next_page());
        assert_eq!(state.page_range(), 20..25);
        assert!(!state.has_next_page());
        assert!(!state.next_page());
        assert_eq!(state.page, 2);
    }

    #[test]
    fn previous_page_stops_at_first() {
        let mut state = state_with(10);
        assert!(!state.previous_page());
        state.merge_batch(5, keys(0..10));
        assert!(state.next_page());
        assert!(state.previous_page());
        assert_eq!(state.page, 0);
    }

    #[test]
    fn unfinished_sweep_has_next_page() {
        let mut state = state_with(10);
        state.merge_batch(42, keys(0..10));
        assert!(!state.needs_scan());
        assert!(state.has_next_page());
        assert!(state.next_page());
        assert!(state.needs_scan());
        assert_eq!(state.keys_needed(), 20);
    }

    #[test]
    fn merge_batch_dedups_and_tracks_cursor() {
        let mut state = state_with(10);
        state.merge_batch(7, vec!["a".into(), "b".into()]);
        state.merge_batch(0, vec!["b".into(), "c".into(), "a".into()]);

        assert_eq!(state.scanned, vec!["a", "b", "c"]);
        assert!(state.sweep_done);
        assert_eq!(state.sweep_cursor, 0);
    }

    #[test]
    fn rescan_keeps_page_and_set_pattern_resets_it() {
        let mut state = state_with(10);
        state.merge_batch(0, keys(0..30));
        state.next_page();
        let generation = state.generation;

        state.rescan();
        assert_eq!(state.page, 1);
        assert!(state.scanned.is_empty());
        assert!(!state.sweep_done);
        assert!(state.generation > generation);

        state.set_pattern(Some("k1*".into()));
        assert_eq!(state.page, 0);
        assert_eq!(state.pattern.as_deref(), Some("k1*"));
    }

    #[test]
    fn clamp_page_falls_back_after_deletes() {
        let mut state = state_with(10);
        state.page = 3;
        state.merge_batch(0, keys(0..15));
        state.clamp_page();
        assert_eq!(state.page, 1);

        let mut empty = state_with(10);
        empty.page = 2;
        empty.merge_batch(0, Vec::new());
        empty.clamp_page();
        assert_eq!(empty.page, 0);
    }

    #[test]
    fn clamp_page_ignores_unfinished_sweep() {
        let mut state = state_with(10);
        state.page = 3;
        state.merge_batch(9, keys(0..15));
        state.clamp_page();
        assert_eq!(state.page, 3);
    }
}
