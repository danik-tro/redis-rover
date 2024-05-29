use ratatui::{layout::Flex, prelude::*, widgets::*};

#[derive(Default)]
pub struct Home {}

impl Widget for &Home {
    fn render(self, area: Rect, buf: &mut Buffer) {
        Paragraph::new("RedisRover")
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_set(symbols::border::PLAIN)
                    .padding(Padding::horizontal(1))
                    .border_style(Style::new().blue()),
            )
            .render(area, buf)
    }
}
