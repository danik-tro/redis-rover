use std::time::{Duration, Instant};

use byte_unit::{Byte, UnitType};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, HighlightSpacing, Paragraph, Row, StatefulWidget,
        Table, TableState, Widget, Wrap,
    },
};
use tui_textarea::{CursorMove, TextArea};

use crate::{
    config,
    redis_client::types::{
        zset_entry_json, ItemDelete, ItemEdit, KeyItem, KeyMeta, KeyValue, KeyspaceState,
        LoadedValue, NewKeySpec, RedisType, STRING_PREVIEW_BYTES, TYPE_BADGE_WIDTH,
    },
    utils::format_duration,
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
        }
    }
}

impl PageInfo {
    /// `Page 3 · 21–30 of 57+` (`+` while the sweep is unfinished).
    fn summary(&self) -> String {
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

enum KeySpacePopupMode {
    FilterPattern,
}

/// How long the filter text must stay unchanged before its matches are counted.
const FILTER_COUNT_DEBOUNCE: Duration = Duration::from_millis(300);

/// Live match count shown under the filter input (#24).
#[derive(Debug, Clone)]
struct FilterPreview {
    /// When the filter text last changed.
    edited_at: Instant,
    /// The pattern of the last count request, if any.
    requested: Option<String>,
    /// The last count received: pattern, count, sweep finished.
    result: Option<(String, usize, bool)>,
}

impl FilterPreview {
    fn new(now: Instant) -> Self {
        Self {
            edited_at: now,
            requested: None,
            result: None,
        }
    }

    /// The pattern to count now, if any: the first one right away, later ones
    /// once the text has been idle for [`FILTER_COUNT_DEBOUNCE`].
    fn request(&mut self, text: &str, now: Instant) -> Option<String> {
        if self.requested.as_deref() == Some(text) {
            return None;
        }
        let idle = now.saturating_duration_since(self.edited_at) >= FILTER_COUNT_DEBOUNCE;
        if self.requested.is_some() && !idle {
            return None;
        }
        self.requested = Some(text.to_owned());
        Some(text.to_owned())
    }

    /// Status line for `text`: the count if it's for this exact text, else
    /// `counting…`.
    fn status(&self, text: &str) -> String {
        match &self.result {
            Some((pattern, count, complete)) if pattern == text => {
                let noun = if *count == 1 { "match" } else { "matches" };
                let count = group_thousands(*count);
                if *complete {
                    format!("{count} {noun}")
                } else {
                    format!("≥ {count} {noun} (partial scan)")
                }
            }
            _ => "counting…".to_owned(),
        }
    }
}

/// Redis `TTL` sentinel for a key that exists but has no expiry.
const TTL_NO_EXPIRY: isize = -1;

/// Compact TTL for the table column: `∞` for no expiry, `—` for a missing key,
/// otherwise the two most significant units (`45s`, `1h 30m`, `2d 4h`).
fn format_ttl(ttl: isize) -> String {
    match ttl {
        TTL_NO_EXPIRY => "∞".into(),
        t if t < 0 => "—".into(),
        t => format_duration(t.unsigned_abs()),
    }
}

/// Detail-pane TTL: spelled-out sentinels, and the raw seconds alongside the
/// humanized duration (`1h 30m (5400s)`).
fn format_ttl_detail(ttl: isize) -> String {
    match ttl {
        TTL_NO_EXPIRY => "No expiry".into(),
        t if t < 0 => "Missing".into(),
        t => format!("{} ({t}s)", format_duration(t.unsigned_abs())),
    }
}

enum KeySpaceMode {
    Normal,
    Popup(KeySpacePopupMode),
}

/// Which pane keyboard navigation drives. `Keys` is the default left-hand key
/// list; `Value` is the detail-view table entered via `EnterValue` so `j`/`k`
/// scroll a collection's rows; `Text` scrolls a STRING / JSON value's lines.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Keys,
    Value,
    Text,
}

/// Scroll state of a STRING / JSON value in the detail view (#30). The
/// limits are measured by the last render.
#[derive(Debug, Clone, Copy)]
struct TextView {
    /// First shown row.
    top: usize,
    /// First shown column (only when `wrap` is off).
    left: usize,
    wrap: bool,
    /// Rows the text area showed on the last render.
    height: usize,
    max_top: usize,
    max_left: usize,
}

impl Default for TextView {
    fn default() -> Self {
        Self {
            top: 0,
            left: 0,
            wrap: true,
            height: 0,
            max_top: 0,
            max_left: 0,
        }
    }
}

/// Columns one `←` / `→` press pans a text value.
const TEXT_PAN_STEP: usize = 8;

/// The selected STRING / JSON value as display lines, built once per load.
struct TextCache {
    lines: Vec<Line<'static>>,
    /// The value is JSON (a `RedisJSON` document, or a STRING that parses).
    json: bool,
}

impl TextCache {
    /// Lines for a text value; `None` for collections / unsupported types.
    fn build(value: &KeyValue) -> Option<Self> {
        match value {
            KeyValue::String { value: text, .. } => {
                let parsed = if value.is_truncated() {
                    None
                } else {
                    parse_json_text(text)
                };
                Some(match parsed {
                    Some(json) => Self {
                        lines: json_lines(&json),
                        json: true,
                    },
                    None => Self {
                        lines: plain_lines(text),
                        json: false,
                    },
                })
            }
            KeyValue::Json(json) => Some(Self {
                lines: json_lines(json),
                json: true,
            }),
            KeyValue::List(_)
            | KeyValue::Set(_)
            | KeyValue::Hash(_)
            | KeyValue::Zset(_)
            | KeyValue::Unknown => None,
        }
    }
}

/// Parse a STRING value as JSON if it looks like an object or array.
fn parse_json_text(text: &str) -> Option<serde_json::Value> {
    let trimmed = text.trim_start();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return None;
    }
    serde_json::from_str(text).ok()
}

/// A plain STRING value, one display line per text line.
fn plain_lines(text: &str) -> Vec<Line<'static>> {
    let color = config::get().colors.base05;
    let mut lines: Vec<Line<'static>> = text
        .lines()
        .map(|line| Line::from(line.to_owned()).fg(color))
        .collect();
    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}

