use byte_unit::{Byte, UnitType};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Layout, Rect},
    style::Stylize,
    text::Line,
    widgets::{
        Block, BorderType, Borders, Cell, Clear, HighlightSpacing, Paragraph, Row, StatefulWidget,
        Table, TableState, Widget, Wrap,
    },
};
use tui_textarea::{CursorMove, TextArea};

use crate::{
    config,
    redis_client::types::{
        ItemDelete, ItemEdit, KeyItem, KeyMeta, KeyValue, NewKeySpec, RedisType,
    },
};

enum KeySpacePopupMode {
    FilterPattern,
}

enum KeySpaceMode {
    Normal,
    Popup(KeySpacePopupMode),
}

/// Which pane keyboard navigation drives. `Keys` is the default left-hand key
/// list; `Value` is the detail-view table entered via `EnterValue` so `j`/`k`
/// scroll a collection's rows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Keys,
    Value,
}

/// The kind of modal action prompt currently overlaid on the keyspace.
enum OverlayKind {
    /// Confirm deletion of the selected key.
    ConfirmDelete,
    /// Confirm deletion of a single collection element (label + resolved target).
    ConfirmDeleteItem {
        label: String,
        target: SelectedElement,
    },
    /// Edit the selected STRING value (text input).
    EditValue,
    /// Enter a key-level TTL in seconds (text input).
    SetTtl,
    /// A read-only notice (e.g. editing a non-STRING type is unsupported).
    Notice(String),
}

/// A transient modal prompt rendered on top of the keyspace. Reuses
/// `tui-textarea` for the kinds that take input ([`OverlayKind::EditValue`],
/// [`OverlayKind::SetTtl`]).
struct ActionOverlay {
    kind: OverlayKind,
    text_area: Option<TextArea<'static>>,
}

/// The set of key types the add-key wizard can create. JSON is intentionally
/// excluded (the wizard cannot construct `RedisJSON` documents this way).
#[derive(Clone, Copy, PartialEq, Eq)]
enum NewKind {
    String,
    List,
    Set,
    Hash,
    Zset,
}

impl NewKind {
    /// The kinds in selector order, used both for the radio list and to map the
    /// selection cursor back to a kind.
    const ALL: [NewKind; 5] = [
        NewKind::String,
        NewKind::List,
        NewKind::Set,
        NewKind::Hash,
        NewKind::Zset,
    ];

    fn label(self) -> &'static str {
        match self {
            NewKind::String => "STRING",
            NewKind::List => "LIST",
            NewKind::Set => "SET",
            NewKind::Hash => "HASH",
            NewKind::Zset => "ZSET",
        }
    }

    /// Whether this kind's element step needs two inputs (HASH field+value,
    /// ZSET member+score) rather than one.
    fn is_pair(self) -> bool {
        matches!(self, NewKind::Hash | NewKind::Zset)
    }

    /// Titles for the two paired inputs (`A`, `B`). Only meaningful for the pair
    /// kinds; other kinds return their single-input title in `A`.
    fn pair_titles(self) -> (&'static str, &'static str) {
        match self {
            NewKind::Hash => ("Field", "Value"),
            NewKind::Zset => ("Member", "Score"),
            NewKind::String => ("Value", ""),
            NewKind::List | NewKind::Set => ("Item", ""),
        }
    }
}

/// Ordered steps of the add-key wizard.
enum NewKeyStep {
    /// Choose the type (radio list).
    Type,
    /// Enter the key name (single input).
    Name,
    /// Enter an element: STRING value / LIST|SET item / HASH field+value /
    /// ZSET member+score. Repeatable for collections.
    Element,
    /// A blocking validation message (e.g. bad ZSET score). `Esc`/`Enter` returns
    /// to the element step.
    Notice(String),
}

/// Which of the two element inputs has focus (HASH/ZSET only).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ActiveField {
    A,
    B,
}

/// Draft state accumulated by the add-key wizard across its steps.
struct NewKeyDraft {
    step: NewKeyStep,
    kind: NewKind,
    kind_cursor: usize,
    name: String,
    string_value: Option<String>,
    list_items: Vec<String>,
    hash_pairs: Vec<(String, String)>,
    zset_pairs: Vec<(String, f64)>,
    input_a: TextArea<'static>,
    input_b: Option<TextArea<'static>>,
    active: ActiveField,
    /// Set when the user requests finalization; polled by `App`.
    finalize: bool,
}

impl NewKeyDraft {
    fn new() -> Self {
        Self {
            step: NewKeyStep::Type,
            kind: NewKind::String,
            kind_cursor: 0,
            name: String::new(),
            string_value: None,
            list_items: Vec::new(),
            hash_pairs: Vec::new(),
            zset_pairs: Vec::new(),
            input_a: TextArea::default(),
            input_b: None,
            active: ActiveField::A,
            finalize: false,
        }
    }

    /// Number of elements accumulated so far for the current kind.
    fn item_count(&self) -> usize {
        match self.kind {
            NewKind::String => usize::from(self.string_value.is_some()),
            NewKind::List | NewKind::Set => self.list_items.len(),
            NewKind::Hash => self.hash_pairs.len(),
            NewKind::Zset => self.zset_pairs.len(),
        }
    }

    /// Build the input widget(s) for the element step of the active kind.
    fn enter_element_step(&mut self) {
        self.step = NewKeyStep::Element;
        self.active = ActiveField::A;
        match self.kind {
            NewKind::String => {
                self.input_a = build_input_text_area("Value", "Enter value", None);
                self.input_b = None;
            }
            NewKind::List | NewKind::Set => {
                self.input_a = build_input_text_area("Item", "Enter item", None);
                self.input_b = None;
            }
            NewKind::Hash => {
                self.input_a = build_input_text_area("Field", "Enter field", None);
                self.input_b = Some(build_input_text_area("Value", "Enter value", None));
            }
            NewKind::Zset => {
                self.input_a = build_input_text_area("Member", "Enter member", None);
                self.input_b = Some(build_input_text_area("Score", "Enter score", None));
            }
        }
    }

    /// Convert the finished draft into a [`NewKeySpec`] paired with its name.
    /// Returns `None` if no element was supplied.
    fn into_spec(self) -> Option<(String, NewKeySpec)> {
        let spec = match self.kind {
            NewKind::String => NewKeySpec::String(self.string_value?),
            NewKind::List => {
                if self.list_items.is_empty() {
                    return None;
                }
                NewKeySpec::List(self.list_items)
            }
            NewKind::Set => {
                if self.list_items.is_empty() {
                    return None;
                }
                NewKeySpec::Set(self.list_items)
            }
            NewKind::Hash => {
                if self.hash_pairs.is_empty() {
                    return None;
                }
                NewKeySpec::Hash(self.hash_pairs)
            }
            NewKind::Zset => {
                if self.zset_pairs.is_empty() {
                    return None;
                }
                NewKeySpec::Zset(self.zset_pairs)
            }
        };
        Some((self.name, spec))
    }
}

/// Draft state for the in-collection add-item overlay (`i`). Scoped to the
/// already-selected collection key; `kind` is its [`RedisType`].
struct AddItemDraft {
    key: String,
    kind: RedisType,
    input_a: TextArea<'static>,
    input_b: Option<TextArea<'static>>,
    active: ActiveField,
    notice: Option<String>,
}

