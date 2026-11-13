mod format;
mod forms;
mod input;
mod overlay;
mod render;
mod state;
mod type_menu;

pub use format::format_bytes;
pub(crate) use format::truncate_to_width;
use format::{format_bytes_u128, format_ttl, format_ttl_detail, value_footer};
pub(crate) use forms::{build_input_text_area, input_text};
pub use input::OverlayRequest;
pub use render::KeySpaceWidget;
pub use state::{PageInfo, ScrollOutcome};
pub use type_menu::{TypeMenu, TypeMenuOutcome};

use forms::{
    add_item_pair_titles, ActionOverlay, ActiveField, AddItemDraft, EditItemDraft, FilterPreview,
    NewKeyDraft, NewKeyStep, NewKind, OverlayKind,
};
use state::{Focus, KeySpaceMode, KeySpacePopupMode, SelectedElement};

use crossterm::event::KeyEvent;
use ratatui::widgets::TableState;
use ratatui_textarea::TextArea;

use crate::{
    redis_client::types::{
        zset_entry_json, KeyMeta, KeyValue, KeyspaceState, LoadedValue, RedisType, SortKey,
        TypeFilter,
    },
    widgets::text_view::{TextCache, TextView},
};

// The bools are independent view flags, not states of one machine.
#[allow(clippy::struct_excessive_bools)]
pub struct KeySpace {
    table: TableState,
    value_table: TableState,
    keys: Vec<KeyMeta>,
    /// False until the first SCAN result lands, so an empty table can show
    /// "Loading…" rather than "No keys".
    loaded: bool,
    /// A keyspace fetch is in flight.
    loading: bool,
    page: PageInfo,
    mode: KeySpaceMode,
    text_area: Option<TextArea<'static>>,
    /// Live match count for the open filter popup.
    filter_preview: Option<FilterPreview>,
    selected_value: Option<LoadedValue>,
    /// Display lines of the selected value when it's a STRING / JSON.
    text_cache: Option<TextCache>,
    /// Scroll state of that text.
    text_view: TextView,
    /// A new page of the current value is being fetched; the old page stays on
    /// screen meanwhile.
    value_loading: bool,
    focus: Focus,
    action_overlay: Option<ActionOverlay>,
    new_key: Option<NewKeyDraft>,
    add_item: Option<AddItemDraft>,
    edit_item: Option<EditItemDraft>,
    /// Connected server runs in cluster mode: SCAN only sees this node, so a
    /// warning banner is shown above the table.
    pub cluster_mode: bool,
    /// Key rows that fit in the table on the last render (0 before the first
    /// render). The app sizes keyspace pages to it.
    visible_rows: usize,
    /// JSON field opened from a preview that the loaded value shows.
    json_path: Vec<String>,
    /// The `o` sort menu is open.
    sort_menu: bool,
    /// The `Shift-t` type checklist, while open.
    type_menu: Option<TypeMenu>,
}

/// The sort menu's letters (`None` = SCAN order), in the order shown.
pub const SORT_MENU: [(char, Option<SortKey>, &str); 5] = [
    ('x', None, "SCAN order"),
    ('k', Some(SortKey::Name), "Key"),
    ('t', Some(SortKey::Type), "Type"),
    ('l', Some(SortKey::Ttl), "TTL"),
    ('s', Some(SortKey::Size), "Size"),
];

/// Banner shown above the key table in cluster mode.
const CLUSTER_BANNER: &str = "⚠ cluster: SCAN sees only this node";

impl KeySpace {
    #[must_use]
    pub fn new(keys: Vec<KeyMeta>) -> Self {
        Self {
            keys,
            loaded: false,
            loading: false,
            page: PageInfo::from(&KeyspaceState::default()),
            table: TableState::default(),
            value_table: TableState::default(),
            mode: KeySpaceMode::Normal,
            text_area: None,
            filter_preview: None,
            selected_value: None,
            text_cache: None,
            text_view: TextView::default(),
            value_loading: false,
            focus: Focus::Keys,
            action_overlay: None,
            new_key: None,
            add_item: None,
            edit_item: None,
            cluster_mode: false,
            visible_rows: 0,
            json_path: Vec::new(),
            sort_menu: false,
            type_menu: None,
        }
    }

