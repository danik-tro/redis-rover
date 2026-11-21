//! The MONITOR screen.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Stylize,
    widgets::{Block, BorderType, Borders, Paragraph, Widget},
};

use crate::config;

/// What one frame of the MONITOR screen draws, copied by `App::build_frame`.
#[derive(Debug, Default)]
pub struct MonitorView;

pub struct MonitorWidget<'a> {
    pub view: &'a MonitorView,
}

impl Widget for MonitorWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let MonitorView = self.view;
        let colors = &config::get().colors;
        let block = Block::default()
            .title(" Monitor ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(colors.base04)
            .fg(colors.base05);
        Paragraph::new("Not running").block(block).render(area, buf);
    }
}