/// The concrete collection element the value-table cursor points at, resolved
/// from the loaded `selected_value` snapshot so it matches what the user sees
/// (important for HASH, whose map order is not stable across reloads).
#[derive(Clone)]
enum SelectedElement {
    ListIndex { index: usize, value: String },
    SetMember(String),
    HashField { field: String, value: String },
    ZsetMember { member: String, score: f64 },
}

impl SelectedElement {
    /// One-line human label for the element, used in the delete-item confirm.
    fn label(&self) -> String {
        match self {
            SelectedElement::ListIndex { index, value } => format!("[{index}] {value}"),
            SelectedElement::SetMember(member) | SelectedElement::ZsetMember { member, .. } => {
                member.clone()
            }
            SelectedElement::HashField { field, .. } => field.clone(),
        }
    }

    /// Translate the element into its deletion request.
    fn into_delete(self) -> ItemDelete {
        match self {
            SelectedElement::ListIndex { index, .. } => ItemDelete::ListIndex(index),
            SelectedElement::SetMember(member) => ItemDelete::SetMember(member),
            SelectedElement::HashField { field, .. } => ItemDelete::HashField(field),
            SelectedElement::ZsetMember { member, .. } => ItemDelete::ZsetMember(member),
        }
    }
}

/// Draft state for the in-collection edit-item overlay (`e` inside a value).
/// Scoped to one resolved element; edits its value/score only (SET replaces the
/// member). The `target` snapshot is captured at open time so confirming builds
/// the right request even if the underlying value later reloads.
struct EditItemDraft {
    key: String,
    target: SelectedElement,
    input: TextArea<'static>,
    notice: Option<String>,
}

pub struct KeySpace {
    table: TableState,
    value_table: TableState,
    keys: Vec<KeyMeta>,
    cursor: Option<usize>,
    pattern: Option<String>,
    mode: KeySpaceMode,
    text_area: Option<TextArea<'static>>,
    selected_value: Option<KeyValue>,
    focus: Focus,
    action_overlay: Option<ActionOverlay>,
    new_key: Option<NewKeyDraft>,
    add_item: Option<AddItemDraft>,
    edit_item: Option<EditItemDraft>,
}

impl KeySpace {
    pub fn new(keys: Vec<KeyMeta>) -> Self {
        Self {
            keys,
            table: TableState::default(),
            value_table: TableState::default(),
            cursor: None,
            pattern: None,
            mode: KeySpaceMode::Normal,
            text_area: None,
            selected_value: None,
            focus: Focus::Keys,
            action_overlay: None,
            new_key: None,
            add_item: None,
            edit_item: None,
        }
    }

