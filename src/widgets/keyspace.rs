use byte_unit::{Byte, UnitType};
use crossterm::event::KeyEvent;
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
    redis_client::types::{KeyMeta, KeyValue, RedisType},
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
}

/// A confirmed action overlay, ready for `App` to translate into a `RedisEvent`.
pub enum OverlayRequest {
    Delete,
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

        let (title, body, hint): (&str, &str, &str) = match &overlay.kind {
            OverlayKind::ConfirmDelete => (
                " Delete key ",
                "Delete the selected key?",
                "Enter: confirm   Esc: cancel",
            ),
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
    }
}
