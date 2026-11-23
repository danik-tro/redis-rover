//! Mouse events, matched against the regions of the last frame.

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

use super::App;
use crate::{action::Action, hit_map::HitTarget, mode::Mode, widgets::pubsub::Pane};

/// Longest gap between the two clicks of a double-click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// Lines of a text value one wheel notch scrolls.
const WHEEL_TEXT_LINES: usize = 3;

/// The last left click, for spotting a double-click.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LastClick {
    at: Instant,
    target: HitTarget,
    row: usize,
}

impl App {
    /// Handle one mouse event. `handle_event` leaves the frame clean for
    /// these: terminals can report every pointer move, and those must not
    /// cost a redraw, so whatever changes the screen here marks it dirty.
    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) -> Option<Action> {
        if matches!(mouse.kind, MouseEventKind::Moved) || self.mouse_blocked() {
            return None;
        }
        let target = self.frame.hits.at(mouse.column, mouse.row)?;
        let now = self.clock.now();
        let action = match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => self.click(target, mouse, now),
            MouseEventKind::ScrollDown => self.wheel(target, true, now),
            MouseEventKind::ScrollUp => self.wheel(target, false, now),
            _ => return None,
        };
        self.redraw.dirty = true;
        action
    }

    /// Something modal is up: what is under it takes no mouse input.
    fn mouse_blocked(&self) -> bool {
        matches!(self.mode, Mode::Popup(_))
            || self.pending.is_some()
            || self.help_is_open()
            || self.db_picker.is_open()
            || (self.mode == Mode::KeySpace && self.keyspace.has_modal())
            || (self.mode == Mode::PubSub && self.pubsub.is_popup())
            || (self.mode == Mode::Monitor && self.monitor.is_filter_open())
    }

    fn click(&mut self, target: HitTarget, mouse: MouseEvent, now: Instant) -> Option<Action> {
        match target {
            HitTarget::KeysPane => self.click_keys(mouse, now),
            HitTarget::ValuePane => self.click_value(mouse, now),
            HitTarget::PubSubList => {
                self.focus_pubsub(Pane::List);
                None
            }
            HitTarget::PubSubLog => {
                self.focus_pubsub(Pane::Log);
                None
            }
            HitTarget::Main | HitTarget::Footer => None,
        }
    }

    /// Whether this click on `row` of `target` completes a double-click. A
    /// third click starts over.
    fn is_double_click(&mut self, target: HitTarget, row: usize, now: Instant) -> bool {
        let double = self.last_click.is_some_and(|last| {
            last.target == target && last.row == row && now.duration_since(last.at) <= DOUBLE_CLICK
        });
        self.last_click = (!double).then_some(LastClick {
            at: now,
            target,
            row,
        });
        double
    }

    /// A click selects the key row; a double-click opens it, at once if its
    /// value is here, else as soon as it loads.
    fn click_keys(&mut self, mouse: MouseEvent, now: Instant) -> Option<Action> {
        let row = self.keyspace.key_row_at(mouse.column, mouse.row)?;
        let double = self.is_double_click(HitTarget::KeysPane, row, now);
        if self.keyspace.click_key_row(row) {
            self.cursor_moved(now);
        }
        if !double {
            return None;
        }
        if self.keyspace.selected_group().is_some() || self.keyspace.has_loaded_value() {
            return Some(Action::EnterValue);
        }
        self.open_when_loaded = self.keyspace.selected_key().map(|(key, _)| key);
        if std::mem::take(&mut self.value_pending) {
            self.request_selected_value();
        }
        None
    }

    /// A click selects a value row; a double-click opens what has a level
    /// below it (a JSON field, a stream row). Elsewhere in the pane a click
    /// focuses the value.
    fn click_value(&mut self, mouse: MouseEvent, now: Instant) -> Option<Action> {
        let Some(row) = self.keyspace.value_row_at(mouse.column, mouse.row) else {
            if self.keyspace.is_keys_focused() {
                self.open_value();
            }
            return None;
        };
        let double = self.is_double_click(HitTarget::ValuePane, row, now);
        self.keyspace.click_value_row(row);
        self.remember_open_key();
        let opens =
            self.keyspace.selected_json_field().is_some() || self.keyspace.is_stream_focused();
        (double && opens).then_some(Action::EnterValue)
    }

    fn open_value(&mut self) {
        self.enter_value();
        self.remember_open_key();
    }

    /// Focus `pane` of the Pub/Sub tab. A row with no log keeps the list.
    fn focus_pubsub(&mut self, pane: Pane) {
        if self.pubsub.focus() != pane {
            self.pubsub.toggle_focus();
        }
    }

    /// The wheel moves the pane under the pointer, wherever the focus is.
    fn wheel(&mut self, target: HitTarget, down: bool, now: Instant) -> Option<Action> {
        match (self.mode, target) {
            (Mode::KeySpace, HitTarget::KeysPane) => self.wheel_keys(down, now),
            (Mode::KeySpace, HitTarget::ValuePane) => {
                self.wheel_value(down, now);
                None
            }
            (Mode::PubSub, HitTarget::PubSubList | HitTarget::PubSubLog) => {
                let pane = if target == HitTarget::PubSubList {
                    Pane::List
                } else {
                    Pane::Log
                };
                self.focus_pubsub(pane);
                if self.pubsub.focus() == pane {
                    self.pubsub_move(down);
                }
                None
            }
            (Mode::Monitor | Mode::Slowlog, HitTarget::Main) => {
                self.scroll(down, now);
                None
            }
            _ => None,
        }
    }

    fn scroll(&mut self, down: bool, now: Instant) {
        if down {
            self.scroll_down(now);
        } else {
            self.scroll_up(now);
        }
    }

    /// One row on the key list; past a page's end it turns the page.
    fn wheel_keys(&mut self, down: bool, now: Instant) -> Option<Action> {
        if !self.keyspace.is_keys_focused() {
            self.keyspace.exit_value_focus();
        }
        if self.keyspace.at_key_page_edge(down) {
            return Some(if down {
                Action::LoadNextPage
            } else {
                Action::LoadPreviousPage
            });
        }
        self.scroll(down, now);
        None
    }

    /// Scroll the value. The first notch over a table only enters it.
    fn wheel_value(&mut self, down: bool, now: Instant) {
        if self.keyspace.is_keys_focused() {
            self.open_value();
            if !self.keyspace.is_text_focused() {
                return;
            }
        }
        if self.keyspace.is_text_focused() {
            self.keyspace.scroll_text(down, WHEEL_TEXT_LINES);
        } else if self.keyspace.is_value_focused() && !self.keyspace.at_value_end(down) {
            self.scroll(down, now);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{Duration, SystemTime},
    };

    use ratatui::{
        buffer::Buffer, crossterm::event::KeyModifiers, layout::Rect, widgets::StatefulWidget,
    };

    use super::*;
    use crate::{
        app::{
            render::AppWidget,
            tests::{app, meta},
        },
        clock::FixedClock,
        redis_client::types::{KeyValue, LoadedValue, ValuePageInfo},
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

    fn left(column: u16, row: u16) -> MouseEvent {
        mouse(MouseEventKind::Down(MouseButton::Left), column, row)
    }

    fn draw(app: &mut App) -> Buffer {
        let area = Rect::new(0, 0, 120, 40);
        let mut buf = Buffer::empty(area);
        app.build_frame(area);
        AppWidget.render(area, &mut buf, app);
        buf
    }

    /// The terminal row showing `text`, in the key pane's columns.
    fn row_of(buf: &Buffer, text: &str) -> u16 {
        (0..buf.area.height)
            .find(|&y| {
                (0..40)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .contains(text)
            })
            .unwrap_or_else(|| panic!("{text} is not on screen"))
    }

    fn app_with_keys() -> (App, Arc<FixedClock>) {
        let mut app = app();
        let clock = Arc::new(FixedClock::new(SystemTime::UNIX_EPOCH));
        app.clock = clock.clone();
        app.keyspace
            .set_keys(vec![meta("user:1"), meta("user:2"), meta("user:3")]);
        (app, clock)
    }

    fn list_value(items: &[&str]) -> LoadedValue {
        LoadedValue {
            value: KeyValue::List(items.iter().map(|s| (*s).to_owned()).collect()),
            page: ValuePageInfo::default(),
        }
    }

    #[test]
    fn a_frame_records_the_panes_and_the_footer() {
        let (mut app, _) = app_with_keys();
        draw(&mut app);

        assert_eq!(app.frame.hits.at(10, 10), Some(HitTarget::KeysPane));
        assert_eq!(app.frame.hits.at(80, 10), Some(HitTarget::ValuePane));
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

        assert_eq!(app.handle_mouse(left(500, 500)), None);
    }

    #[test]
    fn a_click_selects_the_key_row_and_marks_the_frame_dirty() {
        let (mut app, _) = app_with_keys();
        let buf = draw(&mut app);
        app.redraw.dirty = false;

        let action = app.handle_mouse(left(10, row_of(&buf, "user:2")));

        assert_eq!(action, None);
        assert_eq!(app.keyspace.selected_key().unwrap().0, "user:2");
        assert!(app.redraw.dirty);
    }

    #[test]
    fn a_double_click_opens_a_loaded_value() {
        let (mut app, clock) = app_with_keys();
        let buf = draw(&mut app);
        let row = row_of(&buf, "user:2");
        app.handle_mouse(left(10, row));
        app.keyspace.set_selected_value(Some(list_value(&["a"])));

        clock.advance(Duration::from_millis(200));

        assert_eq!(app.handle_mouse(left(10, row)), Some(Action::EnterValue));
    }

    #[test]
    fn a_double_click_before_the_value_lands_opens_it_on_arrival() {
        let (mut app, clock) = app_with_keys();
        let buf = draw(&mut app);
        let row = row_of(&buf, "user:2");
        app.handle_mouse(left(10, row));
        clock.advance(Duration::from_millis(200));
        assert_eq!(app.handle_mouse(left(10, row)), None);
        assert!(app.keyspace.is_keys_focused());

        *app.state.selected_value.lock() = Some(list_value(&["a"]));
        app.load_selected_value(app.clock.now());

        assert!(app.keyspace.is_value_focused());
    }

    #[test]
    fn two_slow_clicks_are_not_a_double_click() {
        let (mut app, clock) = app_with_keys();
        let buf = draw(&mut app);
        let row = row_of(&buf, "user:2");
        app.handle_mouse(left(10, row));
        app.keyspace.set_selected_value(Some(list_value(&["a"])));

        clock.advance(DOUBLE_CLICK + Duration::from_millis(1));

        assert_eq!(app.handle_mouse(left(10, row)), None);
    }

    #[test]
    fn clicks_on_two_rows_are_not_a_double_click() {
        let (mut app, _) = app_with_keys();
        let buf = draw(&mut app);
        app.handle_mouse(left(10, row_of(&buf, "user:1")));
        app.keyspace.set_selected_value(Some(list_value(&["a"])));

        assert_eq!(app.handle_mouse(left(10, row_of(&buf, "user:2"))), None);
    }

    #[test]
    fn the_wheel_moves_the_key_under_the_pointer_and_stops_at_the_last_page() {
        let (mut app, _) = app_with_keys();
        let buf = draw(&mut app);
        let row = row_of(&buf, "user:1");
        app.handle_mouse(left(10, row));

        app.handle_mouse(mouse(MouseEventKind::ScrollDown, 10, row));
        assert_eq!(app.keyspace.selected_key().unwrap().0, "user:2");
        app.handle_mouse(mouse(MouseEventKind::ScrollDown, 10, row));

        assert_eq!(
            app.handle_mouse(mouse(MouseEventKind::ScrollDown, 10, row)),
            Some(Action::LoadNextPage),
            "past the last row the wheel turns the page, it doesn't wrap"
        );
        assert_eq!(app.keyspace.selected_key().unwrap().0, "user:3");
    }

    #[test]
    fn the_wheel_over_the_value_leaves_the_selected_key_alone() {
        let (mut app, _) = app_with_keys();
        let buf = draw(&mut app);
        app.handle_mouse(left(10, row_of(&buf, "user:2")));
        app.keyspace
            .set_selected_value(Some(list_value(&["a", "b", "c"])));
        draw(&mut app);

        app.handle_mouse(mouse(MouseEventKind::ScrollDown, 80, 20));
        app.handle_mouse(mouse(MouseEventKind::ScrollDown, 80, 20));

        assert_eq!(app.keyspace.selected_key().unwrap().0, "user:2");
        assert!(app.keyspace.is_value_focused());
        assert_eq!(app.keyspace.value_selected_row(), Some(1));
    }

    #[test]
    fn the_wheel_stops_at_the_end_of_a_one_page_value() {
        let (mut app, _) = app_with_keys();
        let buf = draw(&mut app);
        app.handle_mouse(left(10, row_of(&buf, "user:2")));
        app.keyspace
            .set_selected_value(Some(list_value(&["a", "b"])));
        draw(&mut app);

        for _ in 0..5 {
            app.handle_mouse(mouse(MouseEventKind::ScrollDown, 80, 20));
        }

        assert_eq!(app.keyspace.value_selected_row(), Some(1));
    }

    #[test]
    fn nothing_under_a_modal_takes_the_mouse() {
        let (mut app, _) = app_with_keys();
        let buf = draw(&mut app);
        let row = row_of(&buf, "user:2");
        app.keyspace.enter_filter_pattern(app.clock.now());

        assert_eq!(app.handle_mouse(left(10, row)), None);
        app.handle_mouse(mouse(MouseEventKind::ScrollDown, 10, row));

        assert_eq!(app.keyspace.selected_row(), None);
    }
}
