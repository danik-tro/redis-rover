use super::KeySpace;

use crate::{
    redis_client::types::{
        ItemDelete, JsonPreview, JsonPreviewKind, KeyMeta, KeyValue, KeyspaceState, LoadedValue,
        Sort,
    },
    widgets::text_view::{TextCache, TextView},
};

/// Snapshot of the paging state shown in the Filters box.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageInfo {
    /// Current page, 0-based.
    pub page: usize,
    /// Position of the page's first key among the keys found so far, 0-based.
    pub start: usize,
    /// One past the page's last key.
    pub end: usize,
    /// Keys the SCAN sweep has found so far.
    pub known_total: usize,
    /// The sweep reached the end, so `known_total` is the full match count.
    pub complete: bool,
    pub page_size: usize,
    /// Raw cursor of the next SCAN step (debug detail).
    pub scan_cursor: usize,
    pub pattern: Option<String>,
    /// Active sort (`None`: SCAN order).
    pub sort: Option<Sort>,
    /// `(known, loaded)` keys with metadata while a type / TTL / size sort
    /// still lacks some; `None` otherwise.
    pub meta_progress: Option<(usize, usize)>,
    /// Picked types as `hash,zset` (empty: every type).
    pub types: String,
    /// Tree mode: keys loaded (the rows count groups and keys shown).
    pub tree_keys: Option<usize>,
}

impl From<&KeyspaceState> for PageInfo {
    fn from(state: &KeyspaceState) -> Self {
        let range = state.page_range();
        Self {
            page: state.page,
            start: range.start,
            end: range.end,
            known_total: state.scanned.len(),
            complete: state.sweep_done,
            page_size: state.page_size,
            scan_cursor: state.sweep_cursor,
            pattern: state.pattern.clone(),
            sort: state.sort(),
            meta_progress: state
                .meta_progress()
                .filter(|&(known, loaded)| known < loaded),
            types: state.type_filter().label(),
            tree_keys: state.is_tree().then_some(state.scanned.len()),
        }
    }
}

impl PageInfo {
    /// `size 41k/120k` while the active sort still lacks metadata.
    /// The filter row's right end: the sort's `size 41k/120k` while it
    /// lacks metadata, else `tree · 1.2k keys` in tree mode.
    pub(super) fn note(&self) -> Option<String> {
        self.sort_note().or_else(|| {
            self.tree_keys
                .map(|keys| format!("tree · {} keys", super::format::format_count(keys)))
        })
    }

    pub(super) fn sort_note(&self) -> Option<String> {
        let (known, loaded) = self.meta_progress?;
        let key = self.sort?.key.label();
        Some(format!(
            "{key} {}/{}",
            super::format::format_count(known),
            super::format::format_count(loaded)
        ))
    }

    /// `Page 3 · 21–30 of 57+` (`+` while the sweep is unfinished).
    pub(super) fn summary(&self) -> String {
        let page = self.page + 1;
        let more = if self.complete { "" } else { "+" };
        if self.start == self.end {
            return format!("Page {page} · No keys");
        }
        format!(
            "Page {page} · {}–{} of {}{more}",
            self.start + 1,
            self.end,
            self.known_total
        )
    }
}

pub(super) enum KeySpacePopupMode {
    FilterPattern,
}

pub(super) enum KeySpaceMode {
    Normal,
    Popup(KeySpacePopupMode),
}

/// Which pane keyboard navigation drives. `Keys` is the default left-hand key
/// list; `Value` is the detail-view table entered via `EnterValue` so `j`/`k`
/// scroll a collection's rows; `Text` scrolls a STRING / JSON value's lines.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Focus {
    Keys,
    Value,
    Text,
}

/// Result of a `j` / `k` move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollOutcome {
    /// The cursor moved (or stayed put) within what's loaded.
    Moved,
    /// `j` on the last row of a value page that has a next page.
    PastEnd,
    /// `k` on the first row of a value page that has a previous page.
    PastStart,
}

/// The concrete collection element the value-table cursor points at, resolved
/// from the loaded `selected_value` page so it matches what the user sees. A
/// LIST `index` is absolute (page start + row), as `LSET` expects.
#[derive(Clone)]
pub(super) enum SelectedElement {
    ListIndex { index: usize, value: String },
    SetMember(String),
    HashField { field: String, value: String },
    ZsetMember { member: String, score: f64 },
}

