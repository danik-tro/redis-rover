use ratatui::{
    layout::{Constraint, Layout},
    style::Stylize,
    text::Line,
    widgets::{
        Block, Borders, Clear, HighlightSpacing, Paragraph, Row, StatefulWidget, Table, TableState,
        Widget,
    },
};

use crate::config;

pub struct KeySpace {
    table: TableState,
    keys: Vec<String>,
}

impl KeySpace {
    pub fn new(keys: Vec<String>) -> Self {
        Self {
            keys,
            table: TableState::default(),
        }
    }

    pub fn set_keys(&mut self, keys: Vec<String>) {
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

const HIGHLIGHT_SYMBOL: &str = " █ ";

impl StatefulWidget for KeySpaceWidget {
    type State = KeySpace;

    fn render(
        self,
        area: ratatui::prelude::Rect,
        buf: &mut ratatui::prelude::Buffer,
        state: &mut Self::State,
    ) {
        let [table_area, view_area] =
            Layout::horizontal([Constraint::Percentage(33), Constraint::Fill(1)])
                .flex(ratatui::layout::Flex::Center)
                .areas(area);

        let widths = [Constraint::Max(25)];
        let header: Row<'_> = Row::new(["Key"].map(|h| Line::from(h.bold())))
            .fg(config::get().colors.base04)
            .bg(config::get().colors.base01)
            .height(3);

        let rows = state
            .keys
            .clone()
            .into_iter()
            .enumerate()
            .map(|(idx, key)| {
                Row::new([Line::from(key)])
                    .fg(config::get().colors.base04)
                    .bg(if idx % 2 == 0 {
                        config::get().colors.base00
                    } else {
                        config::get().colors.base01
                    })
            });
        let table: Table<'_> = Table::new(rows, widths)
            .header(header)
            .block(
                Block::new()
                    .bg(config::get().colors.base00)
                    .fg(config::get().colors.base04)
                    .border_type(ratatui::widgets::BorderType::Rounded)
                    .borders(Borders::all())
                    .title("Keys"),
            )
            .highlight_symbol(HIGHLIGHT_SYMBOL)
            .highlight_style(config::get().colors.base05)
            .column_spacing(3)
            .highlight_spacing(HighlightSpacing::Always);

        StatefulWidget::render(table, table_area, buf, &mut state.table);
        self.render_confirm_popup("Delete key?".into(), area, buf);
    }
}
