//! The clickable screen tabs on the bottom margin row.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};
use unicode_width::UnicodeWidthStr;

use crate::{config, mode::Mode};

/// A top-level screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Keys,
    Dashboard,
    PubSub,
    Monitor,
    Slowlog,
}

const TABS: [(Tab, &str); 5] = [
    (Tab::Keys, " Keys "),
    (Tab::Dashboard, " Dashboard "),
    (Tab::PubSub, " Pub/Sub "),
    (Tab::Monitor, " Monitor "),
    (Tab::Slowlog, " Slowlog "),
];

/// Columns from the screen's left edge to the first tab.
const LEFT: u16 = 2;

impl Tab {
    /// The tab of a screen.
    #[must_use]
    pub fn of(mode: Mode) -> Self {
        match mode {
            Mode::Dashboard => Self::Dashboard,
            Mode::PubSub => Self::PubSub,
            Mode::Monitor => Self::Monitor,
            Mode::Slowlog => Self::Slowlog,
            Mode::Common | Mode::KeySpace | Mode::Popup(_) => Self::Keys,
        }
    }
}

/// Draw the tabs on `area`'s first row and return where each went. Nothing
/// is drawn when they don't all fit. The active tab is bold and reversed, so
/// it doesn't depend on colour.
pub fn render(area: Rect, active: Tab, buf: &mut Buffer) -> Vec<(Rect, Tab)> {
    let total: usize = TABS.iter().map(|(_, label)| label.width()).sum();
    if usize::from(LEFT) * 2 + total > usize::from(area.width) || area.height == 0 {
        return Vec::new();
    }
    let colors = &config::get().colors;
    let mut x = area.x + LEFT;
    let mut placed = Vec::with_capacity(TABS.len());
    for (tab, label) in TABS {
        let style = if tab == active {
            Style::new()
                .fg(colors.base05)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::new().fg(colors.base04)
        };
        buf.set_string(x, area.y, label, style);
        let width = u16::try_from(label.width()).unwrap_or(0);
        placed.push((Rect::new(x, area.y, width, 1), tab));
        x += width;
    }
    placed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(buf: &Buffer) -> String {
        (0..buf.area.width).map(|x| buf[(x, 0)].symbol()).collect()
    }

    #[test]
    fn every_tab_is_drawn_where_it_is_recorded() {
        let area = Rect::new(0, 0, 80, 1);
        let mut buf = Buffer::empty(area);

        let placed = render(area, Tab::Keys, &mut buf);

        assert_eq!(placed.len(), 5);
        let row = text(&buf);
        for (rect, tab) in placed {
            let label = TABS.iter().find(|(t, _)| *t == tab).unwrap().1;
            let start = usize::from(rect.x);
            assert_eq!(&row[start..start + label.len()], label);
        }
    }

    #[test]
    fn the_active_tab_does_not_rely_on_colour() {
        let area = Rect::new(0, 0, 80, 1);
        let mut buf = Buffer::empty(area);

        let placed = render(area, Tab::PubSub, &mut buf);

        let (rect, _) = placed.iter().find(|(_, t)| *t == Tab::PubSub).unwrap();
        let modifier = buf[(rect.x, 0)].modifier;
        assert!(modifier.contains(Modifier::REVERSED | Modifier::BOLD));
    }

    #[test]
    fn a_row_too_narrow_gets_no_tabs() {
        let area = Rect::new(0, 0, 30, 1);
        let mut buf = Buffer::empty(area);

        assert!(render(area, Tab::Keys, &mut buf).is_empty());
        assert_eq!(text(&buf).trim(), "");
    }
}
