use super::{
    build_input_text_area, input_text, ActionOverlay, ActiveField, AddItemDraft, EditItemDraft,
    FilterPreview, Focus, KeySpace, KeySpaceMode, KeySpacePopupMode, OverlayKind, ScrollOutcome,
    SelectedElement,
};

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};

use crate::redis_client::types::{ItemDelete, ItemEdit, KeyItem, KeyValue, RedisType};

impl KeySpace {
    pub fn is_popup(&self) -> bool {
        matches!(self.mode, KeySpaceMode::Popup(_))
    }

    /// A key typed into the filter popup at `now` (the live count waits
    /// for the text to settle).
    pub fn handle_key(&mut self, key: KeyEvent, now: Instant) {
        if let Some(ref mut text_area) = self.text_area {
            if text_area.input(key) {
                if let Some(preview) = self.filter_preview.as_mut() {
                    preview.edited_at = now;
                }
            }
        }
    }

    pub fn enter_filter_pattern(&mut self, now: Instant) {
        self.mode = KeySpaceMode::Popup(KeySpacePopupMode::FilterPattern);
        self.text_area = Some(build_input_text_area("Pattern", "Enter pattern", None));
        self.filter_preview = Some(FilterPreview::new(now));
    }

    /// Current text of the filter input (empty when the popup is closed).
    pub(super) fn filter_text(&self) -> String {
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

    /// Focus is on the key list.
    pub fn is_keys_focused(&self) -> bool {
        self.focus == Focus::Keys
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
        self.text_view.scroll(down, step);
    }

    /// Scroll the focused text value by one screen.
    pub fn scroll_text_page(&mut self, down: bool) {
        self.text_view.scroll_page(down);
    }

    /// Pan the focused text value sideways. Only with wrap off.
    pub fn pan_text(&mut self, right: bool) {
        if self.focus == Focus::Text {
            self.text_view.pan(right);
        }
    }

    /// Turn line wrapping of text values on / off.
    pub fn toggle_wrap(&mut self) {
        self.text_view.toggle_wrap();
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

    /// The deletion of the selected collection element, if one is focused.
    pub fn selected_item_delete(&self) -> Option<ItemDelete> {
        self.selected_element().map(SelectedElement::into_delete)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redis_client::types::RedisType;

    use crate::widgets::keyspace::tests::{
        focused, list_page, render_text, string_value, with_value,
    };

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
    fn empty_filter_pattern_is_none() {
        let mut keyspace = KeySpace::new(Vec::new());
        keyspace.enter_filter_pattern(Instant::now());
        assert_eq!(keyspace.confirm_filter_pattern(), None);
    }

    #[test]
    fn non_empty_filter_pattern_is_some() {
        let mut keyspace = KeySpace::new(Vec::new());
        keyspace.enter_filter_pattern(Instant::now());
        keyspace.text_area = Some(build_input_text_area(
            "Pattern",
            "Enter pattern",
            Some("foo*"),
        ));
        assert_eq!(keyspace.confirm_filter_pattern(), Some("foo*".to_owned()));
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
        assert_eq!(
            keyspace.text_view.left,
            crate::widgets::text_view::TEXT_PAN_STEP
        );
        for _ in 0..100 {
            keyspace.pan_text(true);
        }
        assert_eq!(keyspace.text_view.left, keyspace.text_view.max_left);
        keyspace.pan_text(false);
        assert!(keyspace.text_view.left < keyspace.text_view.max_left);
    }

    #[test]
    fn filter_popup_shows_count() {
        let mut keyspace = KeySpace::new(Vec::new());
        keyspace.enter_filter_pattern(Instant::now());
        assert!(render_text(&mut keyspace).contains("counting…"));
        keyspace.set_filter_count(String::new(), 42, true);
        assert!(render_text(&mut keyspace).contains("42 matches"));
        keyspace.exit_popup();
        assert!(keyspace
            .filter_count_request(std::time::Instant::now())
            .is_none());
    }
}