impl SelectedElement {
    /// One-line human label for the element, used in the delete-item confirm.
    pub(super) fn label(&self) -> String {
        match self {
            SelectedElement::ListIndex { index, value } => format!("[{index}] {value}"),
            SelectedElement::SetMember(member) | SelectedElement::ZsetMember { member, .. } => {
                member.clone()
            }
            SelectedElement::HashField { field, .. } => field.clone(),
        }
    }

    /// Translate the element into its deletion request.
    pub(super) fn into_delete(self) -> ItemDelete {
        match self {
            SelectedElement::ListIndex { index, .. } => ItemDelete::ListIndex(index),
            SelectedElement::SetMember(member) => ItemDelete::SetMember(member),
            SelectedElement::HashField { field, .. } => ItemDelete::HashField(field),
            SelectedElement::ZsetMember { member, .. } => ItemDelete::ZsetMember(member),
        }
    }
}

impl KeySpace {
    pub fn set_selected_value(&mut self, value: Option<LoadedValue>) {
        self.text_cache = value
            .as_ref()
            .and_then(|loaded| TextCache::build(&loaded.value));
        if self.focus == Focus::Text && self.text_cache.is_none() {
            self.focus = Focus::Keys;
        }
        self.selected_value = value;
        self.value_loading = false;
        // A reloaded page may be shorter; keep the cursor on a real row.
        if self.is_value_focused() {
            match self.value_row_count() {
                0 => self.exit_value_focus(),
                len => {
                    let row = self.value_table.selected().unwrap_or(0).min(len - 1);
                    self.value_table.select(Some(row));
                }
            }
        }
    }

    pub fn is_value_loading(&self) -> bool {
        self.value_loading
    }

    pub fn clear_value_loading(&mut self) {
        self.value_loading = false;
    }

    /// Mark that another page of the current value is being fetched.
    pub fn set_value_loading(&mut self) {
        self.value_loading = true;
    }

    pub fn clear_selected_value(&mut self) {
        self.selected_value = None;
        self.text_cache = None;
        self.text_view = TextView {
            wrap: self.text_view.wrap,
            ..TextView::default()
        };
        self.value_loading = false;
        // The detail-view selection belongs to the previously selected key;
        // reset it (and drop value focus) so the new value starts fresh.
        self.value_table.select(None);
        self.focus = Focus::Keys;
    }

    pub fn refresh(&mut self) {
        self.table.select(None);
    }

    pub fn set_page_info(&mut self, page: PageInfo) {
        self.page = page;
    }

