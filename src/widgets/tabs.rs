use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Style, Stylize},
    symbols,
    text::Line,
    widgets::{Block, Borders, Padding, Paragraph, Widget},
};
use strum::{Display, EnumIter, FromRepr};

use crate::config;

#[derive(Debug, Default, Clone, Copy, Display, FromRepr, EnumIter)]
pub enum SelectedTab {
    #[default]
    KeySpace,
    Info,
}

impl SelectedTab {
    pub fn select(&mut self, selected_tab: SelectedTab) {
        *self = selected_tab
    }

    pub fn highlight_style() -> Style {
        Style::default()
            .fg(config::get().colors.base00)
            .bg(config::get().colors.base0a)
            .bold()
    }
}

impl Widget for &SelectedTab {
    fn render(self, area: Rect, buf: &mut Buffer) {
        match self {
            SelectedTab::Info => self.render_tab_info(area, buf),
            SelectedTab::KeySpace => self.render_tab_key_space(area, buf),
        }
    }
}

impl SelectedTab {
    pub fn title(&self) -> Line<'static> {
        match self {
            _ => format!("  {self}  ")
                .fg(config::get().colors.base04)
                .bg(config::get().colors.base00)
                .into(),
        }
    }

    fn render_tab_info(&self, area: Rect, buf: &mut Buffer) {
        Paragraph::new("Info").block(self.block()).render(area, buf)
    }

    fn render_tab_key_space(&self, area: Rect, buf: &mut Buffer) {
        Paragraph::new("Key Space")
            .block(self.block())
            .render(area, buf)
    }

    fn block(&self) -> Block<'static> {
        Block::default()
            .borders(Borders::ALL)
            .border_set(symbols::border::PLAIN)
            .padding(Padding::horizontal(1))
            .border_style(config::get().colors.base03)
    }
}