    pub fn selected_key(&self) -> Option<(String, RedisType)> {
        let idx = self.table.selected()?;
        let meta = self.keys.get(idx)?;
        Some((meta.key.clone(), meta.r_type))
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

    pub fn set_selected_value(&mut self, value: Option<KeyValue>) {
        self.selected_value = value;
    }

    pub fn clear_selected_value(&mut self) {
        self.selected_value = None;
        // The detail-view selection belongs to the previously selected key;
        // reset it (and drop value focus) so the new value starts fresh.
        self.value_table.select(None);
        self.focus = Focus::Keys;
    }

    pub fn is_popup(&self) -> bool {
        matches!(self.mode, KeySpaceMode::Popup(_))
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if let Some(ref mut text_area) = self.text_area {
            text_area.input(key);
        }
    }

    pub fn enter_filter_pattern(&mut self) {
        self.mode = KeySpaceMode::Popup(KeySpacePopupMode::FilterPattern);
        self.text_area = Some(build_input_text_area("Pattern", "Enter pattern", None));
    }

    pub fn confirm_filter_pattern(&mut self) -> Option<String> {
        let pattern = if let Some(text_area) = self.text_area.take() {
            let line = text_area.lines()[0].clone();

            if line.is_empty() {
                self.pattern = None;
            } else {
                self.pattern = Some(line.clone());
            }

            Some(line)
        } else {
            None
        };

        self.exit_popup();
        pattern
    }

    pub fn exit_popup(&mut self) {
        self.mode = KeySpaceMode::Normal;
    }

    pub fn refresh(&mut self) {
        self.table.select(None);
    }

    pub fn update_filters(&mut self, pattern: Option<String>, cursor: Option<usize>) {
        self.cursor = cursor;
        self.pattern = pattern;
    }

    pub fn set_keys(&mut self, keys: Vec<KeyMeta>) {
        _ = std::mem::replace(&mut self.keys, keys);
    }

    pub fn scroll_next(&mut self) {
        match self.focus {
            Focus::Keys => {
                let wrap_index = self.keys.len().max(1);
                let next = self.table.selected().map_or(0, |i| (i + 1) % wrap_index);
                self.scroll_to(next);
            }
            Focus::Value => self.scroll_value(true),
        }
    }

    pub fn scroll_previous(&mut self) {
        match self.focus {
            Focus::Keys => {
                let last: usize = self.keys.len().saturating_sub(1);
                let wrap_index = self.keys.len().max(1);
                let previous = self
                    .table
                    .selected()
                    .map_or(last, |i: usize| (i + last) % wrap_index);
                self.scroll_to(previous);
            }
            Focus::Value => self.scroll_value(false),
        }
    }

    fn scroll_to(&mut self, index: usize) {
        if self.keys.is_empty() {
            self.table.select(None);
        } else {
            self.table.select(Some(index));
        }
    }

    /// Move the detail-view selection within the currently loaded collection
    /// value, wrapping at the ends. No-op when the value has no rows.
    fn scroll_value(&mut self, forward: bool) {
        let len = self.value_row_count();
        if len == 0 {
            self.value_table.select(None);
            return;
        }
        let next = match (self.value_table.selected(), forward) {
            (Some(i), true) => (i + 1) % len,
            (Some(i), false) => (i + len - 1) % len,
            (None, true) => 0,
            (None, false) => len - 1,
        };
        self.value_table.select(Some(next));
    }

    /// Number of rows the loaded value renders as a table. Scalar/absent values
    /// have no navigable rows.
    fn value_row_count(&self) -> usize {
        match self.selected_value.as_ref() {
            Some(KeyValue::List(items)) => items.len(),
            Some(KeyValue::Set(members)) => members.len(),
            Some(KeyValue::Hash(entries)) => entries.len(),
            Some(KeyValue::Zset(entries)) => entries.len(),
            Some(KeyValue::String(_) | KeyValue::Json(_) | KeyValue::Unknown) | None => 0,
        }
    }

    /// True when the loaded value is a collection that can be navigated row by
    /// row (the precondition for [`Self::enter_value`]).
    fn value_is_navigable(&self) -> bool {
        self.value_row_count() > 0
    }

    /// Drill focus into the value table so `j`/`k` scroll its rows. Only takes
    /// effect for navigable collection values; scalar/empty values are ignored.
    pub fn enter_value(&mut self) {
        if !self.value_is_navigable() {
            return;
        }
        self.focus = Focus::Value;
        self.value_table.select(Some(0));
    }

    /// Return focus to the key list, clearing the in-value selection.
    pub fn exit_value_focus(&mut self) {
        self.focus = Focus::Keys;
        self.value_table.select(None);
    }

    pub fn is_value_focused(&self) -> bool {
        self.focus == Focus::Value
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
    /// reading the exact `selected_value` snapshot the user is looking at (so
    /// HASH — whose map order is unstable across reloads — resolves correctly).
    /// `None` unless value-focused with a selected navigable row.
    fn selected_element(&self) -> Option<SelectedElement> {
        if !self.is_value_focused() {
            return None;
        }
        let row = self.value_table.selected()?;
        match self.selected_value.as_ref()? {
            KeyValue::List(items) => items.get(row).map(|v| SelectedElement::ListIndex {
                index: row,
                value: v.clone(),
            }),
            KeyValue::Set(members) => members
                .iter()
                .nth(row)
                .map(|m| SelectedElement::SetMember(m.clone())),
            KeyValue::Hash(map) => map
                .iter()
                .nth(row)
                .map(|(f, v)| SelectedElement::HashField {
                    field: f.clone(),
                    value: v.clone(),
                }),
            KeyValue::Zset(items) => items.get(row).map(|(m, s)| SelectedElement::ZsetMember {
                member: m.clone(),
                score: *s,
            }),
            KeyValue::String(_) | KeyValue::Json(_) | KeyValue::Unknown => None,
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

    // --- Action overlays (delete / edit / TTL / notice) ---

    pub fn is_overlay_open(&self) -> bool {
        self.action_overlay.is_some()
    }

    /// True while a text-input overlay is active and should receive raw keys.
    pub fn is_input_active(&self) -> bool {
        self.action_overlay
            .as_ref()
            .is_some_and(|o| o.text_area.is_some())
    }

    pub fn handle_overlay_key(&mut self, key: KeyEvent) {
        if let Some(overlay) = self.action_overlay.as_mut() {
            if let Some(text_area) = overlay.text_area.as_mut() {
                text_area.input(key);
            }
        }
    }

    /// Open a delete-confirmation prompt for the selected key.
    pub fn open_delete_confirm(&mut self) {
        self.action_overlay = Some(ActionOverlay {
            kind: OverlayKind::ConfirmDelete,
            text_area: None,
        });
    }

    /// Open a delete-confirmation prompt for the selected collection element.
    /// No-op unless a navigable element is currently focused.
    pub fn open_delete_item_confirm(&mut self) {
        let Some(target) = self.selected_element() else {
            return;
        };
        self.action_overlay = Some(ActionOverlay {
            kind: OverlayKind::ConfirmDeleteItem {
                label: target.label(),
                target,
            },
            text_area: None,
        });
    }

    /// Open a TTL input prompt (seconds). Empty / non-positive clears the TTL.
    pub fn open_set_ttl(&mut self) {
        self.action_overlay = Some(ActionOverlay {
            kind: OverlayKind::SetTtl,
            text_area: Some(build_input_text_area(
                "TTL (seconds, empty = persist)",
                "Enter seconds",
                None,
            )),
        });
    }

    /// Open the value editor. STRING values pre-fill the editor; any other type
    /// opens a read-only "temporarily unsupported" notice instead.
    pub fn open_edit_value(&mut self) {
        match self.selected_value.as_ref() {
            Some(KeyValue::String(s)) => {
                self.action_overlay = Some(ActionOverlay {
                    kind: OverlayKind::EditValue,
                    text_area: Some(build_input_text_area("Edit value", "Enter value", Some(s))),
                });
            }
            _ => {
                self.action_overlay = Some(ActionOverlay {
                    kind: OverlayKind::Notice(
                        "Editing this type is temporarily unsupported.".to_string(),
                    ),
                    text_area: None,
                });
            }
        }
    }

    pub fn close_overlay(&mut self) {
        self.action_overlay = None;
    }

    /// Resolve the open overlay into a pending write request, consuming the
    /// overlay. Returns `None` for notice-only overlays (nothing to do).
    pub fn take_overlay_request(&mut self) -> Option<OverlayRequest> {
        let overlay = self.action_overlay.take()?;
        match overlay.kind {
            OverlayKind::ConfirmDelete => Some(OverlayRequest::Delete),
            OverlayKind::ConfirmDeleteItem { target, .. } => {
                Some(OverlayRequest::DeleteItem(target.into_delete()))
            }
            OverlayKind::EditValue => {
                let value = overlay
                    .text_area
                    .as_ref()
                    .map_or_else(String::new, input_text);
                Some(OverlayRequest::SetString(value))
            }
            OverlayKind::SetTtl => {
                let raw = overlay
                    .text_area
                    .as_ref()
                    .map(input_text)
                    .unwrap_or_default();
                // Empty or unparsable / non-positive ⇒ clear the TTL (persist).
                let secs = raw.trim().parse::<i64>().unwrap_or(0);
                Some(OverlayRequest::SetTtl(secs))
            }
            OverlayKind::Notice(_) => None,
        }
    }

    // --- Add-key wizard ---

    pub fn is_wizard_active(&self) -> bool {
        self.new_key.is_some()
    }

    /// Open the add-key wizard at the type-selection step.
    pub fn open_new_key(&mut self) {
        self.new_key = Some(NewKeyDraft::new());
    }

    pub fn close_new_key(&mut self) {
        self.new_key = None;
    }

    /// True once the wizard has signalled it wants to finalize (STRING `Enter`,
    /// or a collection `Tab`-finish with ≥1 element). `App` checks this after
    /// routing each wizard key and, when set, drains
    /// [`Self::take_new_key_request`].
    pub fn wizard_wants_finalize(&self) -> bool {
        self.new_key.as_ref().is_some_and(|d| d.finalize)
    }

    /// Feed a raw key into whichever wizard input currently has focus.
    fn wizard_input(&mut self, key: KeyEvent) {
        let Some(draft) = self.new_key.as_mut() else {
            return;
        };
        match draft.active {
            ActiveField::A => {
                draft.input_a.input(key);
            }
            ActiveField::B => {
                if let Some(input_b) = draft.input_b.as_mut() {
                    input_b.input(key);
                }
            }
        }
    }

    /// Drive the wizard's step machine. Returns a finished
    /// [`OverlayRequest::CreateKey`] when the user finalizes; otherwise `None`.
    /// `App` routes `Enter`/`Esc` here as `commit`/`cancel`; all other keys are
    /// raw input (handled before this is called).
    #[allow(clippy::too_many_lines)]
    pub fn handle_wizard_key(&mut self, key: KeyEvent) {
        let Some(draft) = self.new_key.as_mut() else {
            return;
        };

        match draft.step {
            NewKeyStep::Type => match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    draft.kind_cursor = (draft.kind_cursor + 1) % NewKind::ALL.len();
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    draft.kind_cursor =
                        (draft.kind_cursor + NewKind::ALL.len() - 1) % NewKind::ALL.len();
                }
                KeyCode::Enter => {
                    draft.kind = NewKind::ALL[draft.kind_cursor];
                    draft.step = NewKeyStep::Name;
                    draft.input_a = build_input_text_area("Key name", "Enter key name", None);
                    draft.input_b = None;
                    draft.active = ActiveField::A;
                }
                _ => {}
            },
            NewKeyStep::Name => {
                if key.code == KeyCode::Enter {
                    let name = input_text(&draft.input_a).trim().to_string();
                    if name.is_empty() {
                        return;
                    }
                    draft.name = name;
                    draft.enter_element_step();
                } else if key.code == KeyCode::Tab {
                    // No-op on the single-input name step.
                } else {
                    self.wizard_input(key);
                }
            }
            NewKeyStep::Element => self.handle_element_key(key),
            NewKeyStep::Notice(_) => {
                if matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                    if let Some(d) = self.new_key.as_mut() {
                        d.step = NewKeyStep::Element;
                    }
                }
            }
        }
    }

    /// Handle a key on the repeatable element step. Controls are unified across
    /// every kind:
    ///
    /// - `Enter`: STRING finalizes; collections commit the current element and
    ///   stay for the next one.
    /// - `Ctrl+S`: finish — commit any pending element, then finalize (collections
    ///   need ≥1 element).
    /// - `Tab`: switch field on the paired kinds (HASH/ZSET); ignored otherwise.
    /// - `Esc`: cancel the whole flow (handled by `App`).
    fn handle_element_key(&mut self, key: KeyEvent) {
        let Some(draft) = self.new_key.as_mut() else {
            return;
        };

        let is_finish = key.code == KeyCode::Char('s')
            && key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL);

        if is_finish {
            self.finish_element();
            return;
        }

        match key.code {
            KeyCode::Enter if matches!(draft.kind, NewKind::String) => {
                draft.finalize = true;
            }
            KeyCode::Enter => self.commit_element(),
            KeyCode::Tab if draft.kind.is_pair() => {
                draft.active = match draft.active {
                    ActiveField::A => ActiveField::B,
                    ActiveField::B => ActiveField::A,
                };
            }
            // Tab is meaningless on single-input kinds; swallow it so it doesn't
            // get typed into the input.
            KeyCode::Tab | KeyCode::BackTab => {}
            _ => self.wizard_input(key),
        }
    }

