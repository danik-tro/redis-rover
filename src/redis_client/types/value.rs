use super::{KeyValue, RedisType};

use std::collections::HashSet;

impl KeyValue {
    /// True for a STRING whose preview is shorter than the stored value.
    #[must_use]
    pub fn is_truncated(&self) -> bool {
        matches!(self, Self::String { value, total_len } if value.len() < *total_len)
    }

    /// Text to copy for the whole value (#27): a STRING as stored, anything
    /// else as pretty JSON. LIST / SET → array, HASH → object (scan order),
    /// ZSET → `[{"member", "score"}]` by rank. `None` for unsupported types.
    pub fn to_clipboard_text(&self) -> Option<String> {
        use serde_json::{Map, Value};

        let json = match self {
            Self::String { value, .. } => return Some(value.clone()),
            Self::Json(value) => value.clone(),
            Self::List(items) | Self::Set(items) => {
                Value::Array(items.iter().cloned().map(Value::String).collect())
            }
            Self::Hash(entries) => Value::Object(
                entries
                    .iter()
                    .map(|(field, value)| (field.clone(), Value::String(value.clone())))
                    .collect::<Map<_, _>>(),
            ),
            Self::Zset(entries) => Value::Array(
                entries
                    .iter()
                    .map(|(member, score)| {
                        serde_json::json!({ "member": member, "score": score_json(*score) })
                    })
                    .collect(),
            ),
            Self::Unknown => return None,
        };
        serde_json::to_string_pretty(&json).ok()
    }
}

/// One ZSET element as copied from inside the collection (#27):
/// `{"rank":3,"member":"bob","score":12.5}`. `rank` is 0-based, as `ZRANK`.
pub fn zset_entry_json(rank: usize, member: &str, score: f64) -> String {
    serde_json::json!({ "rank": rank, "member": member, "score": score_json(score) }).to_string()
}

/// A ZSET score as JSON. `inf` / `-inf` have no JSON number, so they stay text.
fn score_json(score: f64) -> serde_json::Value {
    serde_json::Number::from_f64(score)
        .map_or_else(|| serde_json::Value::String(score.to_string()), Into::into)
}

/// Default number of collection elements shown per value page.
pub const DEFAULT_VALUE_PAGE_SIZE: usize = 100;

/// Default `HSCAN` / `SSCAN ... COUNT` hint.
pub const DEFAULT_VALUE_SCAN_COUNT: usize = 100;

/// How much of a STRING value is fetched (`GETRANGE 0 n-1`).
pub const STRING_PREVIEW_BYTES: usize = 64 * 1024;

/// Progress of an `HSCAN` / `SSCAN` sweep over one collection.
///
/// Like the keyspace sweep, HSCAN/SSCAN can repeat elements and can't jump to an
/// offset, so elements are accumulated (deduplicated, in scan order) only as far
/// as the current page needs, and a page is a slice of that list.
#[derive(Debug, Clone, Default)]
pub struct ScanSweep {
    /// Cursor for the next scan step.
    pub cursor: u64,
    /// The sweep reached the end (the scan returned cursor 0).
    pub done: bool,
    /// SET members or HASH field names, in scan order, deduplicated.
    pub names: Vec<String>,
    /// HASH field values, parallel to `names`. Always empty for a SET.
    pub values: Vec<String>,
    seen: HashSet<String>,
}

impl ScanSweep {
    /// Merge one `SSCAN` step, skipping members already seen.
    pub fn merge_members(&mut self, cursor: u64, members: Vec<String>) {
        for member in members {
            if self.seen.insert(member.clone()) {
                self.names.push(member);
            }
        }
        self.advance(cursor);
    }

    /// Merge one `HSCAN` step, skipping fields already seen.
    pub fn merge_fields(&mut self, cursor: u64, fields: Vec<(String, String)>) {
        for (field, value) in fields {
            if self.seen.insert(field.clone()) {
                self.names.push(field);
                self.values.push(value);
            }
        }
        self.advance(cursor);
    }

    fn advance(&mut self, cursor: u64) {
        self.cursor = cursor;
        self.done = cursor == 0;
    }
}

/// Paging state for the selected key's value, shared between the UI and the
/// background fetcher (mirrors [`KeyspaceState`]).
///
/// LIST and ZSET are paged by index (`LRANGE` / `ZRANGE start stop`). HASH and
/// SET are paged over a [`ScanSweep`]. STRING has no pages.
#[derive(Debug, Clone)]
pub struct ValueState {
    pub key: Option<String>,
    pub r_type: RedisType,
    /// Current page, 0-based.
    pub page: usize,
    /// Elements per page.
    pub page_size: usize,
    /// `COUNT` hint for every `HSCAN` / `SSCAN` step.
    pub scan_count: usize,
    /// Collection length (`LLEN` / `ZCARD` / `HLEN` / `SCARD`) from the last fetch.
    pub total: Option<usize>,
    /// HASH / SET sweep progress.
    pub sweep: ScanSweep,
    /// Bumped on every key / page / rescan change, so the fetcher can drop
    /// results for a state the user already left.
    pub generation: u64,
}

