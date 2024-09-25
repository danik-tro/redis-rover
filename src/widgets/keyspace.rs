use byte_unit::{Byte, UnitType};
use crossterm::event::KeyEvent;
use ratatui::{
    layout::{Alignment, Constraint, Layout, Margin},
    style::Stylize,
    widgets::{
        Block, BorderType, Borders, Cell, Clear, HighlightSpacing, Paragraph, Row, StatefulWidget,
        Table, TableState, Widget, Wrap,
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
    keys: Vec<KeyMeta>,
    cursor: Option<usize>,
    pattern: Option<String>,
    mode: KeySpaceMode,
    text_area: Option<TextArea<'static>>,
}

impl KeySpace {
    pub fn new(keys: Vec<KeyMeta>) -> Self {
        Self {
            keys,
            table: TableState::default(),
            cursor: None,
            pattern: None,
            mode: KeySpaceMode::Normal,
            text_area: None,
        }
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
        let mut text_area = TextArea::default();
        text_area.set_placeholder_text("Enter pattern");
        text_area.set_block(
            Block::default()
                .border_style(config::get().colors.base04)
                .border_type(BorderType::Rounded)
                .borders(Borders::ALL)
                .title("Pattern"),
        );

        self.text_area = Some(text_area);
    }

    pub fn confirm_filter_pattern(&mut self) -> Option<String> {
        let pattern = if let Some(text_area) = self.text_area.take() {
            let line = text_area.lines()[0].clone();

            if line == "" {
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
            self.table.select(None)
        } else {
            self.table.select(Some(index));
        }
    }
}

pub struct KeySpaceWidget;

impl KeySpaceWidget {
    fn render_confirm_popup(
        &self,
        state: &mut KeySpace,
        area: ratatui::prelude::Rect,
        buf: &mut ratatui::prelude::Buffer,
    ) {
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

    fn render_key_view(
        &self,
        state: &mut KeySpace,
        area: ratatui::prelude::Rect,
        buf: &mut ratatui::prelude::Buffer,
    ) {
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
            "Key: {}\nType: {:?}\nTTL: {}\nSize: {}",
            key.key,
            key.r_type,
            key.ttl,
            unsafe { Byte::from_u128_unsafe(key.size) }.get_appropriate_unit(UnitType::Binary)
        );

        Paragraph::new(key_info)
            .wrap(Wrap { trim: true })
            .render(key_meta_area, buf);

        match key.value {
            KeyValue::String(ref value) => {
                Paragraph::new(format!("Value: {}", value))
                    .wrap(Wrap { trim: true })
                    .render(view_area, buf);
            }
            KeyValue::List(ref value) => {
                let mut table_state = TableState::default();

                let widths = [Constraint::Percentage(100)];
                let header: Row<'_> = Row::new(["Item"].map(|h| Cell::from(h.bold())))
                    .top_margin(1)
                    .bottom_margin(1)
                    .fg(config::get().colors.base04)
                    .bg(config::get().colors.base02);

                let rows = value.iter().cloned().map(|(value)| {
                    Row::new([Cell::from(value)])
                        .fg(config::get().colors.base04)
                        .bg(config::get().colors.base00)
                });
                let table: Table<'_> = Table::new(rows, widths)
                    .header(header)
                    .flex(ratatui::layout::Flex::Center)
                    .highlight_symbol(HIGHLIGHT_SYMBOL)
                    .highlight_style(config::get().colors.base05)
                    .highlight_spacing(HighlightSpacing::Always);

                StatefulWidget::render(table, view_area, buf, &mut table_state);
            }
            KeyValue::Hash(ref value) => {
                let mut table_state = TableState::default();

                let widths = [Constraint::Percentage(50), Constraint::Percentage(50)];
                let header: Row<'_> = Row::new(["Field", "Value"].map(|h| Cell::from(h.bold())))
                    .top_margin(1)
                    .bottom_margin(1)
                    .fg(config::get().colors.base04)
                    .bg(config::get().colors.base02);

                let rows = value.iter().map(|(field, value)| {
                    Row::new([Cell::from(field.clone()), Cell::from(value.clone())])
                        .fg(config::get().colors.base04)
                        .bg(config::get().colors.base00)
                });
                let table: Table<'_> = Table::new(rows, widths)
                    .header(header)
                    .widths(widths)
                    .flex(ratatui::layout::Flex::Center)
                    .highlight_symbol(HIGHLIGHT_SYMBOL)
                    .highlight_style(config::get().colors.base05)
                    .highlight_spacing(HighlightSpacing::Always);

                StatefulWidget::render(table, view_area, buf, &mut table_state);
            }
            KeyValue::Set(ref value) => {
                let mut table_state = TableState::default();

                let widths = [Constraint::Percentage(100)];
                let header: Row<'_> = Row::new(["Member"].map(|h| Cell::from(h.bold())))
                    .top_margin(1)
                    .bottom_margin(1)
                    .fg(config::get().colors.base04)
                    .bg(config::get().colors.base02);

                let rows = value.iter().cloned().map(|member| {
                    Row::new([Cell::from(member)])
                        .fg(config::get().colors.base04)
                        .bg(config::get().colors.base00)
                });
                let table: Table<'_> = Table::new(rows, widths)
                    .header(header)
                    .flex(ratatui::layout::Flex::Center)
                    .highlight_symbol(HIGHLIGHT_SYMBOL)
                    .highlight_style(config::get().colors.base05)
                    .highlight_spacing(HighlightSpacing::Always);

                StatefulWidget::render(table, view_area, buf, &mut table_state);
            }
            KeyValue::Zset(ref value) => {
                let mut table_state = TableState::default();

                let widths = [Constraint::Percentage(50), Constraint::Percentage(50)];
                let header: Row<'_> = Row::new(["Member", "Score"].map(|h| Cell::from(h.bold())))
                    .top_margin(1)
                    .bottom_margin(1)
                    .fg(config::get().colors.base04)
                    .bg(config::get().colors.base02);

                let rows = value.iter().map(|(member, score)| {
                    Row::new([Cell::from(member.clone()), Cell::from(score.to_string())])
                        .fg(config::get().colors.base04)
                        .bg(config::get().colors.base00)
                });
                let table: Table<'_> = Table::new(rows, widths)
                    .header(header)
                    .widths(widths)
                    .flex(ratatui::layout::Flex::Center)
                    .highlight_symbol(HIGHLIGHT_SYMBOL)
                    .highlight_style(config::get().colors.base05)
                    .highlight_spacing(HighlightSpacing::Always);

                StatefulWidget::render(table, view_area, buf, &mut table_state);
            }
            _ => {}
        }
    }
}

const HIGHLIGHT_SYMBOL: &str = " >> ";

impl StatefulWidget for KeySpaceWidget {
    type State = KeySpace;

    fn render(
        self,
        area: ratatui::prelude::Rect,
        buf: &mut ratatui::prelude::Buffer,
        state: &mut Self::State,
    ) {
        let [t_area, view_area] =
            Layout::horizontal([Constraint::Percentage(35), Constraint::Fill(1)])
                .flex(ratatui::layout::Flex::Center)
                .areas(area);

        let space_block = Block::new()
            .bg(config::get().colors.base00)
            .fg(config::get().colors.base04)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .borders(Borders::all())
            .title("Keys");

        let table_area = space_block.inner(t_area);
        space_block.render(t_area, buf);

        Block::new()
            .bg(config::get().colors.base00)
            .fg(config::get().colors.base04)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .borders(Borders::all())
            .title("Values")
            .render(view_area, buf);

        let [filter_area, table_area] = Layout::vertical([Constraint::Min(2), Constraint::Fill(3)])
            .flex(ratatui::layout::Flex::Center)
            .margin(1)
            .areas(table_area);

        let filters_block = Block::new()
            .bg(config::get().colors.base00)
            .fg(config::get().colors.base04)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .borders(Borders::all())
            .title("Filters");

        let filters_area = filters_block.inner(filter_area);
        filters_block.render(filter_area, buf);

        let [cursor_size_are, pattern_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1)])
                .flex(ratatui::layout::Flex::Center)
                .areas(filters_area);

        let [cursor_area, size_area] = Layout::horizontal([Constraint::Min(1), Constraint::Min(1)])
            .flex(ratatui::layout::Flex::Center)
            .areas(cursor_size_are);

        Paragraph::new(format!("Cursor: {}", state.cursor.unwrap_or_default()))
            .bold()
            .alignment(Alignment::Left)
            .render(cursor_area, buf);

        Paragraph::new("Size: 10")
            .bold()
            .alignment(Alignment::Right)
            .render(size_area, buf);

        Paragraph::new(format!(
            "Pattern: {}",
            state.pattern.as_deref().unwrap_or_else(|| "*")
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
                .fg(config::get().colors.base04)
                .bg(config::get().colors.base02);

        let rows = state
            .keys
            .clone()
            .into_iter()
            .enumerate()
            .map(|(idx, meta)| {
                Row::new([
                    Cell::from(RedisType::from(meta.r_type)),
                    Cell::from(meta.key),
                    Cell::from(meta.ttl.to_string()),
                    Cell::from(format!(
                        "{:.2}",
                        unsafe { Byte::from_u128_unsafe(meta.size) }
                            .get_appropriate_unit(UnitType::Binary)
                    )),
                ])
                .fg(config::get().colors.base04)
                .bg(if idx % 2 == 0 {
                    config::get().colors.base00
                } else {
                    config::get().colors.base01
                })
            });
        let table: Table<'_> = Table::new(rows, widths)
            .header(header)
            .flex(ratatui::layout::Flex::Center)
            .highlight_symbol(HIGHLIGHT_SYMBOL)
            .highlight_style(config::get().colors.base05)
            .highlight_spacing(HighlightSpacing::Always);

        StatefulWidget::render(table, table_area, buf, &mut state.table);

        self.render_key_view(state, view_area, buf);

        if state.is_popup() {
            self.render_confirm_popup(state, area, buf)
        }
    }
}