    /// Finish a collection: commit any pending (typed-but-uncommitted) element so
    /// a single item the user typed without pressing Enter still counts, then
    /// finalize if there's ≥1 element and no pending validation notice.
    fn finish_element(&mut self) {
        self.commit_element();
        let blocked = matches!(
            self.new_key.as_ref().map(|d| &d.step),
            Some(NewKeyStep::Notice(_))
        );
        if !blocked && self.wizard_can_finalize() {
            if let Some(d) = self.new_key.as_mut() {
                d.finalize = true;
            }
        }
    }

    /// Commit the current element input(s) into the draft's accumulator and
    /// reset the input(s) for the next element.
    fn commit_element(&mut self) {
        let Some(draft) = self.new_key.as_mut() else {
            return;
        };
        match draft.kind {
            NewKind::String => {
                draft.string_value = Some(input_text(&draft.input_a));
            }
            NewKind::List | NewKind::Set => {
                let item = input_text(&draft.input_a);
                if item.is_empty() {
                    return;
                }
                draft.list_items.push(item);
                draft.input_a = build_input_text_area("Item", "Enter item", None);
            }
            NewKind::Hash => {
                let field = input_text(&draft.input_a);
                let value = draft.input_b.as_ref().map_or_else(String::new, input_text);
                if field.is_empty() {
                    return;
                }
                draft.hash_pairs.push((field, value));
                draft.input_a = build_input_text_area("Field", "Enter field", None);
                draft.input_b = Some(build_input_text_area("Value", "Enter value", None));
                draft.active = ActiveField::A;
            }
            NewKind::Zset => {
                let member = input_text(&draft.input_a);
                let raw = draft.input_b.as_ref().map_or_else(String::new, input_text);
                if member.is_empty() {
                    return;
                }
                let Ok(score) = raw.trim().parse::<f64>() else {
                    draft.step = NewKeyStep::Notice("Score must be a number.".to_string());
                    return;
                };
                draft.zset_pairs.push((member, score));
                draft.input_a = build_input_text_area("Member", "Enter member", None);
                draft.input_b = Some(build_input_text_area("Score", "Enter score", None));
                draft.active = ActiveField::A;
            }
        }
    }

    /// Whether the wizard has enough to finalize (STRING after a value commit;
    /// any collection with ≥1 element).
    fn wizard_can_finalize(&self) -> bool {
        self.new_key.as_ref().is_some_and(|d| {
            // A pending element on the input still counts toward STRING via
            // commit on confirm; collections require already-committed items.
            match d.kind {
                NewKind::String => true,
                NewKind::List | NewKind::Set => !d.list_items.is_empty(),
                NewKind::Hash => !d.hash_pairs.is_empty(),
                NewKind::Zset => !d.zset_pairs.is_empty(),
            }
        })
    }

    /// Consume the wizard, finalizing it into a create request. STRING commits
    /// its current input value; collections use their accumulated elements (and
    /// also fold in a pending non-empty element if present). Returns `None` if
    /// nothing valid was supplied.
    pub fn take_new_key_request(&mut self) -> Option<(String, NewKeySpec)> {
        // Fold any pending input into the accumulator before finalizing.
        if let Some(draft) = self.new_key.as_mut() {
            match draft.kind {
                NewKind::String => {
                    draft.string_value = Some(input_text(&draft.input_a));
                }
                NewKind::List | NewKind::Set => {
                    let item = input_text(&draft.input_a);
                    if !item.is_empty() {
                        draft.list_items.push(item);
                    }
                }
                NewKind::Hash => {
                    let field = input_text(&draft.input_a);
                    let value = draft.input_b.as_ref().map_or_else(String::new, input_text);
                    if !field.is_empty() {
                        draft.hash_pairs.push((field, value));
                    }
                }
                NewKind::Zset => {
                    let member = input_text(&draft.input_a);
                    let raw = draft.input_b.as_ref().map_or_else(String::new, input_text);
                    if !member.is_empty() {
                        if let Ok(score) = raw.trim().parse::<f64>() {
                            draft.zset_pairs.push((member, score));
                        }
                    }
                }
            }
        }
        let draft = self.new_key.take()?;
        draft.into_spec()
    }

    // --- In-collection add-item (`i`) ---

    pub fn is_add_item_active(&self) -> bool {
        self.add_item.is_some()
    }

    /// Open the add-item overlay for the focused collection `key` of type `kind`.
    /// Scalar/JSON types are rejected (the caller already restricts to focus).
    pub fn open_add_item(&mut self, key: String, kind: RedisType) {
        let (input_a, input_b) = match kind {
            RedisType::List => (build_input_text_area("Item", "Enter item", None), None),
            RedisType::Set => (build_input_text_area("Member", "Enter member", None), None),
            RedisType::Hash => (
                build_input_text_area("Field", "Enter field", None),
                Some(build_input_text_area("Value", "Enter value", None)),
            ),
            RedisType::Zset => (
                build_input_text_area("Member", "Enter member", None),
                Some(build_input_text_area("Score", "Enter score", None)),
            ),
            RedisType::String | RedisType::Json | RedisType::Unknown => return,
        };
        self.add_item = Some(AddItemDraft {
            key,
            kind,
            input_a,
            input_b,
            active: ActiveField::A,
            notice: None,
        });
    }