/// Pretty-print `value` with 2-space indents and syntax colors.
fn json_lines(value: &serde_json::Value) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    push_json(value, 0, Vec::new(), "", &mut out);
    out
}

/// Append the lines of `value` at `depth`. `prefix` goes before the value on
/// its first line (an object key), `comma` after it on its last line.
fn push_json(
    value: &serde_json::Value,
    depth: usize,
    prefix: Vec<Span<'static>>,
    comma: &'static str,
    out: &mut Vec<Line<'static>>,
) {
    use serde_json::Value;

    let colors = &config::get().colors;
    let punct = |text: &'static str| Span::from(text).fg(colors.base05);
    let line = |spans: Vec<Span<'static>>| {
        let mut all = vec![Span::raw("  ".repeat(depth))];
        all.extend(spans);
        Line::from(all)
    };
    let (open, close) = match value {
        Value::Array(items) if !items.is_empty() => ("[", "]"),
        Value::Object(map) if !map.is_empty() => ("{", "}"),
        scalar => {
            let mut spans = prefix;
            spans.push(json_scalar(scalar));
            spans.push(punct(comma));
            out.push(line(spans));
            return;
        }
    };

    let mut first = prefix;
    first.push(punct(open));
    out.push(line(first));
    match value {
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                let comma = if i + 1 < items.len() { "," } else { "" };
                push_json(item, depth + 1, Vec::new(), comma, out);
            }
        }
        Value::Object(map) => {
            for (i, (key, item)) in map.iter().enumerate() {
                let comma = if i + 1 < map.len() { "," } else { "" };
                let key = vec![
                    Span::from(Value::String(key.clone()).to_string()).fg(colors.base0d),
                    punct(": "),
                ];
                push_json(item, depth + 1, key, comma, out);
            }
        }
        _ => {}
    }
    out.push(line(vec![punct(close), punct(comma)]));
}

/// One JSON scalar (or empty array / object), colored by kind.
fn json_scalar(value: &serde_json::Value) -> Span<'static> {
    use serde_json::Value;

    let colors = &config::get().colors;
    let color = match value {
        Value::String(_) => colors.base0b,
        Value::Number(_) => colors.base09,
        Value::Bool(_) | Value::Null => colors.base0e,
        Value::Array(_) | Value::Object(_) => colors.base05,
    };
    Span::from(value.to_string()).fg(color)
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
    /// Confirm copying a large value (#27): fetching it whole may be slow.
    ConfirmYank {
        key: String,
        r_type: RedisType,
        size: u128,
    },
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
/// from the loaded `selected_value` page so it matches what the user sees. A
/// LIST `index` is absolute (page start + row), as `LSET` expects.
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
    /// Short non-modal message (e.g. "Copied …") and when it expires
    /// (`None` = until replaced).
    toast: Option<(String, Option<Instant>)>,
}

/// How long a toast stays up.
const TOAST_TIMEOUT: Duration = Duration::from_millis(2500);

/// Banner shown above the key table in cluster mode.
const CLUSTER_BANNER: &str = "⚠ cluster: SCAN sees only this node";

