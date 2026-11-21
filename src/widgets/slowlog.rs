//! The Slowlog screen.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Stylize,
    widgets::{Block, BorderType, Borders, Paragraph, Widget},
};

use crate::config;

/// What one frame of the Slowlog screen draws, copied by `App::build_frame`.
#[derive(Debug, Default)]
pub struct SlowlogView;

pub struct SlowlogWidget<'a> {
    pub view: &'a SlowlogView,
}

impl Widget for SlowlogWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let SlowlogView = self.view;
        let colors = &config::get().colors;
        let block = Block::default()
            .title(" Slowlog ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(colors.base04)
            .fg(colors.base05);
        Paragraph::new("No entries").block(block).render(area, buf);
    }
}