impl Default for ValueState {
    fn default() -> Self {
        Self {
            key: None,
            r_type: RedisType::Unknown(String::new()),
            page: 0,
            page_size: DEFAULT_VALUE_PAGE_SIZE,
            scan_count: DEFAULT_VALUE_SCAN_COUNT,
            total: None,
            sweep: ScanSweep::default(),
            generation: 0,
        }
    }
}

impl ValueState {
    /// Point the state at `key`. Re-selecting the same key keeps the page and
    /// only restarts the sweep (so a write is picked up without jumping back to
    /// page 1); a different key starts from page 1.
    pub fn select(&mut self, key: &str, r_type: RedisType) {
        if self.key.as_deref() == Some(key) && self.r_type == r_type {
            self.rescan();
            return;
        }
        self.key = Some(key.to_owned());
        self.r_type = r_type;
        self.page = 0;
        self.total = None;
        self.rescan();
    }

    /// Forget the selected key (e.g. after a DB switch). Bumps `generation`
    /// so an in-flight fetch for the old key is dropped.
    pub fn clear(&mut self) {
        self.key = None;
        self.page = 0;
        self.total = None;
        self.rescan();
    }

    /// Restart the sweep from scratch, keeping the current page.
    pub fn rescan(&mut self) {
        self.sweep = ScanSweep::default();
        self.generation += 1;
    }

    /// HASH and SET page over a scan sweep; the others by index.
    pub fn is_swept(&self) -> bool {
        matches!(self.r_type, RedisType::Hash | RedisType::Set)
    }

    /// Index of the current page's first element.
    pub fn page_start(&self) -> usize {
        self.page * self.page_size
    }

    /// Range of `sweep.names` covered by the current page (clamped to what the
    /// sweep has found).
    pub fn page_range(&self) -> std::ops::Range<usize> {
        let found = self.sweep.names.len();
        let start = self.page_start().min(found);
        let end = (start + self.page_size).min(found);
        start..end
    }

    /// The sweep must scan further to fill the current page.
    pub fn needs_scan(&self) -> bool {
        self.is_swept()
            && !self.sweep.done
            && self.sweep.names.len() < (self.page + 1) * self.page_size
    }

    pub fn has_next_page(&self) -> bool {
        if self.is_swept() {
            self.sweep.names.len() > self.page_range().end || !self.sweep.done
        } else {
            self.total
                .is_some_and(|total| self.page_start() + self.page_size < total)
        }
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

    /// If the collection shrank below the current page, fall back to the last
    /// non-empty page. Swept types only clamp once the sweep has finished.
    pub fn clamp_page(&mut self) {
        if self.page_size == 0 {
            return;
        }
        let len = if self.is_swept() {
            if !self.sweep.done {
                return;
            }
            self.sweep.names.len()
        } else {
            let Some(total) = self.total else {
                return;
            };
            total
        };
        let last_page = len.saturating_sub(1) / self.page_size;
        self.page = self.page.min(last_page);
    }

    /// Copy page / total / sweep progress (but not key or generation) from a
    /// working copy the background fetcher advanced.
    pub fn apply(&mut self, other: ValueState) {
        self.page = other.page;
        self.total = other.total;
        self.sweep = other.sweep;
    }

    /// Paging summary for a page of `len` rows.
    pub fn page_info(&self, len: usize) -> ValuePageInfo {
        ValuePageInfo {
            start: self.page_start(),
            len,
            total: self.total,
            has_next: self.has_next_page(),
            has_prev: self.has_previous_page(),
        }
    }
}

/// Where the loaded page sits in the whole collection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ValuePageInfo {
    /// Index of the page's first element, 0-based.
    pub start: usize,
    /// Rows on this page.
    pub len: usize,
    /// Collection length, if known.
    pub total: Option<usize>,
    pub has_next: bool,
    pub has_prev: bool,
}

impl ValuePageInfo {
    /// More than one page exists, so row navigation turns pages at the edges.
    pub fn is_paged(&self) -> bool {
        self.has_next || self.has_prev
    }
}

/// A fetched value page plus its position, as handed to the UI.
#[derive(Debug, Clone)]
pub struct LoadedValue {
    pub value: KeyValue,
    pub page: ValuePageInfo,
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::redis_client::types::tests::keys;

    #[test]
    fn value_state_clear_forgets_key() {
        let mut state = value_state(RedisType::List, 10);
        let generation = state.generation;
        state.clear();
        assert_eq!(state.key, None);
        assert!(state.generation > generation);
    }

    fn value_state(r_type: RedisType, page_size: usize) -> ValueState {
        let mut state = ValueState {
            page_size,
            ..ValueState::default()
        };
        state.select("k", r_type);
        state
    }

