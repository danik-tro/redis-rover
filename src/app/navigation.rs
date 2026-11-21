//! Jumping between the top-level screens with the `g` chords.

use super::App;
use crate::mode::Mode;

impl App {
    /// Show `target`. A jump always goes back to the key list: remembering
    /// the screen it came from would let two screens point at each other.
    pub(super) fn go_to(&mut self, target: Mode) {
        let top_level = self.mode == Mode::KeySpace || self.mode.hides_key_list();
        if self.mode == target || !top_level {
            return;
        }
        if self.mode == Mode::PubSub {
            self.pubsub.close_popup();
            self.pubsub.close_detail();
        }
        self.previous_mode = None;
        if target == Mode::PubSub {
            self.open_pubsub(Mode::KeySpace);
        } else {
            self.mode = target;
            self.sync_hint();
        }
    }

    /// Esc on a server screen.
    pub(super) fn leave_screen(&mut self) {
        self.previous_mode = None;
        self.mode = Mode::KeySpace;
        self.sync_hint();
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use crate::{
        app::{
            tests::{app, key},
            App,
        },
        mode::Mode,
    };

    fn press(app: &mut App, code: KeyCode) {
        if let Some(action) = app.handle_key_event(key(code)) {
            app.update(&action);
        }
    }

    fn chord(app: &mut App, second: char) {
        press(app, KeyCode::Char('g'));
        press(app, KeyCode::Char(second));
    }

    #[test]
    fn every_chord_works_from_every_top_level_screen() {
        let screens = [
            ('m', Mode::Monitor),
            ('s', Mode::Slowlog),
            ('p', Mode::PubSub),
            ('d', Mode::Dashboard),
        ];
        for (_, from) in screens {
            for (letter, to) in screens {
                let mut app = app();
                app.go_to(from);
                assert_eq!(app.mode, from);
                chord(&mut app, letter);
                assert_eq!(app.mode, to, "{from:?} -> g {letter}");
            }
        }
    }

    #[test]
    fn a_jump_goes_back_to_the_key_list() {
        let mut app = app();
        chord(&mut app, 'm');
        chord(&mut app, 's');
        chord(&mut app, 'm');
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.mode, Mode::KeySpace);

        chord(&mut app, 'p');
        chord(&mut app, 'd');
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.mode, Mode::KeySpace);
    }

    #[test]
    fn m_and_b_still_toggle() {
        let mut app = app();
        press(&mut app, KeyCode::Char('m'));
        assert_eq!(app.mode, Mode::Dashboard);
        press(&mut app, KeyCode::Char('b'));
        assert_eq!(app.mode, Mode::PubSub);
        press(&mut app, KeyCode::Char('b'));
        assert_eq!(app.mode, Mode::Dashboard);
        press(&mut app, KeyCode::Char('m'));
        assert_eq!(app.mode, Mode::KeySpace);
    }

    #[test]
    fn new_screens_pause_list_work() {
        let mut app = app();
        for (letter, mode) in [('m', Mode::Monitor), ('s', Mode::Slowlog)] {
            chord(&mut app, letter);
            assert_eq!(app.mode, mode);
            assert!(app.live.paused, "{mode:?}");
            press(&mut app, KeyCode::Esc);
            assert!(!app.live.paused, "{mode:?}");
        }
    }

    #[test]
    fn an_error_popup_returns_to_the_new_screen() {
        let mut app = app();
        chord(&mut app, 'm');
        app.show_error_popup("boom");
        app.switch_to_previous_mode();
        assert_eq!(app.mode, Mode::Monitor);
    }
}
