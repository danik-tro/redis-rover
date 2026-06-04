use byte_unit::{Byte, UnitType};
use crossterm::event::KeyEvent;
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Layout, Rect},
    style::Stylize,
    widgets::{
        Block, BorderType, Borders, Cell, HighlightSpacing, Paragraph, Row, StatefulWidget, Table,
        TableState, Widget, Wrap,
    },
};
use tui_textarea::TextArea;

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

pub struct KeySpace {
    table: TableState,
    value_table: TableState,
    keys: Vec<KeyMeta>,
    cursor: Option<usize>,
    pattern: Option<String>,
    mode: KeySpaceMode,
    text_area: Option<TextArea<'static>>,
    selected_value: Option<KeyValue>,
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
        }
    }

    pub fn selected_key(&self) -> Option<(String, RedisType)> {
        let idx = self.table.selected()?;
        let meta = self.keys.get(idx)?;
        Some((meta.key.clone(), meta.r_type))
    }

    pub fn set_selected_value(&mut self, value: Option<KeyValue>) {
        self.selected_value = value;
    }

    pub fn clear_selected_value(&mut self) {
        self.selected_value = None;
        // The detail-view selection belongs to the previously selected key;
        // reset it so the new value's table starts unselected.
        self.value_table.select(None);
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
        let cfg = config::get();
        self.mode = KeySpaceMode::Popup(KeySpacePopupMode::FilterPattern);
        let mut text_area = TextArea::default();
        text_area.set_placeholder_text("Enter pattern");
        text_area.set_block(
            Block::default()
                .border_style(cfg.colors.base04)
                .border_type(BorderType::Rounded)
                .borders(Borders::ALL)
                .title("Pattern"),
        );

        self.text_area = Some(text_area);
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
        let wrap_index = self.keys.len().max(1);
        let next = self.table.selected().map_or(0, |i| (i + 1) % wrap_index);
        self.scroll_to(next);
    }

    pub fn scroll_previous(&mut self) {
        let last: usize = self.keys.len().saturating_sub(1);
        let wrap_index = self.keys.len().max(1);
        let previous = self
            .table
            .selected()
            .map_or(last, |i: usize| (i + last) % wrap_index);
        self.scroll_to(previous);
    }

    fn scroll_to(&mut self, index: usize) {
        if self.keys.is_empty() {
            self.table.select(None);
        } else {
            self.table.select(Some(index));
        }
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

        let space_block = Block::new()
            .bg(cfg.colors.base00)
            .fg(cfg.colors.base04)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .borders(Borders::all())
            .title("Keys");

        let table_area = space_block.inner(t_area);
        space_block.render(t_area, buf);

        Block::new()
            .bg(cfg.colors.base00)
            .fg(cfg.colors.base04)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .borders(Borders::all())
            .title("Values")
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
    }
}