    #[test]
    fn sweep_merge_dedups_members_and_fields() {
        let mut sweep = ScanSweep::default();
        sweep.merge_members(5, vec!["a".into(), "b".into()]);
        sweep.merge_members(0, vec!["b".into(), "c".into()]);
        assert_eq!(sweep.names, vec!["a", "b", "c"]);
        assert!(sweep.values.is_empty());
        assert!(sweep.done);

        let mut sweep = ScanSweep::default();
        sweep.merge_fields(3, vec![("f1".into(), "1".into())]);
        sweep.merge_fields(
            0,
            vec![("f1".into(), "x".into()), ("f2".into(), "2".into())],
        );
        assert_eq!(sweep.names, vec!["f1", "f2"]);
        assert_eq!(sweep.values, vec!["1", "2"]);
    }

    #[test]
    fn swept_value_pages_over_sweep() {
        let mut state = value_state(RedisType::Set, 10);
        assert!(state.needs_scan());
        state.sweep.merge_members(9, keys(0..10));
        assert!(!state.needs_scan());
        assert_eq!(state.page_range(), 0..10);
        // Unfinished sweep: there may be more.
        assert!(state.has_next_page());
        assert!(state.next_page());
        assert!(state.needs_scan());

        state.sweep.merge_members(0, keys(10..15));
        assert_eq!(state.page_range(), 10..15);
        assert!(!state.has_next_page());
        assert!(!state.next_page());
    }

    #[test]
    fn indexed_value_pages_by_total() {
        let mut state = value_state(RedisType::List, 10);
        assert!(!state.needs_scan());
        assert!(!state.has_next_page(), "unknown total means no next page");

        state.total = Some(25);
        assert!(state.next_page());
        assert!(state.next_page());
        assert_eq!(state.page_start(), 20);
        assert!(!state.next_page());

        state.total = Some(20);
        assert!(!state.has_next_page());
    }

    #[test]
    fn select_same_key_keeps_page_other_key_resets() {
        let mut state = value_state(RedisType::List, 10);
        state.total = Some(100);
        state.next_page();
        state.next_page();
        let generation = state.generation;

        state.select("k", RedisType::List);
        assert_eq!(state.page, 2);
        assert!(state.generation > generation);

        state.select("other", RedisType::List);
        assert_eq!(state.page, 0);
        assert_eq!(state.total, None);
        assert_eq!(state.key.as_deref(), Some("other"));
    }

    #[test]
    fn value_clamp_page_after_shrink() {
        let mut list = value_state(RedisType::List, 10);
        list.page = 5;
        list.total = Some(23);
        list.clamp_page();
        assert_eq!(list.page, 2);

        let mut set = value_state(RedisType::Hash, 10);
        set.page = 5;
        set.sweep.merge_fields(7, vec![("a".into(), "1".into())]);
        set.clamp_page();
        assert_eq!(set.page, 5, "unfinished sweep doesn't clamp");
        set.sweep.merge_fields(0, Vec::new());
        set.clamp_page();
        assert_eq!(set.page, 0);
    }

    #[test]
    fn clipboard_text_per_type() {
        let string = KeyValue::String {
            value: "{\"a\":1}".into(),
            total_len: 7,
        };
        assert_eq!(string.to_clipboard_text().unwrap(), "{\"a\":1}");

        let list = KeyValue::List(vec!["a".into(), "b".into()]);
        assert_eq!(list.to_clipboard_text().unwrap(), "[\n  \"a\",\n  \"b\"\n]");

        let hash = KeyValue::Hash(vec![("z".into(), "1".into()), ("a".into(), "2".into())]);
        assert_eq!(
            hash.to_clipboard_text().unwrap(),
            "{\n  \"z\": \"1\",\n  \"a\": \"2\"\n}",
            "fields keep scan order"
        );

        let zset = KeyValue::Zset(vec![("bob".into(), 12.5), ("eve".into(), f64::INFINITY)]);
        let parsed: serde_json::Value =
            serde_json::from_str(&zset.to_clipboard_text().unwrap()).unwrap();
        assert_eq!(
            parsed,
            serde_json::json!([
                { "member": "bob", "score": 12.5 },
                { "member": "eve", "score": "inf" }
            ])
        );

        assert!(KeyValue::Unknown.to_clipboard_text().is_none());
    }

    #[test]
    fn zset_entry_has_rank_member_score() {
        assert_eq!(
            zset_entry_json(3, "bob", 12.5),
            r#"{"rank":3,"member":"bob","score":12.5}"#
        );
    }

    #[test]
    fn string_truncation_flag() {
        let full = KeyValue::String {
            value: "abc".into(),
            total_len: 3,
        };
        let cut = KeyValue::String {
            value: "ab".into(),
            total_len: 3,
        };
        assert!(!full.is_truncated());
        assert!(cut.is_truncated());
    }
}
