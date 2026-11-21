//! The Slowlog screen: loading, reset, and its keys.

use crossterm::event::{KeyCode, KeyEvent};

use super::{confirm::AfterWrite, App, READ_ONLY_NOTICE};
use crate::{action::Action, redis_client::event::RedisEvent};

impl App {
    /// Ask for the entries and settings (screen opened, `r`).
    pub(super) fn fetch_slowlog(&self) {
        self.send_redis_event(RedisEvent::FetchSlowlog);
    }

    pub(super) fn handle_slowlog_action(&mut self, action: &Action) {
        match action {
            Action::SlowlogRefresh => self.fetch_slowlog(),
            Action::SlowlogSort => self.slowlog.toggle_sort(),
            Action::SlowlogEnter => self.slowlog.open_detail(),
            Action::SlowlogReset => self.request_slowlog_reset(),
            Action::SlowlogLoaded => {
                let data = self.state.slowlog.lock().take();
                if let Some(data) = data {
                    self.slowlog.load(data);
                }
            }
            _ => {}
        }
    }

    /// `R`: a write, so it goes through the preview. Refused up front while
    /// read-only (the runner refuses it too).
    fn request_slowlog_reset(&mut self) {
        if self.is_read_only() {
            self.toast.show(READ_ONLY_NOTICE, false, self.clock.now());
            return;
        }
        // On a cluster the reset covers one node: say which.
        self.pin_picked_node();
        let notes = self
            .picked_node_label()
            .map(|node| format!("node {node}"))
            .into_iter()
            .collect();
        self.submit_write_noted(RedisEvent::SlowlogReset, AfterWrite::default(), None, notes);
    }

    /// Esc backs out one layer: help, detail, then the screen.
    pub(super) fn handle_slowlog_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.code == KeyCode::Esc {
            if self.help_is_open() {
                self.help_opened_at = None;
                return None;
            }
            if self.slowlog.is_detail() {
                self.slowlog.close_detail();
                return None;
            }
        }
        self.handle_keybindings(key)
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::sync::mpsc;

    use crate::{
        app::{
            tests::{app_with_channels, key},
            App,
        },
        mode::Mode,
        redis_client::event::RedisEvent,
    };

    fn press(app: &mut App, k: KeyEvent) {
        if let Some(action) = app.handle_key_event(k) {
            app.update(&action);
        }
    }

    fn opened() -> (App, mpsc::UnboundedReceiver<RedisEvent>) {
        let (mut app, _, mut events) = app_with_channels();
        press(&mut app, key(KeyCode::Char('g')));
        press(&mut app, key(KeyCode::Char('s')));
        assert_eq!(app.mode, Mode::Slowlog);
        assert!(matches!(events.try_recv(), Ok(RedisEvent::FetchSlowlog)));
        (app, events)
    }

    fn shift_r() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT)
    }

    #[test]
    fn reset_shows_the_preview_first() {
        let (mut app, mut events) = opened();
        press(&mut app, shift_r());
        assert!(app.pending.is_some());
        assert!(events.try_recv().is_err(), "nothing sent yet");
        press(&mut app, key(KeyCode::Char('y')));
        assert!(matches!(events.try_recv(), Ok(RedisEvent::SlowlogReset)));
    }

    #[test]
    fn reset_is_refused_while_read_only() {
        let (mut app, mut events) = opened();
        app.set_read_only(true);
        press(&mut app, shift_r());
        assert!(app.pending.is_none());
        assert!(events.try_recv().is_err());
        assert!(app.is_read_only(), "R doesn't toggle it here");
    }
}