    pub fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
    }

    /// Show `keys` as the flat list.
    pub fn set_keys(&mut self, keys: Vec<KeyMeta>) {
        self.tree_rows = None;
        _ = std::mem::replace(&mut self.keys, keys);
        self.loaded = true;
    }

    /// Number of rows the loaded value renders as a table. Scalar/absent values
    /// have no navigable rows.
    pub(super) fn value_row_count(&self) -> usize {
        match self.selected_value.as_ref().map(|loaded| &loaded.value) {
            Some(KeyValue::List(items) | KeyValue::Set(items)) => items.len(),
            Some(KeyValue::Hash(entries)) => entries.len(),
            Some(KeyValue::Zset(entries)) => entries.len(),
            Some(KeyValue::JsonPreview(JsonPreview {
                kind: JsonPreviewKind::Object { fields, .. },
                ..
            })) => fields.len(),
            Some(
                KeyValue::String { .. }
                | KeyValue::Json(_)
                | KeyValue::JsonPreview(_)
                | KeyValue::Unknown,
            )
            | None => 0,
        }
    }

    /// True when the loaded value is a collection that can be navigated row by
    /// row (the precondition for [`Self::enter_value`]).
    pub(super) fn value_is_navigable(&self) -> bool {
        self.value_row_count() > 0
    }

    /// The currently selected row within the value table, if value-focused.
    /// Used to remember the cursor position across an add-item refresh.
    pub fn value_selected_row(&self) -> Option<usize> {
        if self.is_value_focused() {
            self.value_table.selected()
        } else {
            None
        }
    }

    /// Resolve the value-table cursor to the concrete element it points at,
    /// reading the exact page the user is looking at. `None` unless
    /// value-focused with a selected navigable row.
    pub(super) fn selected_element(&self) -> Option<SelectedElement> {
        if !self.is_value_focused() {
            return None;
        }
        let row = self.value_table.selected()?;
        let loaded = self.selected_value.as_ref()?;
        match &loaded.value {
            KeyValue::List(items) => items.get(row).map(|v| SelectedElement::ListIndex {
                index: loaded.page.start + row,
                value: v.clone(),
            }),
            KeyValue::Set(members) => members
                .get(row)
                .map(|m| SelectedElement::SetMember(m.clone())),
            KeyValue::Hash(entries) => entries.get(row).map(|(f, v)| SelectedElement::HashField {
                field: f.clone(),
                value: v.clone(),
            }),
            KeyValue::Zset(items) => items.get(row).map(|(m, s)| SelectedElement::ZsetMember {
                member: m.clone(),
                score: *s,
            }),
            KeyValue::String { .. }
            | KeyValue::Json(_)
            | KeyValue::JsonPreview(_)
            | KeyValue::Unknown => None,
        }
    }

    /// The field the cursor is on in a JSON object preview, if value-focused.
    pub fn selected_json_field(&self) -> Option<String> {
        if !self.is_value_focused() {
            return None;
        }
        let row = self.value_table.selected()?;
        match &self.selected_value.as_ref()?.value {
            KeyValue::JsonPreview(JsonPreview {
                kind: JsonPreviewKind::Object { fields, .. },
                ..
            }) => fields.get(row).map(|field| field.name.clone()),
            _ => None,
        }
    }

    /// The JSON field path the loaded value belongs to (shown in its footer).
    pub fn set_json_path(&mut self, path: Vec<String>) {
        self.json_path = path;
    }

    /// Put focus in the value after a JSON field opened or closed: on its
    /// text, or on row `row` of its field table.
    pub fn focus_value_at(&mut self, row: usize) {
        if self.text_cache.is_some() {
            self.focus = Focus::Text;
        } else {
            self.restore_value_focus(row);
        }
    }

    /// Re-enter value focus at `row` (clamped to the reloaded value's row count)
    /// after a refresh, so adding an item keeps the user inside the collection
    /// instead of bouncing back to the key list. No-op if the value is no longer
    /// navigable.
    pub fn restore_value_focus(&mut self, row: usize) {
        let len = self.value_row_count();
        if len == 0 {
            return;
        }
        self.focus = Focus::Value;
        self.value_table.select(Some(row.min(len - 1)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redis_client::types::{KeyValue, LoadedValue, ValuePageInfo};

    use crate::widgets::keyspace::tests::{focused, list_page};

    #[test]
    fn list_element_index_is_absolute() {
        let mut keyspace = focused(list_page(3, 200, true, true));
        keyspace.scroll_next();
        match keyspace.selected_element() {
            Some(SelectedElement::ListIndex { index, value }) => {
                assert_eq!(index, 201);
                assert_eq!(value, "v201");
            }
            _ => panic!("expected a list element"),
        }
    }

    #[test]
    fn hash_element_follows_page_order() {
        let mut keyspace = focused(LoadedValue {
            value: KeyValue::Hash(vec![("b".into(), "2".into()), ("a".into(), "1".into())]),
            page: ValuePageInfo::default(),
        });
        keyspace.scroll_next();
        match keyspace.selected_element() {
            Some(SelectedElement::HashField { field, .. }) => assert_eq!(field, "a"),
            _ => panic!("expected a hash field"),
        }
    }

    #[test]
    fn shorter_reload_clamps_value_cursor() {
        let mut keyspace = focused(list_page(5, 0, false, false));
        keyspace.scroll_previous();
        keyspace.set_selected_value(Some(list_page(2, 0, false, false)));
        assert_eq!(keyspace.value_table.selected(), Some(1));
    }

    #[test]
    fn page_summary() {
        let mut page = PageInfo {
            page: 2,
            start: 20,
            end: 30,
            known_total: 57,
            complete: false,
            page_size: 10,
            ..PageInfo::default()
        };
        assert_eq!(page.summary(), "Page 3 · 21–30 of 57+");

        page.complete = true;
        assert_eq!(page.summary(), "Page 3 · 21–30 of 57");

        let empty = PageInfo {
            complete: true,
            ..PageInfo::default()
        };
        assert_eq!(empty.summary(), "Page 1 · No keys");
    }
}
