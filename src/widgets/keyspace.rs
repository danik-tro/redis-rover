use byte_unit::{Byte, UnitType};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Margin},
    style::Stylize,
    widgets::{
        Block, Borders, Cell, Clear, HighlightSpacing, Paragraph, Row, StatefulWidget, Table,
        TableState, Widget, Wrap,
    },
};

use crate::{
    config,
    redis_client::types::{KeyMeta, RedisType},
};

pub struct KeySpace {
    table: TableState,
    keys: Vec<KeyMeta>,
    cursor: Option<usize>,
    pattern: Option<String>,
}

impl KeySpace {
    pub fn new(keys: Vec<KeyMeta>) -> Self {
        Self {
            keys,
            table: TableState::default(),
            cursor: None,
            pattern: None,
        }
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
        message: String,
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
            Constraint::Min(message.len() as u16 + 1),
            Constraint::Percentage(30),
        ])
        .flex(ratatui::layout::Flex::Center)
        .areas(popup_area);

        let [yes_area, no_area] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .flex(ratatui::layout::Flex::Center)
                .areas(popup_area);

        let [_, confirm_area, _] = Layout::vertical([
            Constraint::Percentage(80),
            Constraint::Min(1),
            Constraint::Percentage(10),
        ])
        .flex(ratatui::layout::Flex::Center)
        .areas(popup_area);

        let [yes_area, no_area] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .flex(ratatui::layout::Flex::Center)
                .areas(confirm_area);

        Widget::render(Clear, popup_area, buf);
        Block::new()
            .title(message)
            .title_alignment(ratatui::layout::Alignment::Left)
            .borders(Borders::all())
            .border_type(ratatui::widgets::BorderType::Rounded)
            .fg(config::get().colors.base04)
            .bg(config::get().colors.base00)
            .render(popup_area, buf);

        Paragraph::new("[y] Confirm")
            .alignment(ratatui::layout::Alignment::Center)
            .render(yes_area, buf);
        Paragraph::new("[n] Discard")
            .alignment(ratatui::layout::Alignment::Center)
            .render(no_area, buf);
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
            Layout::horizontal([Constraint::Percentage(25), Constraint::Fill(1)])
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
    }
}