    pub fn close_add_item(&mut self) {
        self.add_item = None;
    }

    /// Feed a raw key into the active add-item input, or toggle paired inputs on
    /// `Tab`. (`Enter`/`Esc` are routed by `App` to confirm/cancel.)
    pub fn handle_add_item_key(&mut self, key: KeyEvent) {
        let Some(draft) = self.add_item.as_mut() else {
            return;
        };
        // Dismiss a notice on any key.
        if draft.notice.is_some() {
            draft.notice = None;
            return;
        }
        match key.code {
            KeyCode::Tab if draft.input_b.is_some() => {
                draft.active = match draft.active {
                    ActiveField::A => ActiveField::B,
                    ActiveField::B => ActiveField::A,
                };
            }
            _ => match draft.active {
                ActiveField::A => {
                    draft.input_a.input(key);
                }
                ActiveField::B => {
                    if let Some(input_b) = draft.input_b.as_mut() {
                        input_b.input(key);
                    }
                }
            },
        }
    }

    /// Consume the add-item overlay into a `(key, item)` request. Returns `None`
    /// on invalid input (empty field / unparsable score), leaving a notice for
    /// the user instead.
    pub fn take_add_item_request(&mut self) -> Option<(String, KeyItem)> {
        let draft = self.add_item.as_mut()?;
        let a = input_text(&draft.input_a);
        let b = draft.input_b.as_ref().map_or_else(String::new, input_text);
        let item = match draft.kind {
            RedisType::List => {
                if a.is_empty() {
                    return None;
                }
                KeyItem::ListValue(a)
            }
            RedisType::Set => {
                if a.is_empty() {
                    return None;
                }
                KeyItem::SetMember(a)
            }
            RedisType::Hash => {
                if a.is_empty() {
                    return None;
                }
                KeyItem::HashField { field: a, value: b }
            }
            RedisType::Zset => {
                if a.is_empty() {
                    return None;
                }
                let Ok(score) = b.trim().parse::<f64>() else {
                    draft.notice = Some("Score must be a number.".to_string());
                    return None;
                };
                KeyItem::ZsetMember { member: a, score }
            }
            RedisType::String | RedisType::Json | RedisType::Unknown => return None,
        };
        let key = draft.key.clone();
        self.add_item = None;
        Some((key, item))
    }

    // --- In-collection edit-item (`e` inside a value) ---

    pub fn is_edit_item_active(&self) -> bool {
        self.edit_item.is_some()
    }

    /// Open the edit-item overlay for the currently focused collection element.
    /// Seeds the input with the editable field (value/score/member) and remembers
    /// the resolved identity. No-op unless a navigable element is focused.
    pub fn open_edit_item(&mut self, key: String) {
        let Some(target) = self.selected_element() else {
            return;
        };
        let (title, seed) = match &target {
            SelectedElement::ListIndex { index, value } => {
                (format!("Value [#{index}]"), value.clone())
            }
            SelectedElement::SetMember(member) => ("Member".to_string(), member.clone()),
            SelectedElement::HashField { field, value } => {
                (format!("Value (field: {field})"), value.clone())
            }
            SelectedElement::ZsetMember { member, score } => {
                (format!("Score (member: {member})"), score.to_string())
            }
        };
        let input = build_input_text_area(&title, "Enter value", Some(&seed));
        self.edit_item = Some(EditItemDraft {
            key,
            target,
            input,
            notice: None,
        });
    }

    pub fn close_edit_item(&mut self) {
        self.edit_item = None;
    }

    /// Feed a raw key into the edit-item input (single field). A pending notice is
    /// dismissed on the next key. (`Enter`/`Esc` are routed by `App`.)
    pub fn handle_edit_item_key(&mut self, key: KeyEvent) {
        let Some(draft) = self.edit_item.as_mut() else {
            return;
        };
        if draft.notice.is_some() {
            draft.notice = None;
            return;
        }
        draft.input.input(key);
    }

    /// Consume the edit-item overlay into a `(key, edit)` request. Returns `None`
    /// on invalid input (unparsable ZSET score), leaving a notice for the user.
    pub fn take_edit_item_request(&mut self) -> Option<(String, ItemEdit)> {
        let draft = self.edit_item.as_mut()?;
        let text = input_text(&draft.input);
        let edit = match &draft.target {
            SelectedElement::ListIndex { index, .. } => ItemEdit::ListSet {
                index: *index,
                value: text,
            },
            SelectedElement::SetMember(old) => ItemEdit::SetReplace {
                old: old.clone(),
                new: text,
            },
            SelectedElement::HashField { field, .. } => ItemEdit::HashSet {
                field: field.clone(),
                value: text,
            },
            SelectedElement::ZsetMember { member, .. } => {
                let Ok(score) = text.trim().parse::<f64>() else {
                    draft.notice = Some("Score must be a number.".to_string());
                    return None;
                };
                ItemEdit::ZsetScore {
                    member: member.clone(),
                    score,
                }
            }
        };
        let key = draft.key.clone();
        self.edit_item = None;
        Some((key, edit))
    }
}

/// A confirmed action overlay, ready for `App` to translate into a `RedisEvent`.
pub enum OverlayRequest {
    Delete,
    DeleteItem(ItemDelete),
    SetString(String),
    SetTtl(i64),
}

/// Read the first line of a single-line input text-area.
fn input_text(text_area: &TextArea<'static>) -> String {
    text_area.lines().first().cloned().unwrap_or_default()
}

/// Build a single-line bordered input box matching the keyspace style,
/// optionally seeded with `initial` content.
fn build_input_text_area(
    title: &str,
    placeholder: &str,
    initial: Option<&str>,
) -> TextArea<'static> {
    let cfg = config::get();
    let mut text_area = match initial {
        Some(value) => {
            let mut ta = TextArea::new(vec![value.to_string()]);
            // Start the cursor at the end so the user can append/edit the tail
            // without first walking past every character.
            ta.move_cursor(CursorMove::End);
            ta
        }
        None => TextArea::default(),
    };
    text_area.set_placeholder_text(placeholder);
    text_area.set_block(
        Block::default()
            .border_style(cfg.colors.base04)
            .border_type(BorderType::Rounded)
            .borders(Borders::ALL)
            .title(title.to_string()),
    );
    text_area
}

/// Render one of two paired inputs (hash field/value, zset member/score) with a
/// border color that signals focus: the active field is drawn in an accent
/// color with a `▸ … ◂` title, the inactive one dimmed. Renders a styled clone
/// so the stored `TextArea` keeps its neutral default; `title` is supplied by
/// the caller (ratatui's `Block` exposes no title getter).
fn render_focused_input(
    text_area: &TextArea<'static>,
    title: &str,
    is_active: bool,
    area: Rect,
    buf: &mut Buffer,
) {
    let cfg = config::get();
    let mut clone = text_area.clone();

    let (border, title_line) = if is_active {
        (cfg.colors.base0a, format!("▸ {title} ◂"))
    } else {
        (cfg.colors.base03, title.to_string())
    };

    clone.set_block(
        Block::default()
            .border_style(border)
            .border_type(BorderType::Rounded)
            .borders(Borders::ALL)
            .title(title_line),
    );
    clone.render(area, buf);
}

