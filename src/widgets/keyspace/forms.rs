use super::{KeySpace, SelectedElement};

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::widgets::{Block, BorderType, Borders};
use ratatui_textarea::{CursorMove, TextArea};

use crate::{
    config,
    redis_client::types::{NewKeySpec, RedisType},
    utils::group_thousands,
};

/// How long the filter text must stay unchanged before its matches are counted.
const FILTER_COUNT_DEBOUNCE: Duration = Duration::from_millis(300);

/// Live match count shown under the filter input (#24).
#[derive(Debug, Clone)]
pub(super) struct FilterPreview {
    /// When the filter text last changed.
    pub(super) edited_at: Instant,
    /// The pattern of the last count request, if any.
    requested: Option<String>,
    /// The last count received: pattern, count, sweep finished.
    pub(super) result: Option<(String, usize, bool)>,
}

impl FilterPreview {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            edited_at: now,
            requested: None,
            result: None,
        }
    }

    /// The pattern to count now, if any: the first one right away, later ones
    /// once the text has been idle for [`FILTER_COUNT_DEBOUNCE`].
    pub(super) fn request(&mut self, text: &str, now: Instant) -> Option<String> {
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
    pub(super) fn status(&self, text: &str) -> String {
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

/// The kind of modal action prompt currently overlaid on the keyspace.
pub(super) enum OverlayKind {
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
/// `ratatui-textarea` for the kinds that take input ([`OverlayKind::EditValue`],
/// [`OverlayKind::SetTtl`]).
pub(super) struct ActionOverlay {
    pub(super) kind: OverlayKind,
    pub(super) text_area: Option<TextArea<'static>>,
}

/// The set of key types the add-key wizard can create. JSON is intentionally
/// excluded (the wizard cannot construct `RedisJSON` documents this way).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum NewKind {
    String,
    List,
    Set,
    Hash,
    Zset,
}

impl NewKind {
    /// The kinds in selector order, used both for the radio list and to map the
    /// selection cursor back to a kind.
    pub(super) const ALL: [NewKind; 5] = [
        NewKind::String,
        NewKind::List,
        NewKind::Set,
        NewKind::Hash,
        NewKind::Zset,
    ];

    pub(super) fn label(self) -> &'static str {
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
    pub(super) fn is_pair(self) -> bool {
        matches!(self, NewKind::Hash | NewKind::Zset)
    }

    /// Titles for the two paired inputs (`A`, `B`). Only meaningful for the pair
    /// kinds; other kinds return their single-input title in `A`.
    pub(super) fn pair_titles(self) -> (&'static str, &'static str) {
        match self {
            NewKind::Hash => ("Field", "Value"),
            NewKind::Zset => ("Member", "Score"),
            NewKind::String => ("Value", ""),
            NewKind::List | NewKind::Set => ("Item", ""),
        }
    }
}

/// Ordered steps of the add-key wizard.
pub(super) enum NewKeyStep {
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
pub(super) enum ActiveField {
    A,
    B,
}

/// Draft state accumulated by the add-key wizard across its steps.
pub(super) struct NewKeyDraft {
    pub(super) step: NewKeyStep,
    pub(super) kind: NewKind,
    pub(super) kind_cursor: usize,
    name: String,
    string_value: Option<String>,
    list_items: Vec<String>,
    hash_pairs: Vec<(String, String)>,
    zset_pairs: Vec<(String, f64)>,
    pub(super) input_a: TextArea<'static>,
    pub(super) input_b: Option<TextArea<'static>>,
    pub(super) active: ActiveField,
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
    pub(super) fn item_count(&self) -> usize {
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
pub(super) struct AddItemDraft {
    pub(super) key: String,
    pub(super) kind: RedisType,
    pub(super) input_a: TextArea<'static>,
    pub(super) input_b: Option<TextArea<'static>>,
    pub(super) active: ActiveField,
    pub(super) notice: Option<String>,
}

/// Draft state for the in-collection edit-item overlay (`e` inside a value).
/// Scoped to one resolved element; edits its value/score only (SET replaces the
/// member). The `target` snapshot is captured at open time so confirming builds
/// the right request even if the underlying value later reloads.
pub(super) struct EditItemDraft {
    pub(super) key: String,
    pub(super) target: SelectedElement,
    pub(super) input: TextArea<'static>,
    pub(super) notice: Option<String>,
}

impl KeySpace {
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
}

/// Read the first line of a single-line input text-area.
pub(crate) fn input_text(text_area: &TextArea<'static>) -> String {
    text_area.lines().first().cloned().unwrap_or_default()
}

/// Build a single-line bordered input box matching the keyspace style,
/// optionally seeded with `initial` content.
pub(crate) fn build_input_text_area(
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

/// Titles for the two paired add-item inputs of a collection type. Single-input
/// types return their label in `A` (their `B` is unused).
pub(super) fn add_item_pair_titles(kind: &RedisType) -> (&'static str, &'static str) {
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

#[cfg(test)]
mod tests {

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
}