impl KeySpace {
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
            toast: None,
        }
    }

    /// Show a toast for [`TOAST_TIMEOUT`], or until replaced if `sticky`.
    pub fn show_toast(&mut self, msg: impl Into<String>, sticky: bool) {
        let expires = (!sticky).then(|| Instant::now() + TOAST_TIMEOUT);
        self.toast = Some((msg.into(), expires));
    }

    /// Drop the toast once it has expired.
    pub fn expire_toast(&mut self, now: Instant) {
        if self
            .toast
            .as_ref()
            .is_some_and(|(_, expires)| expires.is_some_and(|at| now >= at))
        {
            self.toast = None;
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
            KeyValue::Unknown => false,
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

    pub fn is_popup(&self) -> bool {
        matches!(self.mode, KeySpaceMode::Popup(_))
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if let Some(ref mut text_area) = self.text_area {
            if text_area.input(key) {
                if let Some(preview) = self.filter_preview.as_mut() {
                    preview.edited_at = Instant::now();
                }
            }
        }
    }

    pub fn enter_filter_pattern(&mut self) {
        self.mode = KeySpaceMode::Popup(KeySpacePopupMode::FilterPattern);
        self.text_area = Some(build_input_text_area("Pattern", "Enter pattern", None));
        self.filter_preview = Some(FilterPreview::new(Instant::now()));
    }

    /// Current text of the filter input (empty when the popup is closed).
    fn filter_text(&self) -> String {
        self.text_area.as_ref().map(input_text).unwrap_or_default()
    }

    /// The pattern whose matches should be counted now, if any (debounced).
    pub fn filter_count_request(&mut self, now: Instant) -> Option<String> {
        if !self.is_popup() {
            return None;
        }
        let text = self.filter_text();
        self.filter_preview.as_mut()?.request(&text, now)
    }

    /// Store a match count from the runner. Ignored once the popup is closed.
    pub fn set_filter_count(&mut self, pattern: String, count: usize, complete: bool) {
        if let Some(preview) = self.filter_preview.as_mut() {
            preview.result = Some((pattern, count, complete));
        }
    }

    /// Close the filter popup and return the entered pattern. An empty input
    /// means "no filter" and yields `None`, so callers never see `Some("")`.
    pub fn confirm_filter_pattern(&mut self) -> Option<String> {
        let pattern = self
            .text_area
            .take()
            .map(|text_area| text_area.lines()[0].clone())
            .filter(|line| !line.is_empty());

        self.exit_popup();
        pattern
    }

    pub fn exit_popup(&mut self) {
        self.mode = KeySpaceMode::Normal;
        self.filter_preview = None;
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

    pub fn set_keys(&mut self, keys: Vec<KeyMeta>) {
        _ = std::mem::replace(&mut self.keys, keys);
        self.loaded = true;
    }

    pub fn scroll_next(&mut self) -> ScrollOutcome {
        match self.focus {
            Focus::Keys => {
                let wrap_index = self.keys.len().max(1);
                let next = self.table.selected().map_or(0, |i| (i + 1) % wrap_index);
                self.scroll_to(next);
                ScrollOutcome::Moved
            }
            Focus::Value => self.scroll_value(true),
            Focus::Text => {
                self.scroll_text(true, 1);
                ScrollOutcome::Moved
            }
        }
    }

    pub fn scroll_previous(&mut self) -> ScrollOutcome {
        match self.focus {
            Focus::Keys => {
                let last: usize = self.keys.len().saturating_sub(1);
                let wrap_index = self.keys.len().max(1);
                let previous = self
                    .table
                    .selected()
                    .map_or(last, |i: usize| (i + last) % wrap_index);
                self.scroll_to(previous);
                ScrollOutcome::Moved
            }
            Focus::Value => self.scroll_value(false),
            Focus::Text => {
                self.scroll_text(false, 1);
                ScrollOutcome::Moved
            }
        }
    }

    fn scroll_to(&mut self, index: usize) {
        if self.keys.is_empty() {
            self.table.select(None);
        } else {
            self.table.select(Some(index));
        }
    }

    /// Move the detail-view selection within the loaded value page.
    ///
    /// A single-page value wraps at the ends, as before. A paged value instead
    /// reports the edge (`PastEnd` / `PastStart`) when another page exists that
    /// way, so the caller can turn the page; on the very first / last row it
    /// stays put.
    fn scroll_value(&mut self, forward: bool) -> ScrollOutcome {
        let len = self.value_row_count();
        if len == 0 {
            self.value_table.select(None);
            return ScrollOutcome::Moved;
        }
        let page = self
            .selected_value
            .as_ref()
            .map(|loaded| loaded.page)
            .unwrap_or_default();
        let next = match (self.value_table.selected(), forward) {
            (Some(i), true) if i + 1 >= len && page.is_paged() => {
                return if page.has_next {
                    ScrollOutcome::PastEnd
                } else {
                    ScrollOutcome::Moved
                };
            }
            (Some(0), false) if page.is_paged() => {
                return if page.has_prev {
                    ScrollOutcome::PastStart
                } else {
                    ScrollOutcome::Moved
                };
            }
            (Some(i), true) => (i + 1) % len,
            (Some(i), false) => (i + len - 1) % len,
            (None, true) => 0,
            (None, false) => len - 1,
        };
        self.value_table.select(Some(next));
        ScrollOutcome::Moved
    }

    /// Number of rows the loaded value renders as a table. Scalar/absent values
    /// have no navigable rows.
    fn value_row_count(&self) -> usize {
        match self.selected_value.as_ref().map(|loaded| &loaded.value) {
            Some(KeyValue::List(items) | KeyValue::Set(items)) => items.len(),
            Some(KeyValue::Hash(entries)) => entries.len(),
            Some(KeyValue::Zset(entries)) => entries.len(),
            Some(KeyValue::String { .. } | KeyValue::Json(_) | KeyValue::Unknown) | None => 0,
        }
    }

    /// True when the loaded value is a collection that can be navigated row by
    /// row (the precondition for [`Self::enter_value`]).
    fn value_is_navigable(&self) -> bool {
        self.value_row_count() > 0
    }

    /// Drill focus into the value table so `j`/`k` scroll its rows, or into a
    /// STRING / JSON value so they scroll its lines. Empty collections and
    /// unsupported types are ignored.
    pub fn enter_value(&mut self) {
        if self.text_cache.is_some() {
            self.focus = Focus::Text;
            return;
        }
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

    /// Focus is on a STRING / JSON value (`j`/`k` scroll its lines).
    pub fn is_text_focused(&self) -> bool {
        self.focus == Focus::Text
    }

    /// Scroll the focused text value `step` rows down (or up).
    pub fn scroll_text(&mut self, down: bool, step: usize) {
        let view = &mut self.text_view;
        view.top = if down {
            (view.top + step).min(view.max_top)
        } else {
            view.top.saturating_sub(step)
        };
    }

    /// Scroll the focused text value by one screen.
    pub fn scroll_text_page(&mut self, down: bool) {
        let step = self.text_view.height.saturating_sub(1).max(1);
        self.scroll_text(down, step);
    }

    /// Pan the focused text value sideways. Only with wrap off.
    pub fn pan_text(&mut self, right: bool) {
        let view = &mut self.text_view;
        if view.wrap || !self.focus.eq(&Focus::Text) {
            return;
        }
        view.left = if right {
            (view.left + TEXT_PAN_STEP).min(view.max_left)
        } else {
            view.left.saturating_sub(TEXT_PAN_STEP)
        };
    }

    /// Turn line wrapping of text values on / off.
    pub fn toggle_wrap(&mut self) {
        self.text_view.wrap = !self.text_view.wrap;
        self.text_view.top = 0;
        self.text_view.left = 0;
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
    fn selected_element(&self) -> Option<SelectedElement> {
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
            KeyValue::String { .. } | KeyValue::Json(_) | KeyValue::Unknown => None,
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

    /// Open the value editor. STRING values pre-fill the editor; a truncated
    /// STRING or any other type opens a read-only notice instead, so a cut-off
    /// preview is never written back.
    pub fn open_edit_value(&mut self) {
        match self.selected_value.as_ref().map(|loaded| &loaded.value) {
            Some(value) if value.is_truncated() => {
                self.action_overlay = Some(ActionOverlay {
                    kind: OverlayKind::Notice("Value too large to edit here.".to_string()),
                    text_area: None,
                });
            }
            Some(KeyValue::String { value: s, .. }) => {
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

    /// Show a dismissable one-line message.
    pub fn show_notice(&mut self, msg: impl Into<String>) {
        self.action_overlay = Some(ActionOverlay {
            kind: OverlayKind::Notice(msg.into()),
            text_area: None,
        });
    }

    /// Drop everything shown for the old DB, so nothing stale can be edited
    /// while the new DB loads.
    pub fn clear_for_db_switch(&mut self) {
        self.close_overlay();
        self.clear_selected_value();
        self.set_keys(Vec::new());
        self.refresh();
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
            OverlayKind::ConfirmYank { key, r_type, .. } => {
                Some(OverlayRequest::Yank { key, r_type })
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
            RedisType::String | RedisType::Json | RedisType::Stream | RedisType::Unknown(_) => {
                return
            }
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
            RedisType::String | RedisType::Json | RedisType::Stream | RedisType::Unknown(_) => {
                return None
            }
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
    /// Copy the whole value of `key` (a read, allowed in read-only mode).
    Yank {
        key: String,
        r_type: RedisType,
    },
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
fn add_item_pair_titles(kind: &RedisType) -> (&'static str, &'static str) {
    match kind {
        RedisType::Hash => ("Field", "Value"),
        RedisType::Zset => ("Member", "Score"),
        RedisType::Set => ("Member", ""),
        RedisType::List
        | RedisType::String
        | RedisType::Json
        | RedisType::Stream
        | RedisType::Unknown(_) => ("Item", ""),
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

        // Live match count on the popup's bottom border.
        if let Some(preview) = state.filter_preview.as_ref() {
            if popup_area.height >= 3 && popup_area.width > 4 {
                let status = Rect {
                    x: popup_area.x + 2,
                    y: popup_area.bottom() - 1,
                    width: popup_area.width - 4,
                    height: 1,
                };
                Paragraph::new(
                    Line::from(format!(" {} ", preview.status(&state.filter_text())))
                        .fg(config::get().colors.base0b),
                )
                .alignment(Alignment::Right)
                .render(status, buf);
            }
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
        let yank_body;
        let (title, body, hint): (&str, &str, &str) = match &overlay.kind {
            OverlayKind::ConfirmYank { key, size, .. } => {
                yank_body = format!(
                    "{key} is ~{} (estimate). Copy it anyway?",
                    format_bytes_u128(*size)
                );
                (
                    " Copy large value ",
                    yank_body.as_str(),
                    "Enter: copy   Esc: cancel",
                )
            }
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

    /// A one-line boxed message at the bottom-right of `area`.
    fn render_toast(msg: &str, area: Rect, buf: &mut Buffer) {
        let cfg = config::get();
        let text_width = u16::try_from(Line::from(msg).width()).unwrap_or(u16::MAX);
        let width = text_width
            .saturating_add(4)
            .min(area.width.saturating_sub(2));
        if width < 5 || area.height < 5 {
            return;
        }
        let toast = Rect {
            x: area.right() - width - 1,
            y: area.bottom() - 4,
            width,
            height: 3,
        };
        Clear.render(toast, buf);
        Paragraph::new(Line::from(msg).fg(cfg.colors.base05))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(cfg.colors.base0b)
                    .bg(cfg.colors.base00)
                    .padding(ratatui::widgets::Padding::horizontal(1)),
            )
            .render(toast, buf);
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
            let (title_a, title_b) = add_item_pair_titles(&draft.kind);
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

    /// Placeholder for an empty key table (#40): distinguishes "still loading",
    /// "pattern matched nothing" and "empty database", with the keys to recover.
    fn render_empty_keys(state: &KeySpace, area: Rect, buf: &mut Buffer) {
        let lines = if !state.loaded || state.loading {
            vec![Line::from("Loading keys…")]
        } else if let Some(pattern) = state.page.pattern.as_deref() {
            vec![
                Line::from(format!("No keys match `{pattern}`")).bold(),
                Line::from("l clear filter · f change filter"),
            ]
        } else {
            vec![
                Line::from("No keys in this database").bold(),
                Line::from("a add key · r refresh"),
            ]
        };
        // No "next page" hint: paging slices one deduplicated SCAN sweep, so a
        // page is only empty once the sweep has ended with no keys at all.
        Self::render_placeholder(lines, area, buf);
    }

    /// Centered dim placeholder line(s), as used by the empty states.
    fn render_placeholder(lines: Vec<Line<'_>>, area: Rect, buf: &mut Buffer) {
        let height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        let [centered] = Layout::vertical([Constraint::Length(height)])
            .flex(ratatui::layout::Flex::Center)
            .areas(area);
        Paragraph::new(lines)
            .fg(config::get().colors.base03)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .render(centered, buf);
    }

    fn render_key_view(state: &mut KeySpace, area: Rect, buf: &mut Buffer) {
        let cfg = config::get();
        // Inside the Values pane's border.
        let inner = Block::new().borders(Borders::ALL).inner(area);
        let selected = state
            .table
            .selected()
            .and_then(|index| state.keys.get(index));
        let Some(key) = selected else {
            Self::render_placeholder(vec![Line::from("Select a key · Enter to open")], inner, buf);
            return;
        };

        let [key_meta_area, rule_area, view_area] = Layout::vertical([
            Constraint::Length(4),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .horizontal_margin(1)
        .areas(inner);

        let field = |label: &'static str, value: Span<'static>| {
            Line::from(vec![
                Span::from(format!("{label:<5} ")).fg(cfg.colors.base03),
                value,
            ])
        };
        let badge: ratatui::text::Text = (&key.r_type).into();
        let badge = badge
            .lines
            .into_iter()
            .next()
            .and_then(|line| line.spans.into_iter().next())
            .unwrap_or_default();
        // The detail pane has room for the full name (badges cut long ones).
        let badge = Span::styled(key.r_type.name().to_owned(), badge.style);
        let size = Byte::from_u128(key.size)
            .unwrap_or_default()
            .get_appropriate_unit(UnitType::Binary);
        let key_info = vec![
            field(
                "Key",
                Span::from(key.key.clone()).fg(cfg.colors.base05).bold(),
            ),
            field("Type", badge),
            field(
                "TTL",
                Span::from(format_ttl_detail(key.ttl)).fg(cfg.colors.base05),
            ),
            field(
                "Size",
                Span::from(format!("{size:.2}")).fg(cfg.colors.base05),
            ),
        ];

        Paragraph::new(key_info)
            .wrap(Wrap { trim: true })
            .render(key_meta_area, buf);
        render_rule(rule_area, buf);

        let Some(loaded) = state.selected_value.as_ref() else {
            Self::render_placeholder(vec![Line::from("Loading…")], view_area, buf);
            return;
        };

        if let Some(cache) = state.text_cache.as_ref() {
            let footer = value_footer(loaded, state.value_loading);
            Self::render_text_value(
                cache,
                footer,
                state.focus == Focus::Text,
                view_area,
                buf,
                &mut state.text_view,
            );
            return;
        }

        let view_area = match value_footer(loaded, state.value_loading) {
            Some(footer) => {
                let [value_area, footer_area] =
                    Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(view_area);
                Paragraph::new(Line::from(footer).fg(cfg.colors.base03))
                    .alignment(Alignment::Right)
                    .render(footer_area, buf);
                value_area
            }
            None => view_area,
        };

        Self::render_value(&loaded.value, view_area, buf, &mut state.value_table);
    }

    /// A STRING / JSON value: a header, the scrollable text, and a footer
    /// with the shown line range, truncation note (`note`) and key hints.
    fn render_text_value(
        cache: &TextCache,
        note: Option<String>,
        focused: bool,
        area: Rect,
        buf: &mut Buffer,
        view: &mut TextView,
    ) {
        let cfg = config::get();
        let [header_area, body_area, footer_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(area);

        let header = if cache.json { "Value · JSON" } else { "Value" };
        Paragraph::new(Line::from(header).fg(cfg.colors.base03).bold()).render(header_area, buf);

        let height = usize::from(body_area.height);
        let width = usize::from(body_area.width);
        let paragraph = if view.wrap {
            Paragraph::new(cache.lines.clone()).wrap(Wrap { trim: false })
        } else {
            Paragraph::new(cache.lines.clone())
        };
        let total = if view.wrap {
            paragraph.line_count(body_area.width)
        } else {
            cache.lines.len()
        };
        view.height = height;
        view.max_top = total.saturating_sub(height);
        view.max_left = if view.wrap {
            0
        } else {
            cache
                .lines
                .iter()
                .map(Line::width)
                .max()
                .unwrap_or(0)
                .saturating_sub(width)
        };
        view.top = view.top.min(view.max_top);
        view.left = view.left.min(view.max_left);

        let to_u16 = |n: usize| u16::try_from(n).unwrap_or(u16::MAX);
        if view.wrap {
            paragraph
                .scroll((to_u16(view.top), 0))
                .render(body_area, buf);
        } else {
            // Slice the rows first: `scroll` takes a `u16` offset.
            let rows: Vec<Line<'static>> = cache
                .lines
                .iter()
                .skip(view.top)
                .take(height)
                .cloned()
                .collect();
            Paragraph::new(rows)
                .scroll((0, to_u16(view.left)))
                .render(body_area, buf);
        }

        let footer = text_footer(note, view, total, focused);
        Paragraph::new(Line::from(footer).fg(cfg.colors.base03))
            .alignment(Alignment::Right)
            .render(footer_area, buf);
    }

    fn render_value(value: &KeyValue, area: Rect, buf: &mut Buffer, table_state: &mut TableState) {
        match value {
            KeyValue::List(items) => {
                Self::render_single_column_table("Item", items, area, buf, table_state);
            }
            KeyValue::Set(members) => {
                Self::render_single_column_table("Member", members, area, buf, table_state);
            }
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
            // STRING / JSON go through `render_text_value`.
            KeyValue::String { .. } | KeyValue::Json(_) | KeyValue::Unknown => {
                Self::render_placeholder(
                    vec![Line::from("Viewer for this type isn't supported yet")],
                    area,
                    buf,
                );
            }
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
        let header = Row::new([Cell::from(header_label)])
            .bottom_margin(1)
            .fg(cfg.colors.base03)
            .bold();

        let rows = rows_data
            .iter()
            .map(|item| Row::new([Cell::from(item.as_str())]).fg(cfg.colors.base05));
        let table = Table::new(rows, widths)
            .header(header)
            .highlight_symbol(highlight_symbol())
            .highlight_style(selected_row_style())
            .highlight_spacing(HighlightSpacing::Always);

        StatefulWidget::render(table, area, buf, table_state);
        render_header_rule(area, buf);
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
        let header = Row::new([Cell::from(left_label), Cell::from(right_label)])
            .bottom_margin(1)
            .fg(cfg.colors.base03)
            .bold();

        let rows = rows_data.iter().map(|(left, right)| {
            Row::new([
                Cell::from(left.as_str()).fg(cfg.colors.base05),
                Cell::from(right.as_str()).fg(cfg.colors.base04),
            ])
        });
        let table = Table::new(rows, widths)
            .header(header)
            .column_spacing(2)
            .highlight_symbol(highlight_symbol())
            .highlight_style(selected_row_style())
            .highlight_spacing(HighlightSpacing::Always);

        StatefulWidget::render(table, area, buf, table_state);
        render_header_rule(area, buf);
    }
}

/// One-line paging / truncation summary shown under the value, e.g.
/// `Items 101–200 of 12,345 · n/p page`. `None` when there's nothing to say
/// (an untruncated STRING, an empty collection, an unsupported type).
fn value_footer(loaded: &LoadedValue, loading: bool) -> Option<String> {
    let noun = match &loaded.value {
        KeyValue::String { value, total_len } => {
            if !loaded.value.is_truncated() {
                return None;
            }
            let shown = format_bytes(value.len().min(STRING_PREVIEW_BYTES));
            let total = format_bytes(*total_len);
            return Some(format!("Showing first {shown} of {total}"));
        }
        KeyValue::List(_) => "Items",
        KeyValue::Hash(_) => "Fields",
        KeyValue::Set(_) | KeyValue::Zset(_) => "Members",
        KeyValue::Json(_) | KeyValue::Unknown => return None,
    };
    let page = loaded.page;
    if page.len == 0 {
        return None;
    }
    let mut footer = format!(
        "{noun} {}–{}",
        group_thousands(page.start + 1),
        group_thousands(page.start + page.len)
    );
    if let Some(total) = page.total {
        footer.push_str(" of ");
        footer.push_str(&group_thousands(total));
    }
    if page.is_paged() {
        footer.push_str(" · n/p page");
    }
    if loading {
        footer.push_str(" · loading…");
    }
    Some(footer)
}

/// Footer under a text value: truncation note, the shown line range, and the
/// keys that work (`Enter to scroll` when there's more than fits).
fn text_footer(note: Option<String>, view: &TextView, total: usize, focused: bool) -> String {
    let mut parts: Vec<String> = note.into_iter().collect();
    if total > view.height {
        let last = (view.top + view.height).min(total);
        parts.push(format!(
            "lines {}–{} of {}",
            group_thousands(view.top + 1),
            group_thousands(last),
            group_thousands(total)
        ));
    }
    if focused {
        let pan = if view.wrap { "" } else { " · ←/→ pan" };
        let wrap = if view.wrap { "w no-wrap" } else { "w wrap" };
        parts.push(format!("j/k scroll · n/p page{pan} · {wrap}"));
    } else if total > view.height || view.max_left > 0 {
        parts.push("Enter to scroll".to_owned());
    }
    parts.join(" · ")
}

/// `1234567` → `1,234,567`.
fn group_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Human byte size, matching the Size field (`64.00 KiB`).
pub fn format_bytes(n: usize) -> String {
    format_bytes_u128(u128::try_from(n).unwrap_or(u128::MAX))
}

fn format_bytes_u128(n: u128) -> String {
    let byte = Byte::from_u128(n)
        .unwrap_or_default()
        .get_appropriate_unit(UnitType::Binary);
    format!("{byte:.2}")
}

/// Width of the type column: fits the longest label (`json`, `hash`, …).
#[allow(clippy::cast_possible_truncation)]
const TYPE_COLUMN_WIDTH: u16 = TYPE_BADGE_WIDTH as u16;

/// Selection marker shared by the key and value tables.
fn highlight_symbol() -> Span<'static> {
    Span::from("▌ ").fg(config::get().colors.base0d)
}

fn selected_row_style() -> Style {
    Style::new().bg(config::get().colors.base02).bold()
}

/// Dim horizontal rule across `area`'s first row.
fn render_rule(area: Rect, buf: &mut Buffer) {
    Paragraph::new("─".repeat(usize::from(area.width)))
        .fg(config::get().colors.base02)
        .render(area, buf);
}

/// Draws the rule into the blank row a table header leaves via
/// `bottom_margin(1)`, underlining the header.
fn render_header_rule(table_area: Rect, buf: &mut Buffer) {
    if table_area.height < 2 {
        return;
    }
    render_rule(
        Rect {
            y: table_area.y + 1,
            height: 1,
            ..table_area
        },
        buf,
    );
}

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
        let value_focused = state.focus != Focus::Keys;
        let focused = Style::new().fg(cfg.colors.base0d);
        let unfocused = Style::new().fg(cfg.colors.base02);
        let (keys_border, values_border) = if value_focused {
            (unfocused, focused)
        } else {
            (focused, unfocused)
        };
        let pane_title = |text: &'static str, has_focus: bool| {
            if has_focus {
                Line::from(text).fg(cfg.colors.base05).bold()
            } else {
                Line::from(text).fg(cfg.colors.base03)
            }
        };

        let page_summary = if state.loaded {
            state.page.summary()
        } else {
            format!("Page {} · loading…", state.page.page + 1)
        };
        let space_block = Block::new()
            .bg(cfg.colors.base00)
            .fg(cfg.colors.base04)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(keys_border)
            .borders(Borders::all())
            .title(pane_title(
                if state.loading {
                    " Keys · loading… "
                } else {
                    " Keys "
                },
                !value_focused,
            ))
            // Paging status lives on the bottom border.
            .title_bottom(
                Line::from(format!(" {page_summary} "))
                    .fg(cfg.colors.base05)
                    .left_aligned(),
            )
            // Raw SCAN cursor: secondary/debug detail, kept dim.
            .title_bottom(
                Line::from(format!(
                    " {}/page · cursor {} ",
                    state.page.page_size, state.page.scan_cursor
                ))
                .fg(cfg.colors.base03)
                .right_aligned(),
            );

        let table_area = space_block.inner(t_area);
        space_block.render(t_area, buf);

        Block::new()
            .bg(cfg.colors.base00)
            .fg(cfg.colors.base04)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(values_border)
            .borders(Borders::all())
            .title(pane_title(
                if value_focused {
                    " Values · focused "
                } else {
                    " Values "
                },
                value_focused,
            ))
            .render(view_area, buf);

        let [filter_area, banner_area, rule_area, table_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(u16::from(state.cluster_mode)),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .horizontal_margin(1)
        .areas(table_area);

        let pattern = match state.page.pattern.as_deref() {
            Some(p) => Span::from(p).fg(cfg.colors.base0d).bold(),
            None => Span::from("*").fg(cfg.colors.base03),
        };
        Paragraph::new(Line::from(vec![
            Span::from("filter ").fg(cfg.colors.base03),
            pattern,
        ]))
        .render(filter_area, buf);
        if state.cluster_mode {
            Paragraph::new(Line::from(CLUSTER_BANNER).fg(cfg.colors.base0b).bold())
                .render(banner_area, buf);
        }
        render_rule(rule_area, buf);
        // Header + the rule row under it take two lines.
        state.visible_rows = usize::from(table_area.height.saturating_sub(2)).max(1);

        let widths = [
            Constraint::Length(TYPE_COLUMN_WIDTH),
            Constraint::Fill(1),
            Constraint::Length(7),
            Constraint::Length(10),
        ];
        let header: Row<'_> = Row::new([
            Cell::from("Type"),
            Cell::from("Key"),
            Cell::from(Line::from("TTL").right_aligned()),
            Cell::from(Line::from("Size").right_aligned()),
        ])
        .bottom_margin(1)
        .fg(cfg.colors.base03)
        .bold();

        let rows = state.keys.iter().map(|meta| {
            let badge: ratatui::text::Text = (&meta.r_type).into();
            // Keys with a real expiry stand out; `∞` / `—` stay dim.
            let ttl_color = if meta.ttl >= 0 {
                cfg.colors.base0b
            } else {
                cfg.colors.base03
            };
            Row::new([
                Cell::from(badge),
                Cell::from(meta.key.as_str()).fg(cfg.colors.base05),
                Cell::from(Line::from(format_ttl(meta.ttl)).right_aligned()).fg(ttl_color),
                Cell::from(
                    Line::from(format!(
                        "{:.2}",
                        Byte::from_u128(meta.size)
                            .unwrap_or_default()
                            .get_appropriate_unit(UnitType::Binary)
                    ))
                    .right_aligned(),
                )
                .fg(cfg.colors.base03),
            ])
        });
        let table: Table<'_> = Table::new(rows, widths)
            .header(header)
            .column_spacing(1)
            .highlight_symbol(highlight_symbol())
            .highlight_style(selected_row_style())
            .highlight_spacing(HighlightSpacing::Always);

        if state.keys.is_empty() {
            Self::render_empty_keys(state, table_area, buf);
        } else {
            StatefulWidget::render(table, table_area, buf, &mut state.table);
            render_header_rule(table_area, buf);
        }

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

        if let Some((msg, _)) = state.toast.as_ref() {
            Self::render_toast(msg, area, buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_input_text_area, format_ttl, format_ttl_detail, group_thousands, value_footer,
        KeySpace, PageInfo, ScrollOutcome, SelectedElement,
    };
    use crate::redis_client::types::{KeyMeta, KeyValue, LoadedValue, RedisType, ValuePageInfo};
    use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};

    fn render_text(keyspace: &mut KeySpace) -> String {
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

    fn list_page(rows: usize, start: usize, has_prev: bool, has_next: bool) -> LoadedValue {
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

    fn focused(value: LoadedValue) -> KeySpace {
        let mut keyspace = KeySpace::new(Vec::new());
        keyspace.set_selected_value(Some(value));
        keyspace.enter_value();
        keyspace
    }

    #[test]
    fn single_page_value_wraps() {
        let mut keyspace = focused(list_page(3, 0, false, false));
        assert_eq!(keyspace.scroll_previous(), ScrollOutcome::Moved);
        assert_eq!(keyspace.value_table.selected(), Some(2));
        assert_eq!(keyspace.scroll_next(), ScrollOutcome::Moved);
        assert_eq!(keyspace.value_table.selected(), Some(0));
    }

    #[test]
    fn paged_value_reports_edges() {
        let mut keyspace = focused(list_page(3, 100, true, true));
        assert_eq!(keyspace.scroll_previous(), ScrollOutcome::PastStart);
        assert_eq!(keyspace.value_table.selected(), Some(0));
        keyspace.scroll_next();
        keyspace.scroll_next();
        assert_eq!(keyspace.scroll_next(), ScrollOutcome::PastEnd);
        assert_eq!(keyspace.value_table.selected(), Some(2));
    }

    #[test]
    fn last_page_stays_put_at_end() {
        let mut keyspace = focused(list_page(2, 100, true, false));
        keyspace.scroll_next();
        assert_eq!(keyspace.scroll_next(), ScrollOutcome::Moved);
        assert_eq!(keyspace.value_table.selected(), Some(1));
    }

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
    fn value_footer_text() {
        let page = list_page(100, 100, true, true);
        assert_eq!(
            value_footer(&page, false).as_deref(),
            Some("Items 101–200 of 1,000 · n/p page")
        );
        assert_eq!(
            value_footer(&page, true).as_deref(),
            Some("Items 101–200 of 1,000 · n/p page · loading…")
        );

        let small = LoadedValue {
            value: KeyValue::Set(vec!["a".into()]),
            page: ValuePageInfo {
                len: 1,
                total: Some(1),
                ..ValuePageInfo::default()
            },
        };
        assert_eq!(
            value_footer(&small, false).as_deref(),
            Some("Members 1–1 of 1")
        );

        let full = LoadedValue {
            value: KeyValue::String {
                value: "abc".into(),
                total_len: 3,
            },
            page: ValuePageInfo::default(),
        };
        assert_eq!(value_footer(&full, false), None);

        let cut = LoadedValue {
            value: KeyValue::String {
                value: "x".repeat(1024),
                total_len: 2048,
            },
            page: ValuePageInfo::default(),
        };
        assert_eq!(
            value_footer(&cut, false).as_deref(),
            Some("Showing first 1.00 KiB of 2.00 KiB")
        );
    }

    #[test]
    fn thousands_grouping() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1000), "1,000");
        assert_eq!(group_thousands(1_234_567), "1,234,567");
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

    #[test]
    fn ttl_sentinels() {
        assert_eq!(format_ttl(-1), "∞");
        assert_eq!(format_ttl(-2), "—");
        assert_eq!(format_ttl_detail(-1), "No expiry");
        assert_eq!(format_ttl_detail(-2), "Missing");
    }

    #[test]
    fn ttl_durations() {
        assert_eq!(format_ttl(0), "0s");
        assert_eq!(format_ttl(45), "45s");
        assert_eq!(format_ttl(60), "1m");
        assert_eq!(format_ttl(3600), "1h");
        assert_eq!(format_ttl(5400), "1h 30m");
        assert_eq!(format_ttl(3601), "1h");
        assert_eq!(format_ttl(90_061), "1d 1h");
    }

    #[test]
    fn ttl_detail_includes_raw_seconds() {
        assert_eq!(format_ttl_detail(5400), "1h 30m (5400s)");
        assert_eq!(format_ttl_detail(45), "45s (45s)");
    }

    #[test]
    fn empty_filter_pattern_is_none() {
        let mut keyspace = KeySpace::new(Vec::new());
        keyspace.enter_filter_pattern();
        assert_eq!(keyspace.confirm_filter_pattern(), None);
    }

    #[test]
    fn non_empty_filter_pattern_is_some() {
        let mut keyspace = KeySpace::new(Vec::new());
        keyspace.enter_filter_pattern();
        keyspace.text_area = Some(build_input_text_area(
            "Pattern",
            "Enter pattern",
            Some("foo*"),
        ));
        assert_eq!(keyspace.confirm_filter_pattern(), Some("foo*".to_owned()));
    }

    fn line_text(line: &ratatui::text::Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn string_value(text: &str) -> LoadedValue {
        LoadedValue {
            value: KeyValue::String {
                value: text.into(),
                total_len: text.len(),
            },
            page: ValuePageInfo::default(),
        }
    }

    /// A keyspace with one selected key whose loaded value is `value`.
    fn with_value(r_type: RedisType, value: LoadedValue) -> KeySpace {
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
    fn json_is_pretty_printed_in_order() {
        let value: serde_json::Value =
            serde_json::from_str(r#"{"z":1,"b":[true,null],"c":{},"s":"x\"y"}"#).unwrap();
        let lines: Vec<String> = super::json_lines(&value).iter().map(line_text).collect();
        assert_eq!(
            lines,
            vec![
                "{",
                "  \"z\": 1,",
                "  \"b\": [",
                "    true,",
                "    null",
                "  ],",
                "  \"c\": {},",
                "  \"s\": \"x\\\"y\"",
                "}",
            ]
        );
    }

    #[test]
    fn string_json_detection() {
        assert!(super::parse_json_text("  {\"a\": 1}").is_some());
        assert!(super::parse_json_text("[1, 2]").is_some());
        assert!(
            super::parse_json_text("42").is_none(),
            "only objects / arrays"
        );
        assert!(super::parse_json_text("{not json").is_none());

        let json = super::TextCache::build(&string_value("{\"a\":1}").value).unwrap();
        assert!(json.json);
        assert_eq!(json.lines.len(), 3);

        let truncated = KeyValue::String {
            value: "{\"a\":1}".into(),
            total_len: 100,
        };
        assert!(!super::TextCache::build(&truncated).unwrap().json);

        let plain = super::TextCache::build(&string_value("a\nb").value).unwrap();
        assert!(!plain.json);
        assert_eq!(plain.lines.len(), 2);
        assert!(super::TextCache::build(&KeyValue::List(Vec::new())).is_none());
    }

    #[test]
    fn text_value_scrolls_and_clamps() {
        let text = (0..100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut keyspace = with_value(RedisType::String, string_value(&text));
        keyspace.enter_value();
        assert!(keyspace.is_text_focused());
        assert!(!keyspace.is_value_focused());

        let screen = render_text(&mut keyspace);
        assert!(screen.contains("line 0"));
        assert!(screen.contains("of 100"));
        assert!(screen.contains("j/k scroll"));

        keyspace.scroll_next();
        keyspace.scroll_text_page(true);
        assert!(keyspace.text_view.top > 1);
        for _ in 0..50 {
            keyspace.scroll_text_page(true);
        }
        assert_eq!(keyspace.text_view.top, keyspace.text_view.max_top);
        let screen = render_text(&mut keyspace);
        assert!(screen.contains("line 99"));

        keyspace.scroll_text(false, usize::MAX);
        assert_eq!(keyspace.text_view.top, 0);
    }

    #[test]
    fn text_value_pans_only_without_wrap() {
        let long = "x".repeat(500);
        let mut keyspace = with_value(RedisType::String, string_value(&long));
        keyspace.enter_value();
        render_text(&mut keyspace);
        keyspace.pan_text(true);
        assert_eq!(keyspace.text_view.left, 0, "wrap on: no panning");

        keyspace.toggle_wrap();
        render_text(&mut keyspace);
        keyspace.pan_text(true);
        assert_eq!(keyspace.text_view.left, super::TEXT_PAN_STEP);
        for _ in 0..100 {
            keyspace.pan_text(true);
        }
        assert_eq!(keyspace.text_view.left, keyspace.text_view.max_left);
        keyspace.pan_text(false);
        assert!(keyspace.text_view.left < keyspace.text_view.max_left);
    }

    #[test]
    fn redis_json_value_renders_pretty() {
        let doc = serde_json::json!({ "name": "Ann", "tags": ["a"] });
        let loaded = LoadedValue {
            value: KeyValue::Json(doc),
            page: ValuePageInfo::default(),
        };
        let mut keyspace = with_value(RedisType::Json, loaded);
        let screen = render_text(&mut keyspace);
        assert!(screen.contains("Value · JSON"));
        assert!(screen.contains("\"name\": \"Ann\","));
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

    #[test]
    fn toast_expires() {
        let mut keyspace = KeySpace::new(Vec::new());
        keyspace.show_toast("Copied", false);
        assert!(render_text(&mut keyspace).contains("Copied"));
        keyspace.expire_toast(std::time::Instant::now());
        assert!(keyspace.toast.is_some());
        keyspace.expire_toast(std::time::Instant::now() + super::TOAST_TIMEOUT);
        assert!(keyspace.toast.is_none());

        keyspace.show_toast("Copying…", true);
        keyspace.expire_toast(std::time::Instant::now() + super::TOAST_TIMEOUT * 10);
        assert!(keyspace.toast.is_some(), "sticky until replaced");
    }

    #[test]
    fn filter_count_is_debounced() {
        use std::time::{Duration, Instant};

        let start = Instant::now();
        let mut preview = super::FilterPreview::new(start);
        assert_eq!(
            preview.request("", start).as_deref(),
            Some(""),
            "first one right away"
        );
        assert_eq!(preview.request("", start), None, "same text: no repeat");

        preview.edited_at = start;
        assert_eq!(
            preview.request("us", start + Duration::from_millis(100)),
            None
        );
        assert_eq!(
            preview
                .request("us", start + super::FILTER_COUNT_DEBOUNCE)
                .as_deref(),
            Some("us")
        );
    }

    #[test]
    fn filter_count_status() {
        let mut preview = super::FilterPreview::new(std::time::Instant::now());
        assert_eq!(preview.status("user:*"), "counting…");
        preview.result = Some(("user:*".into(), 1234, true));
        assert_eq!(preview.status("user:*"), "1,234 matches");
        assert_eq!(
            preview.status("user:1*"),
            "counting…",
            "stale result hidden"
        );
        preview.result = Some(("a".into(), 1, true));
        assert_eq!(preview.status("a"), "1 match");
        preview.result = Some(("a".into(), 5000, false));
        assert_eq!(preview.status("a"), "≥ 5,000 matches (partial scan)");
    }

    #[test]
    fn filter_popup_shows_count() {
        let mut keyspace = KeySpace::new(Vec::new());
        keyspace.enter_filter_pattern();
        assert!(render_text(&mut keyspace).contains("counting…"));
        keyspace.set_filter_count(String::new(), 42, true);
        assert!(render_text(&mut keyspace).contains("42 matches"));
        keyspace.exit_popup();
        assert!(keyspace
            .filter_count_request(std::time::Instant::now())
            .is_none());
    }

    #[test]
    fn visible_rows_follow_table_height() {
        let mut keyspace = KeySpace::new(Vec::new());
        assert_eq!(keyspace.visible_rows(), 0);
        render_text(&mut keyspace);
        // 20 rows - 2 borders - filter - rule - header - header rule.
        assert_eq!(keyspace.visible_rows(), 14);
        keyspace.cluster_mode = true;
        render_text(&mut keyspace);
        assert_eq!(keyspace.visible_rows(), 13);
    }
}
