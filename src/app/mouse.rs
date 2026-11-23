//! Mouse events, matched against the regions of the last frame.

use ratatui::crossterm::event::{MouseEvent, MouseEventKind};

use super::App;
use crate::action::Action;

impl App {
    /// Handle one mouse event. `handle_event` leaves the frame clean for
    /// these: terminals can report every pointer move, and those must not
    /// cost a redraw, so whatever changes the screen here marks it dirty.
    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) -> Option<Action> {
        if matches!(mouse.kind, MouseEventKind::Moved) {
            return None;
        }
        let _target = self.frame.hits.at(mouse.column, mouse.row)?;
        None
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{
        buffer::Buffer,
        crossterm::event::{KeyModifiers, MouseButton},
        layout::Rect,
        widgets::StatefulWidget,
    };

    use super::*;
    use crate::{
        app::{render::AppWidget, tests::app},
        hit_map::HitTarget,
        tui,
    };

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn a_frame_records_the_screen_and_the_footer() {
        let mut app = app();
        let area = Rect::new(0, 0, 120, 40);
        let mut buf = Buffer::empty(area);
        app.build_frame(area);
        AppWidget.render(area, &mut buf, &mut app);

        assert_eq!(app.frame.hits.at(10, 10), Some(HitTarget::Main));
        assert_eq!(app.frame.hits.at(10, 36), Some(HitTarget::Footer));
        assert_eq!(app.frame.hits.at(0, 0), None);
    }

    #[test]
    fn pointer_moves_never_mark_the_frame_dirty() {
        let mut app = app();
        app.redraw.dirty = false;

        let event = tui::Event::Mouse(mouse(MouseEventKind::Moved, 5, 5));

        assert_eq!(app.handle_event(&event), None);
        assert!(!app.redraw.dirty);
    }

    #[test]
    fn a_click_outside_every_region_does_nothing() {
        let mut app = app();

        let click = mouse(MouseEventKind::Down(MouseButton::Left), 500, 500);

        assert_eq!(app.handle_mouse(click), None);
    }
}
