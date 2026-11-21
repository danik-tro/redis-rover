//! The MONITOR screen: starting and stopping the task, and its keys.

use crossterm::event::{KeyCode, KeyEvent};

use super::{confirm::PendingConfirm, App};
use crate::{
    action::Action,
    config,
    mode::Mode,
    redis_client::{monitor::MonitorCmd, monitor_store::MonitorStatus},
};

impl App {
    /// MONITOR runs (or is trying to).
    pub(super) fn monitor_active(&self) -> bool {
        self.monitor_link.on
            && !matches!(self.state.monitor.lock().status(), MonitorStatus::Failed(_))
    }

    /// Opening the screen asks to start MONITOR, unless it already runs.
    pub(super) fn offer_monitor(&mut self) {
        if !self.monitor_active() {
            self.pending = Some(PendingConfirm::start_monitor(self.environment));
        }
    }

    pub(super) fn start_monitor(&mut self) {
        self.monitor_link.on = true;
        self.send_monitor(MonitorCmd::Start);
    }

    fn stop_monitor(&mut self) {
        self.monitor_link.on = false;
        self.send_monitor(MonitorCmd::Stop);
    }

    fn send_monitor(&self, cmd: MonitorCmd) {
        if let Some(tx) = &self.monitor_link.tx {
            // Closed only while the app shuts down.
            let _ = tx.send(cmd);
        }
    }

    /// The mode changed: MONITOR costs the server CPU, so it stops once its
    /// screen is gone (an error popup over it doesn't count).
    pub(super) fn sync_monitor_run(&mut self) {
        if self.monitor_link.on
            && self.view_mode() != Mode::Monitor
            && !config::get().monitor.keep_running
        {
            self.stop_monitor();
        }
    }

    pub(super) fn handle_monitor_action(&mut self, action: &Action) {
        match action {
            Action::MonitorToggle => {
                if self.monitor_active() {
                    self.stop_monitor();
                } else {
                    self.offer_monitor();
                }
            }
            Action::MonitorFilter => self.monitor.open_filter(),
            Action::MonitorTogglePause => self.monitor.toggle_pause(),
            Action::MonitorClear => self.monitor.clear(),
            Action::MonitorEnter => self.monitor.open_detail(),
            _ => {}
        }
    }

    /// Keys on the screen. The filter input takes raw keys; Esc then backs
    /// out one layer at a time: help, detail, the screen.
    pub(super) fn handle_monitor_key(&mut self, key: KeyEvent) -> Option<Action> {
        if self.monitor.is_filter_open() {
            match key.code {
                KeyCode::Esc => self.monitor.close_filter(),
                KeyCode::Enter => self.monitor.apply_filter(),
                _ if Self::is_quit_key(key) => return Some(Action::Quit),
                _ => self.monitor.filter_key(key),
            }
            return None;
        }
        if key.code == KeyCode::Esc {
            if self.help_is_open() {
                self.help_opened_at = None;
                return None;
            }
            if self.monitor.is_detail() {
                self.monitor.close_detail();
                return None;
            }
        }
        self.handle_keybindings(key)
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent};
    use tokio::sync::mpsc;

    use crate::{
        app::{
            tests::{app, key},
            App,
        },
        mode::Mode,
        redis_client::{monitor::MonitorCmd, monitor_store::MonitorStatus},
        target::Environment,
    };