/// Titles for the two paired add-item inputs of a collection type. Single-input
/// types return their label in `A` (their `B` is unused).
fn add_item_pair_titles(kind: RedisType) -> (&'static str, &'static str) {
    match kind {
        RedisType::Hash => ("Field", "Value"),
        RedisType::Zset => ("Member", "Score"),
        RedisType::Set => ("Member", ""),
        RedisType::List | RedisType::String | RedisType::Json | RedisType::Unknown => ("Item", ""),
    }
}

pub struct KeySpaceWidget;

impl KeySpaceWidget {
    fn render_confirm_popup(state: &mut KeySpace, area: Rect, buf: &mut Buffer) {
        let [_, popup_area, _] = Layout::vertical([
            Constraint::Percentage(40),
            Constraint::Min(3),
            Constraint::Percentage(40),
        ])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);

        let [_, popup_area, _] = Layout::horizontal([
            Constraint::Percentage(30),
            Constraint::Min(15),
            Constraint::Percentage(30),
        ])
        .flex(ratatui::layout::Flex::Center)
        .areas(popup_area);

        if let Some(ref text_area) = state.text_area {
            text_area.render(popup_area, buf);
        }
    }

    /// Render the active action overlay (delete-confirm, edit, TTL input, or a
    /// notice) as a centered modal box on top of the keyspace. Mirrors the
    /// centering approach of [`Self::render_confirm_popup`].
    fn render_action_overlay(state: &KeySpace, area: Rect, buf: &mut Buffer) {
        let Some(overlay) = state.action_overlay.as_ref() else {
            return;
        };
        let cfg = config::get();

        let [_, popup_area, _] = Layout::vertical([
            Constraint::Percentage(40),
            Constraint::Length(5),
            Constraint::Percentage(40),
        ])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);

        let [_, popup_area, _] = Layout::horizontal([
            Constraint::Percentage(20),
            Constraint::Min(30),
            Constraint::Percentage(20),
        ])
        .flex(ratatui::layout::Flex::Center)
        .areas(popup_area);

        Clear.render(popup_area, buf);

        // Input overlays render their own bordered text-area; non-input overlays
        // render a bordered message with a key hint.
        if let Some(ref text_area) = overlay.text_area {
            text_area.render(popup_area, buf);
            return;
        }

        let delete_item_body;
        let (title, body, hint): (&str, &str, &str) = match &overlay.kind {
            OverlayKind::ConfirmDelete => (
                " Delete key ",
                "Delete the selected key?",
                "Enter: confirm   Esc: cancel",
            ),
            OverlayKind::ConfirmDeleteItem { label, .. } => {
                delete_item_body = format!("Delete item: {label}?");
                (
                    " Delete item ",
                    delete_item_body.as_str(),
                    "Enter: confirm   Esc: cancel",
                )
            }
            OverlayKind::Notice(msg) => (" Notice ", msg.as_str(), "Esc: dismiss"),
            // The input kinds are handled above; unreachable here.
            OverlayKind::EditValue | OverlayKind::SetTtl => return,
        };

        let block = Block::default()
            .title(title)
            .title_alignment(Alignment::Center)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(cfg.colors.base04)
            .bg(cfg.colors.base00)
            .fg(cfg.colors.base05);

        let lines = vec![
            Line::from(body).alignment(Alignment::Center),
            Line::from(""),
            Line::styled(hint, cfg.colors.base04).alignment(Alignment::Center),
        ];

        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(block)
            .render(popup_area, buf);
    }

    /// Center a popup of `height` rows in `area`, returning the inner Rect (after
    /// clearing it). Mirrors the centering used by the other overlays.
    fn centered_popup(area: Rect, height: u16, buf: &mut Buffer) -> Rect {
        let [_, popup_area, _] = Layout::vertical([
            Constraint::Percentage(30),
            Constraint::Length(height),
            Constraint::Percentage(30),
        ])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);

        let [_, popup_area, _] = Layout::horizontal([
            Constraint::Percentage(20),
            Constraint::Min(34),
            Constraint::Percentage(20),
        ])
        .flex(ratatui::layout::Flex::Center)
        .areas(popup_area);

        Clear.render(popup_area, buf);
        popup_area
    }

    /// Render the add-key wizard overlay for whichever step is active.
    fn render_new_key_overlay(state: &KeySpace, area: Rect, buf: &mut Buffer) {
        let Some(draft) = state.new_key.as_ref() else {
            return;
        };
        let cfg = config::get();

        match &draft.step {
            NewKeyStep::Type => {
                // 5 kinds + border (2) + blank + hint line.
                let popup = Self::centered_popup(area, NEW_KIND_POPUP_HEIGHT, buf);
                let block = Block::default()
                    .title(" Select type ")
                    .title_alignment(Alignment::Center)
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(cfg.colors.base04)
                    .bg(cfg.colors.base00)
                    .fg(cfg.colors.base05);

                let mut lines: Vec<Line> = NewKind::ALL
                    .iter()
                    .enumerate()
                    .map(|(idx, kind)| {
                        let marker = if idx == draft.kind_cursor {
                            "(●)"
                        } else {
                            "( )"
                        };
                        let line = Line::from(format!("  {marker} {}", kind.label()));
                        if idx == draft.kind_cursor {
                            line.style(cfg.colors.base04)
                        } else {
                            line
                        }
                    })
                    .collect();
                lines.push(Line::from(""));
                lines.push(
                    Line::styled("j/k move   Enter: next   Esc: cancel", cfg.colors.base03)
                        .alignment(Alignment::Center),
                );

                Paragraph::new(lines).block(block).render(popup, buf);
            }
            NewKeyStep::Name => {
                let popup = Self::centered_popup(area, 5, buf);
                let [input_area, hint_area] =
                    Layout::vertical([Constraint::Length(3), Constraint::Length(1)]).areas(popup);
                draft.input_a.render(input_area, buf);
                Self::render_wizard_hint(
                    &format!("New {} key   Enter: next   Esc: cancel", draft.kind.label()),
                    hint_area,
                    buf,
                );
            }
            NewKeyStep::Element => Self::render_element_inputs(draft, area, buf),
            NewKeyStep::Notice(msg) => {
                let popup = Self::centered_popup(area, 5, buf);
                let block = Block::default()
                    .title(" Notice ")
                    .title_alignment(Alignment::Center)
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(cfg.colors.base04)
                    .bg(cfg.colors.base00)
                    .fg(cfg.colors.base05);
                let lines = vec![
                    Line::from(msg.as_str()).alignment(Alignment::Center),
                    Line::from(""),
                    Line::styled("Enter / Esc: back", cfg.colors.base03)
                        .alignment(Alignment::Center),
                ];
                Paragraph::new(lines)
                    .wrap(Wrap { trim: true })
                    .block(block)
                    .render(popup, buf);
            }
        }
    }

    /// Render the (one or two) element inputs plus a running summary + hint for
    /// the wizard's repeatable element step.
    fn render_element_inputs(draft: &NewKeyDraft, area: Rect, buf: &mut Buffer) {
        let count = draft.item_count();
        let hint = if draft.kind.is_pair() {
            format!("{count} added   Tab: switch field   Enter: add   Ctrl+S: finish   Esc: cancel")
        } else if matches!(draft.kind, NewKind::String) {
            "Enter: create   Esc: cancel".to_string()
        } else {
            format!("{count} added   Enter: add   Ctrl+S: finish   Esc: cancel")
        };

        if let Some(input_b) = draft.input_b.as_ref() {
            let popup = Self::centered_popup(area, 7, buf);
            let [a_area, b_area, hint_area] = Layout::vertical([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .areas(popup);
            let (title_a, title_b) = draft.kind.pair_titles();
            let a_active = draft.active == ActiveField::A;
            render_focused_input(&draft.input_a, title_a, a_active, a_area, buf);
            render_focused_input(input_b, title_b, !a_active, b_area, buf);
            Self::render_wizard_hint(&hint, hint_area, buf);
        } else {
            let popup = Self::centered_popup(area, 5, buf);
            let [a_area, hint_area] =
                Layout::vertical([Constraint::Length(3), Constraint::Length(1)]).areas(popup);
            draft.input_a.render(a_area, buf);
            Self::render_wizard_hint(&hint, hint_area, buf);
        }
    }

    /// Render the add-item overlay for the focused collection.
    fn render_add_item_overlay(state: &KeySpace, area: Rect, buf: &mut Buffer) {
        let Some(draft) = state.add_item.as_ref() else {
            return;
        };
        let cfg = config::get();

        if let Some(notice) = draft.notice.as_ref() {
            let popup = Self::centered_popup(area, 5, buf);
            let block = Block::default()
                .title(" Notice ")
                .title_alignment(Alignment::Center)
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(cfg.colors.base04)
                .bg(cfg.colors.base00)
                .fg(cfg.colors.base05);
            let lines = vec![
                Line::from(notice.as_str()).alignment(Alignment::Center),
                Line::from(""),
                Line::styled("Any key: back", cfg.colors.base03).alignment(Alignment::Center),
            ];
            Paragraph::new(lines)
                .wrap(Wrap { trim: true })
                .block(block)
                .render(popup, buf);
            return;
        }

        let hint = format!(
            "Add to {key}   {keys}Enter: add   Esc: cancel",
            key = draft.key,
            keys = if draft.input_b.is_some() {
                "Tab: switch field   "
            } else {
                ""
            },
        );

        if let Some(input_b) = draft.input_b.as_ref() {
            let popup = Self::centered_popup(area, 7, buf);
            let [a_area, b_area, hint_area] = Layout::vertical([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .areas(popup);
            let (title_a, title_b) = add_item_pair_titles(draft.kind);
            let a_active = draft.active == ActiveField::A;
            render_focused_input(&draft.input_a, title_a, a_active, a_area, buf);
            render_focused_input(input_b, title_b, !a_active, b_area, buf);
            Self::render_wizard_hint(&hint, hint_area, buf);
        } else {
            let popup = Self::centered_popup(area, 5, buf);
            let [a_area, hint_area] =
                Layout::vertical([Constraint::Length(3), Constraint::Length(1)]).areas(popup);
            draft.input_a.render(a_area, buf);
            Self::render_wizard_hint(&hint, hint_area, buf);
        }
    }

    /// Render the edit-item overlay for the focused collection element.
    fn render_edit_item_overlay(state: &KeySpace, area: Rect, buf: &mut Buffer) {
        let Some(draft) = state.edit_item.as_ref() else {
            return;
        };
        let cfg = config::get();

        if let Some(notice) = draft.notice.as_ref() {
            let popup = Self::centered_popup(area, 5, buf);
            let block = Block::default()
                .title(" Notice ")
                .title_alignment(Alignment::Center)
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(cfg.colors.base04)
                .bg(cfg.colors.base00)
                .fg(cfg.colors.base05);
            let lines = vec![
                Line::from(notice.as_str()).alignment(Alignment::Center),
                Line::from(""),
                Line::styled("Any key: back", cfg.colors.base03).alignment(Alignment::Center),
            ];
            Paragraph::new(lines)
                .wrap(Wrap { trim: true })
                .block(block)
                .render(popup, buf);
            return;
        }

        let popup = Self::centered_popup(area, 5, buf);
        let [input_area, hint_area] =
            Layout::vertical([Constraint::Length(3), Constraint::Length(1)]).areas(popup);
        draft.input.render(input_area, buf);
        Self::render_wizard_hint(
            &format!("Edit {key}   Enter: save   Esc: cancel", key = draft.key),
            hint_area,
            buf,
        );
    }

    fn render_wizard_hint(hint: &str, area: Rect, buf: &mut Buffer) {
        let cfg = config::get();
        Paragraph::new(Line::styled(hint, cfg.colors.base03))
            .alignment(Alignment::Center)
            .render(area, buf);
    }

    fn render_key_view(state: &mut KeySpace, area: Rect, buf: &mut Buffer) {
        let Some(selected_index) = state.table.selected() else {
            return;
        };
        let Some(key) = state.keys.get(selected_index) else {
            return;
        };

        let key_details_block = Block::default()
            .title("Key Details")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded);

        let key_details_area = key_details_block.inner(area);
        key_details_block.render(area, buf);

        let [key_meta_area, view_area] =
            Layout::vertical([Constraint::Max(6), Constraint::Fill(1)])
                .flex(ratatui::layout::Flex::Center)
                .areas(key_details_area);

        let key_info = format!(
            "Key: {key}\nType: {ty:?}\nTTL: {ttl}\nSize: {size}",
            key = key.key,
            ty = key.r_type,
            ttl = key.ttl,
            size = Byte::from_u128(key.size)
                .unwrap_or_default()
                .get_appropriate_unit(UnitType::Binary)
        );

        Paragraph::new(key_info)
            .wrap(Wrap { trim: true })
            .render(key_meta_area, buf);

        let Some(loaded_value) = state.selected_value.as_ref() else {
            Paragraph::new("Loading…")
                .wrap(Wrap { trim: true })
                .render(view_area, buf);
            return;
        };

        Self::render_value(loaded_value, view_area, buf, &mut state.value_table);
    }

    fn render_value(value: &KeyValue, area: Rect, buf: &mut Buffer, table_state: &mut TableState) {
        match value {
            KeyValue::String(s) => {
                Paragraph::new(format!("Value: {s}"))
                    .wrap(Wrap { trim: true })
                    .render(area, buf);
            }
            KeyValue::List(items) => {
                Self::render_single_column_table("Item", items, area, buf, table_state);
            }
            KeyValue::Set(members) => Self::render_single_column_table(
                "Member",
                members.iter().cloned().collect::<Vec<_>>().as_slice(),
                area,
                buf,
                table_state,
            ),
            KeyValue::Hash(entries) => {
                let pairs: Vec<(String, String)> = entries
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                Self::render_two_column_table("Field", "Value", &pairs, area, buf, table_state);
            }
            KeyValue::Zset(entries) => {
                let pairs: Vec<(String, String)> = entries
                    .iter()
                    .map(|(member, score)| (member.clone(), score.to_string()))
                    .collect();
                Self::render_two_column_table("Member", "Score", &pairs, area, buf, table_state);
            }
            KeyValue::Json(_) | KeyValue::Unknown => {}
        }
    }

    fn render_single_column_table(
        header_label: &str,
        rows_data: &[String],
        area: Rect,
        buf: &mut Buffer,
        table_state: &mut TableState,
    ) {
        let cfg = config::get();
        let widths = [Constraint::Percentage(100)];
        let header = Row::new([Cell::from(header_label.bold())])
            .top_margin(1)
            .bottom_margin(1)
            .fg(cfg.colors.base04)
            .bg(cfg.colors.base02);

        let rows = rows_data.iter().map(|item| {
            Row::new([Cell::from(item.as_str())])
                .fg(cfg.colors.base04)
                .bg(cfg.colors.base00)
        });
        let table = Table::new(rows, widths)
            .header(header)
            .flex(ratatui::layout::Flex::Center)
            .highlight_symbol(HIGHLIGHT_SYMBOL)
            .highlight_style(cfg.colors.base05)
            .highlight_spacing(HighlightSpacing::Always);

        StatefulWidget::render(table, area, buf, table_state);
    }

    fn render_two_column_table(
        left_label: &str,
        right_label: &str,
        rows_data: &[(String, String)],
        area: Rect,
        buf: &mut Buffer,
        table_state: &mut TableState,
    ) {
        let cfg = config::get();
        let widths = [Constraint::Percentage(50), Constraint::Percentage(50)];
        let header = Row::new([
            Cell::from(left_label.bold()),
            Cell::from(right_label.bold()),
        ])
        .top_margin(1)
        .bottom_margin(1)
        .fg(cfg.colors.base04)
        .bg(cfg.colors.base02);

        let rows = rows_data.iter().map(|(left, right)| {
            Row::new([Cell::from(left.as_str()), Cell::from(right.as_str())])
                .fg(cfg.colors.base04)
                .bg(cfg.colors.base00)
        });
        let table = Table::new(rows, widths)
            .header(header)
            .widths(widths)
            .flex(ratatui::layout::Flex::Center)
            .highlight_symbol(HIGHLIGHT_SYMBOL)
            .highlight_style(cfg.colors.base05)
            .highlight_spacing(HighlightSpacing::Always);

        StatefulWidget::render(table, area, buf, table_state);
    }
}

const HIGHLIGHT_SYMBOL: &str = " >> ";

/// Height of the type-selector popup: 5 radio rows + 2 borders + blank + hint.
/// (`NewKind::ALL` has 5 entries.)
const NEW_KIND_POPUP_HEIGHT: u16 = 9;

impl StatefulWidget for KeySpaceWidget {
    type State = KeySpace;

    #[allow(clippy::too_many_lines)]
    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let cfg = config::get();
        let [t_area, view_area] =
            Layout::horizontal([Constraint::Percentage(35), Constraint::Fill(1)])
                .flex(ratatui::layout::Flex::Center)
                .areas(area);

        // Highlight whichever pane currently has keyboard focus so it's obvious
        // where `j`/`k` will move after `EnterValue`.
        let value_focused = state.is_value_focused();
        let (keys_border, values_border) = if value_focused {
            (cfg.colors.base03, cfg.colors.base05)
        } else {
            (cfg.colors.base05, cfg.colors.base03)
        };

        let space_block = Block::new()
            .bg(cfg.colors.base00)
            .fg(cfg.colors.base04)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(keys_border)
            .borders(Borders::all())
            .title("Keys");

        let table_area = space_block.inner(t_area);
        space_block.render(t_area, buf);

        Block::new()
            .bg(cfg.colors.base00)
            .fg(cfg.colors.base04)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(values_border)
            .borders(Borders::all())
            .title(if value_focused {
                " Values [focused] "
            } else {
                "Values"
            })
            .render(view_area, buf);

        let [filter_area, table_area] = Layout::vertical([Constraint::Min(2), Constraint::Fill(3)])
            .flex(ratatui::layout::Flex::Center)
            .margin(1)
            .areas(table_area);

        let filters_block = Block::new()
            .bg(cfg.colors.base00)
            .fg(cfg.colors.base04)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .borders(Borders::all())
            .title("Filters");

        let filters_inner = filters_block.inner(filter_area);
        filters_block.render(filter_area, buf);

        let [cursor_size_are, pattern_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1)])
                .flex(ratatui::layout::Flex::Center)
                .areas(filters_inner);

        let [cursor_area, size_area] = Layout::horizontal([Constraint::Min(1), Constraint::Min(1)])
            .flex(ratatui::layout::Flex::Center)
            .areas(cursor_size_are);

        Paragraph::new(format!(
            "Cursor: {cursor}",
            cursor = state.cursor.unwrap_or_default()
        ))
        .bold()
        .alignment(Alignment::Left)
        .render(cursor_area, buf);

        Paragraph::new("Size: 10")
            .bold()
            .alignment(Alignment::Right)
            .render(size_area, buf);

        Paragraph::new(format!(
            "Pattern: {pattern}",
            pattern = state.pattern.as_deref().unwrap_or("*")
        ))
        .bold()
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: true })
        .render(pattern_area, buf);

        let widths = [
            Constraint::Percentage(20),
            Constraint::Percentage(35),
            Constraint::Percentage(20),
            Constraint::Percentage(25),
        ];
        let header: Row<'_> =
            Row::new(["Type", "Key", "TTL(s)", "Size"].map(|h| Cell::from(h.bold())))
                .top_margin(1)
                .bottom_margin(1)
                .fg(cfg.colors.base04)
                .bg(cfg.colors.base02);

        let rows = state.keys.iter().enumerate().map(|(idx, meta)| {
            let badge: ratatui::text::Text = meta.r_type.into();
            Row::new([
                Cell::from(badge),
                Cell::from(meta.key.as_str()),
                Cell::from(meta.ttl.to_string()),
                Cell::from(format!(
                    "{:.2}",
                    Byte::from_u128(meta.size)
                        .unwrap_or_default()
                        .get_appropriate_unit(UnitType::Binary)
                )),
            ])
            .fg(cfg.colors.base04)
            .bg(if idx % 2 == 0 {
                cfg.colors.base00
            } else {
                cfg.colors.base01
            })
        });
        let table: Table<'_> = Table::new(rows, widths)
            .header(header)
            .flex(ratatui::layout::Flex::Center)
            .highlight_symbol(HIGHLIGHT_SYMBOL)
            .highlight_style(cfg.colors.base05)
            .highlight_spacing(HighlightSpacing::Always);

        StatefulWidget::render(table, table_area, buf, &mut state.table);

        Self::render_key_view(state, view_area, buf);

        if state.is_popup() {
            Self::render_confirm_popup(state, area, buf);
        }

        if state.is_overlay_open() {
            Self::render_action_overlay(state, area, buf);
        }

        if state.is_wizard_active() {
            Self::render_new_key_overlay(state, area, buf);
        }

        if state.is_add_item_active() {
            Self::render_add_item_overlay(state, area, buf);
        }

        if state.is_edit_item_active() {
            Self::render_edit_item_overlay(state, area, buf);
        }
    }
}