    /// Size (`MEMORY USAGE`) of the selected key, if any.
    pub fn selected_size(&self) -> Option<u128> {
        let meta = self.keys.get(self.table.selected()?)?;
        Some(meta.size)
    }

    /// Text to copy for the selected collection element (#27), when focus is
    /// inside a collection: the LIST item / SET member, the HASH field's
    /// value, or a ZSET `{"rank","member","score"}` JSON object.
    pub fn selected_element_text(&self) -> Option<String> {
        let row = self.value_table.selected()?;
        let start = self.selected_value.as_ref()?.page.start;
        Some(match self.selected_element()? {
            SelectedElement::ListIndex { value, .. } | SelectedElement::HashField { value, .. } => {
                value
            }
            SelectedElement::SetMember(member) => member,
            SelectedElement::ZsetMember { member, score } => {
                zset_entry_json(start + row, &member, score)
            }
        })
    }

    /// Text to copy for the selected key's loaded value, if it holds the whole
    /// value: a STRING that isn't truncated, a JSON document, or a collection
    /// that fits on one page. `None` means the value must be fetched whole.
    pub fn loaded_value_text(&self) -> Option<String> {
        let loaded = self.selected_value.as_ref()?;
        let whole = match &loaded.value {
            KeyValue::String { .. } => !loaded.value.is_truncated(),
            KeyValue::Json(_) => true,
            KeyValue::List(_) | KeyValue::Set(_) | KeyValue::Hash(_) | KeyValue::Zset(_) => {
                !loaded.page.is_paged()
            }
            KeyValue::JsonPreview(_) | KeyValue::Unknown => false,
        };
        if !whole {
            return None;
        }
        loaded.value.to_clipboard_text()
    }

    /// Ask before copying `key`, whose value is `size` bytes.
    pub fn open_yank_confirm(&mut self, key: String, r_type: RedisType, size: u128) {
        self.action_overlay = Some(ActionOverlay {
            kind: OverlayKind::ConfirmYank { key, r_type, size },
            text_area: None,
        });
    }

    pub fn open_sort_menu(&mut self) {
        self.sort_menu = true;
    }

    pub fn close_sort_menu(&mut self) {
        self.sort_menu = false;
    }

    pub fn is_sort_menu_open(&self) -> bool {
        self.sort_menu
    }

    /// Open the type checklist, ticked as `filter`, offering the types on
    /// the page next to the standard ones.
    pub fn open_type_menu(&mut self, filter: &TypeFilter) {
        let seen = self.keys.iter().map(|meta| meta.r_type.name());
        self.type_menu = Some(TypeMenu::new(filter, seen));
    }

    pub fn close_type_menu(&mut self) {
        self.type_menu = None;
    }

    pub fn is_type_menu_open(&self) -> bool {
        self.type_menu.is_some()
    }

    /// A key in the open type checklist.
    pub fn handle_type_menu_key(&mut self, key: KeyEvent) -> TypeMenuOutcome {
        self.type_menu
            .as_mut()
            .map_or(TypeMenuOutcome::Open, |menu| menu.handle_key(key))
    }

    /// Key rows that fit in the table, as measured by the last render.
    pub fn visible_rows(&self) -> usize {
        self.visible_rows
    }

    /// Row of the selected key within the current page.
    pub fn selected_row(&self) -> Option<usize> {
        self.table.selected()
    }

    pub fn selected_key(&self) -> Option<(String, RedisType)> {
        let idx = self.table.selected()?;
        let meta = self.keys.get(idx)?;
        Some((meta.key.clone(), meta.r_type.clone()))
    }

    /// The table lists exactly `names`, in this order.
    pub fn shows_keys(&self, names: &[String]) -> bool {
        self.keys.len() == names.len()
            && self
                .keys
                .iter()
                .zip(names)
                .all(|(row, name)| row.key == *name)
    }