    fn press(app: &mut App, k: KeyEvent) {
        if let Some(action) = app.handle_key_event(k) {
            app.update(&action);
        }
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, key(KeyCode::Char(c)));
        }
    }

    fn with_channel(env: Environment) -> (App, mpsc::UnboundedReceiver<MonitorCmd>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (app().environment(env).monitor_tx(tx), rx)
    }

    fn sent(rx: &mut mpsc::UnboundedReceiver<MonitorCmd>) -> Vec<MonitorCmd> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[test]
    fn opening_asks_first_and_y_starts() {
        let (mut app, mut rx) = with_channel(Environment::Local);
        typed(&mut app, "gm");
        assert_eq!(app.mode, Mode::Monitor);
        assert!(app.pending.is_some());
        assert!(sent(&mut rx).is_empty(), "nothing before the answer");
        typed(&mut app, "y");
        assert_eq!(sent(&mut rx), [MonitorCmd::Start]);
    }

    #[test]
    fn declining_stays_on_the_screen_stopped() {
        let (mut app, mut rx) = with_channel(Environment::Local);
        typed(&mut app, "gmn");
        assert_eq!(app.mode, Mode::Monitor);
        assert!(app.pending.is_none());
        assert!(sent(&mut rx).is_empty());
        typed(&mut app, "t");
        assert!(app.pending.is_some(), "t asks again");
    }

    #[test]
    fn prod_needs_the_word_typed() {
        let (mut app, mut rx) = with_channel(Environment::Prod);
        typed(&mut app, "gm");
        typed(&mut app, "y");
        press(&mut app, key(KeyCode::Enter));
        assert!(sent(&mut rx).is_empty(), "y is a letter here");
        assert!(app.pending.is_some());
        press(&mut app, key(KeyCode::Backspace));
        typed(&mut app, "monitor");
        press(&mut app, key(KeyCode::Enter));
        assert_eq!(sent(&mut rx), [MonitorCmd::Start]);
        assert!(app.pending.is_none());
    }

    #[test]
    fn a_pasted_answer_does_not_start_it() {
        let (mut app, mut rx) = with_channel(Environment::Local);
        typed(&mut app, "gm");
        app.handle_paste("y");
        assert!(app.pending.is_some());
        assert!(sent(&mut rx).is_empty());
    }

    #[test]
    fn leaving_stops_it_but_an_error_popup_does_not() {
        let (mut app, mut rx) = with_channel(Environment::Local);
        typed(&mut app, "gmy");
        sent(&mut rx);

        app.update(&crate::action::Action::Error("boom".into()));
        assert!(sent(&mut rx).is_empty(), "still on the screen");
        press(&mut app, key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Monitor);
        assert!(sent(&mut rx).is_empty());

        press(&mut app, key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::KeySpace);
        assert_eq!(sent(&mut rx), [MonitorCmd::Stop]);
    }

    #[test]
    fn a_jump_to_another_screen_stops_it() {
        let (mut app, mut rx) = with_channel(Environment::Local);
        typed(&mut app, "gmy");
        sent(&mut rx);
        typed(&mut app, "gs");
        assert_eq!(app.mode, Mode::Slowlog);
        assert_eq!(sent(&mut rx), [MonitorCmd::Stop]);
    }

    #[test]
    fn a_refused_monitor_can_be_asked_for_again() {
        let (mut app, mut rx) = with_channel(Environment::Local);
        typed(&mut app, "gmy");
        sent(&mut rx);
        app.state
            .monitor
            .lock()
            .set_status(MonitorStatus::Failed("NOPERM".into()));
        typed(&mut app, "t");
        assert!(app.pending.is_some());
        assert!(sent(&mut rx).is_empty());
    }

    #[test]
    fn esc_backs_out_of_filter_then_detail_then_screen() {
        let (mut app, _rx) = with_channel(Environment::Local);
        typed(&mut app, "gmn");
        typed(&mut app, "/");
        assert!(app.monitor.is_filter_open());
        typed(&mut app, "q");
        assert_eq!(app.mode, Mode::Monitor, "letters go into the filter");
        press(&mut app, key(KeyCode::Esc));
        assert!(!app.monitor.is_filter_open());
        assert_eq!(app.mode, Mode::Monitor);
        press(&mut app, key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::KeySpace);
    }

    #[test]
    fn a_paste_goes_into_the_filter() {
        let (mut app, _rx) = with_channel(Environment::Local);
        typed(&mut app, "gmn/");
        app.handle_paste("user:*\n");
        assert!(app.monitor.is_filter_open(), "a line break didn't apply it");
    }
}