    /// Re-select the row whose key matches `name`, if it's still present in the
    /// current page. Used to keep the cursor on a key across a refresh after a
    /// non-destructive edit. Returns `true` if the key was found and selected.
    pub fn select_key_by_name(&mut self, name: &str) -> bool {
        if let Some(idx) = self.keys.iter().position(|meta| meta.key == name) {
            self.table.select(Some(idx));
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::KeySpace;
    use crate::redis_client::types::{KeyMeta, KeyValue, LoadedValue, RedisType, ValuePageInfo};
    use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};

    pub(super) fn render_text(keyspace: &mut KeySpace) -> String {
        let area = Rect::new(0, 0, 120, 20);
        let mut buf = Buffer::empty(area);
        StatefulWidget::render(super::KeySpaceWidget, area, &mut buf, keyspace);
        buf.content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn cluster_banner_only_in_cluster_mode() {
        let mut keyspace = KeySpace::new(Vec::new());
        assert!(!render_text(&mut keyspace).contains("cluster: SCAN"));
        keyspace.cluster_mode = true;
        assert!(render_text(&mut keyspace).contains(super::CLUSTER_BANNER));
    }

    pub(super) fn list_page(
        rows: usize,
        start: usize,
        has_prev: bool,
        has_next: bool,
    ) -> LoadedValue {
        LoadedValue {
            value: KeyValue::List((0..rows).map(|i| format!("v{}", start + i)).collect()),
            page: ValuePageInfo {
                start,
                len: rows,
                total: Some(1000),
                has_next,
                has_prev,
            },
        }
    }

    pub(super) fn focused(value: LoadedValue) -> KeySpace {
        let mut keyspace = KeySpace::new(Vec::new());
        keyspace.set_selected_value(Some(value));
        keyspace.enter_value();
        keyspace
    }

    pub(super) fn string_value(text: &str) -> LoadedValue {
        LoadedValue {
            value: KeyValue::String {
                value: text.into(),
                total_len: text.len(),
            },
            page: ValuePageInfo::default(),
        }
    }

    /// A keyspace with one selected key whose loaded value is `value`.
    pub(super) fn with_value(r_type: RedisType, value: LoadedValue) -> KeySpace {
        let mut keyspace = KeySpace::new(Vec::new());
        keyspace.set_keys(vec![KeyMeta {
            key: "k".into(),
            r_type,
            size: 0,
            ttl: -1,
            value: KeyValue::Unknown,
        }]);
        keyspace.table.select(Some(0));
        keyspace.set_selected_value(Some(value));
        keyspace
    }

    #[test]
    fn yank_text_for_elements() {
        let zset = LoadedValue {
            value: KeyValue::Zset(vec![("a".into(), 1.0), ("bob".into(), 12.5)]),
            page: ValuePageInfo {
                start: 100,
                len: 2,
                total: Some(300),
                has_next: true,
                has_prev: true,
            },
        };
        let mut keyspace = focused(zset);
        keyspace.value_table.select(Some(1));
        assert_eq!(
            keyspace.selected_element_text().as_deref(),
            Some(r#"{"rank":101,"member":"bob","score":12.5}"#)
        );
        assert_eq!(keyspace.loaded_value_text(), None, "paged: fetch it whole");

        let hash = LoadedValue {
            value: KeyValue::Hash(vec![("name".into(), "Ann".into())]),
            page: ValuePageInfo::default(),
        };
        let keyspace = focused(hash);
        assert_eq!(keyspace.selected_element_text().as_deref(), Some("Ann"));
        assert_eq!(
            keyspace.loaded_value_text().as_deref(),
            Some("{\n  \"name\": \"Ann\"\n}")
        );

        let keyspace = focused(list_page(3, 0, false, false));
        assert_eq!(keyspace.selected_element_text().as_deref(), Some("v0"));
    }

    #[test]
    fn truncated_string_is_not_yanked_from_preview() {
        let loaded = LoadedValue {
            value: KeyValue::String {
                value: "abc".into(),
                total_len: 10,
            },
            page: ValuePageInfo::default(),
        };
        let keyspace = with_value(RedisType::String, loaded);
        assert_eq!(keyspace.loaded_value_text(), None);
        let keyspace = with_value(RedisType::String, string_value("abc"));
        assert_eq!(keyspace.loaded_value_text().as_deref(), Some("abc"));
    }
}
