use super::App;
use std::time::{Instant, SystemTime};

use crate::{
    config,
    pubsub_notify::{self, Notice},
    redis_client::{event::RedisEvent, pubsub::PubSubCmd, pubsub_store::Target},
};

use crate::{
    mode::Mode,
    widgets::pubsub::{Pane, Row},
};

impl App {
    /// `b`: open the Pub/Sub tab from the keyspace or dashboard; in the tab,
    /// go back to where it was opened from.
    pub(super) fn toggle_pubsub(&mut self) {
        match self.mode {
            Mode::PubSub => {
                self.pubsub.close_popup();
                self.pubsub.close_detail();
                self.mode = self.pubsub_return;
                self.sync_hint();
            }
            Mode::KeySpace | Mode::Dashboard => {
                self.pubsub_return = self.mode;
                self.mode = Mode::PubSub;
                self.sync_hint();
                self.mark_selected_read();
                self.summary.pubsub_unread = 0;
                self.fetch_pubsub_channels();
            }
            Mode::Common | Mode::Popup(_) => {}
        }
    }

    /// Enter in the add popup: subscribe to the target typed.
    pub(super) fn confirm_add_subscription(&mut self) {
        if let Some(target) = self.pubsub.take_add_request() {
            self.subscribe_target(target);
        }
    }

    /// Enter in the tab. Log: open the selected message. List: subscribe to
    /// a server channel (it moves to the subscriptions), or move focus into
    /// a subscription's log.
    pub(super) fn pubsub_enter(&mut self) {
        if self.pubsub.is_detail() {
            return;
        }
        if self.pubsub.focus() == Pane::Log {
            self.pubsub.open_detail();
            return;
        }
        match self.pubsub.selected_row() {
            Some(Row::Server(name)) => self.subscribe_target(Target::Channel(name)),
            Some(Row::Sub(_)) => self.pubsub.toggle_focus(),
            None => {}
        }
    }

    /// Record the subscription (the UI shows it right away), select it, and
    /// ask the task to subscribe. A duplicate only selects the existing one.
    pub(super) fn subscribe_target(&mut self, target: Target) {
        let (index, added) = self.state.pubsub.lock().add(target.clone());
        self.pubsub.select(index);
        self.mark_selected_read();
        if added {
            self.send_pubsub_cmd(PubSubCmd::Subscribe(target));
        }
    }

    /// Ctrl-s in the publish popup. Under `--read-only` the popup stays open
    /// with its notice (the runner would reject the write anyway).
    pub(super) fn confirm_publish(&mut self) {
        if let Some((channel, payload)) = self.pubsub.take_publish_request() {
            self.send_redis_event(RedisEvent::Publish { channel, payload });
        }
    }

    /// Ask the runner for the server's channels (tab open, `r`).
    pub(super) fn fetch_pubsub_channels(&self) {
        self.send_redis_event(RedisEvent::FetchPubSubChannels);
    }

    /// `d` in the tab: unsubscribe and drop the selected subscription.
    pub(super) fn remove_subscription(&mut self) {
        let Some(target) = self.pubsub.selected_target() else {
            return;
        };
        self.state.pubsub.lock().remove(&target);
        self.pubsub.reset_log();
        self.pubsub.clamp_selection();
        self.send_pubsub_cmd(PubSubCmd::Unsubscribe(target));
    }

    /// The Pub/Sub tab is on screen (possibly under an error popup).
    fn pubsub_visible(&self) -> bool {
        self.mode == Mode::PubSub
            || (matches!(self.mode, Mode::Popup(_)) && self.previous_mode == Some(Mode::PubSub))
    }

    /// Subscription open in the tab: its messages are being read.
    fn open_subscription(&self) -> Option<usize> {
        self.pubsub_visible()
            .then(|| self.pubsub.selected_sub())
            .flatten()
    }

    /// The subscription selected in the tab has been read.
    fn mark_selected_read(&mut self) {
        if let Some(index) = self.open_subscription() {
            if let Some(sub) = self.state.pubsub.lock().subs.get_mut(index) {
                sub.unread = 0;
            }
        }
    }

    /// `N`: notifications on / off for the selected subscription (also
    /// un-mutes an auto-muted one).
    pub(super) fn toggle_notify(&mut self) {
        let Some(index) = self.pubsub.selected_sub() else {
            return;
        };
        if let Some(sub) = self.state.pubsub.lock().subs.get_mut(index) {
            sub.notify = sub.notify.toggled();
        }
    }

    /// Notifier, on every `Tick`: count what each subscription received since
    /// the last tick into its unread badge, run its notification state
    /// machine, and toast the notices. The subscription open in the tab
    /// stays read and never toasts. Returns whether anything shown changed
    /// (a badge, a mute, a toast).
    pub(super) fn pubsub_tick(&mut self, now: Instant, wall: SystemTime) -> bool {
        let policy = config::get().pubsub.notify.policy();
        let open = self.open_subscription();
        let mut notices: Vec<Notice> = Vec::new();
        let mut unread = 0;
        let mut changed = false;
        {
            let mut store = self.state.pubsub.lock();
            for (i, sub) in store.subs.iter_mut().enumerate() {
                let new = sub.stats.total.saturating_sub(sub.seen_total);
                sub.seen_total = sub.stats.total;
                let rate = f64::from(sub.stats.rate_1s(wall));
                let notify_before = std::mem::discriminant(&sub.notify);
                let preview = (new > 0
                    && matches!(sub.notify, pubsub_notify::NotifyState::On { .. }))
                .then(|| sub.newest_preview())
                .flatten();
                let notice = pubsub_notify::on_tick(
                    &mut sub.notify,
                    sub.target.name(),
                    new,
                    rate,
                    preview.as_deref(),
                    now,
                    &policy,
                );
                changed |= notify_before != std::mem::discriminant(&sub.notify);
                if open == Some(i) {
                    changed |= sub.unread > 0;
                    sub.unread = 0;
                    continue;
                }
                changed |= new > 0;
                sub.unread += new;
                unread += sub.unread;
                notices.extend(notice);
            }
        }
        let unread = if self.pubsub_visible() { 0 } else { unread };
        changed |= self.summary.pubsub_unread != unread;
        self.summary.pubsub_unread = unread;
        for msg in pubsub_notify::toasts(notices) {
            changed |= self.toast.notify(msg, now);
        }
        changed
    }

    fn send_pubsub_cmd(&self, cmd: PubSubCmd) {
        if let Err(err) = self.pubsub_tx.send(cmd) {
            log::error!("Failed to send Pub/Sub command: {err:?}");
        }
    }

    /// `j` / `k` in the Pub/Sub tab: scroll the open detail, else move in
    /// the focused pane (the log pauses when moving up).
    pub(super) fn pubsub_move(&mut self, down: bool) {
        if self.pubsub.is_detail() {
            self.pubsub.scroll_detail(down);
            return;
        }
        match (self.pubsub.focus(), down) {
            (Pane::Log, true) => self.pubsub.log_down(),
            (Pane::Log, false) => self.pubsub.log_up(),
            (Pane::List, true) => {
                self.pubsub.select_next();
                self.mark_selected_read();
            }
            (Pane::List, false) => {
                self.pubsub.select_prev();
                self.mark_selected_read();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant, SystemTime};

    use crate::redis_client::{event::RedisEvent, pubsub::PubSubCmd, pubsub_store::Target};

    use crossterm::event::KeyCode;

    use crate::app::tests::{
        app, app_on_orders, app_with_channels, app_with_pubsub, app_with_subs, ctrl, key, publish,
        render_screen, tap, unread,
    };
    use crate::{action::Action, mode::Mode};

    #[test]
    fn pubsub_tab_back_survives_an_error_popup() {
        let mut app = app();
        app.toggle_dashboard();
        app.toggle_pubsub();
        assert_eq!(app.mode, Mode::PubSub);

        app.show_error_popup("Pub/Sub connection lost");
        app.switch_to_previous_mode();
        assert_eq!(app.mode, Mode::PubSub);

        assert_eq!(
            app.handle_key_event(key(KeyCode::Esc)),
            Some(Action::TogglePubSub)
        );
        app.toggle_pubsub();
        assert_eq!(app.mode, Mode::Dashboard);
    }

    #[test]
    fn add_popup_subscribes_once_and_remove_unsubscribes() {
        let (mut app, mut cmds) = app_with_pubsub();
        app.toggle_pubsub();
        for _ in 0..2 {
            app.pubsub.open_add();
            for c in "orders:*".chars() {
                assert_eq!(app.handle_key_event(key(KeyCode::Char(c))), None);
            }
            assert_eq!(app.handle_key_event(key(KeyCode::Enter)), None);
        }
        let target = Target::Pattern("orders:*".into());
        assert_eq!(cmds.try_recv(), Ok(PubSubCmd::Subscribe(target.clone())));
        assert!(cmds.try_recv().is_err(), "duplicate must not resubscribe");
        assert_eq!(app.state.pubsub.lock().subs.len(), 1);

        app.remove_subscription();
        assert_eq!(cmds.try_recv(), Ok(PubSubCmd::Unsubscribe(target)));
        assert!(app.state.pubsub.lock().subs.is_empty());
    }

    #[test]
    fn opening_the_tab_lists_server_channels() {
        let (mut app, _, mut events) = app_with_channels();
        app.toggle_pubsub();
        assert!(matches!(
            events.try_recv(),
            Ok(RedisEvent::FetchPubSubChannels)
        ));
    }

    #[test]
    fn enter_on_a_server_channel_subscribes_it() {
        use crate::redis_client::pubsub_store::ServerChannels;

        let (mut app, mut cmds, _events) = app_with_channels();
        app.toggle_pubsub();
        app.state.pubsub.lock().set_server_channels(ServerChannels {
            channels: vec![("events".into(), 1)],
            total: 1,
            patterns: 0,
        });
        // An empty subscription list: the server channel is row 0.
        assert_eq!(
            app.handle_key_event(key(KeyCode::Enter)),
            Some(Action::PubSubEnter)
        );
        app.pubsub_enter();
        let target = Target::Channel("events".into());
        assert_eq!(cmds.try_recv(), Ok(PubSubCmd::Subscribe(target.clone())));
        let store = app.state.pubsub.lock();
        assert_eq!(store.targets(), vec![target]);
        assert_eq!(store.server_only_channels().count(), 0);
        drop(store);
        assert_eq!(app.pubsub.selected_sub(), Some(0));
    }

    #[test]
    fn publish_popup_sends_on_ctrl_s() {
        let (mut app, _, mut events) = app_with_channels();
        app.toggle_pubsub();
        let _ = events.try_recv();
        app.pubsub.open_publish(false);
        for c in "orders:new".chars() {
            assert_eq!(app.handle_key_event(key(KeyCode::Char(c))), None);
        }
        assert_eq!(app.handle_key_event(key(KeyCode::Tab)), None);
        assert_eq!(app.handle_key_event(key(KeyCode::Char('x'))), None);
        assert_eq!(app.handle_key_event(ctrl('s')), None);
        match events.try_recv() {
            Ok(RedisEvent::Publish { channel, payload }) => {
                assert_eq!((channel.as_str(), payload.as_str()), ("orders:new", "x"));
            }
            other => panic!("expected Publish, got {other:?}"),
        }
        assert!(!app.pubsub.is_popup());
    }

    #[test]
    fn read_only_publish_sends_nothing() {
        let (app, _, mut events) = app_with_channels();
        let mut app = app.read_only(true);
        app.toggle_pubsub();
        let _ = events.try_recv();
        assert!(
            !app.summary.hint.contains("publish"),
            "{}",
            app.summary.hint
        );
        app.pubsub.open_publish(app.read_only);
        for c in "chan".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }
        app.handle_key_event(ctrl('s'));
        assert!(events.try_recv().is_err());
        assert!(app.pubsub.is_publish_popup(), "stays open with its notice");
    }

    #[test]
    fn notifier_toasts_first_message_then_only_counts() {
        let mut app = app_with_subs(&["chat"]);
        let start = Instant::now();
        app.pubsub_tick(start, SystemTime::now());
        assert_eq!(app.toast.message(), None);

        publish(&app, "chat", "hi");
        app.pubsub_tick(start, SystemTime::now());
        assert_eq!(app.toast.message(), Some("chat: hi"));
        assert_eq!(unread(&app), vec![1]);
        assert_eq!(app.summary.pubsub_unread, 1);

        // Inside the cooldown: the badge grows, no new toast.
        app.toast.expire(start + Duration::from_secs(5));
        publish(&app, "chat", "again");
        publish(&app, "chat", "and again");
        app.pubsub_tick(start + Duration::from_secs(5), SystemTime::now());
        assert_eq!(app.toast.message(), None);
        assert_eq!(unread(&app), vec![3]);
        assert!(render_screen(&mut app).contains("PubSub 3"));

        // Off: still counted, never toasted.
        app.toggle_pubsub();
        app.toggle_notify();
        app.toggle_pubsub();
        publish(&app, "chat", "later");
        app.pubsub_tick(start + Duration::from_mins(1), SystemTime::now());
        assert_eq!(app.toast.message(), None);
        assert_eq!(unread(&app), vec![1], "opening the tab read it");
    }

    #[test]
    fn several_subscriptions_share_one_toast() {
        let mut app = app_with_subs(&["a", "b", "c"]);
        for name in ["a", "b", "c"] {
            publish(&app, name, "x");
        }
        app.pubsub_tick(Instant::now(), SystemTime::now());
        assert_eq!(
            app.toast.message(),
            Some("3 subscriptions have new messages")
        );
        app.toast.expire(Instant::now() + Duration::from_secs(10));
        assert_eq!(app.toast.message(), None, "nothing else queued");
    }

    #[test]
    fn pubsub_toast_never_replaces_a_sticky_toast() {
        let mut app = app_with_subs(&["chat"]);
        app.toast.show("Copying key…", true, Instant::now());
        publish(&app, "chat", "hi");
        app.pubsub_tick(Instant::now(), SystemTime::now());
        assert_eq!(app.toast.message(), Some("Copying key…"));
        assert_eq!(unread(&app), vec![1]);
    }

    #[test]
    fn unread_resets_when_selected_in_the_tab() {
        let mut app = app_with_subs(&["a", "b"]);
        publish(&app, "a", "x");
        publish(&app, "b", "x");
        publish(&app, "b", "y");
        app.pubsub_tick(Instant::now(), SystemTime::now());
        assert_eq!(unread(&app), vec![1, 2]);
        assert_eq!(app.summary.pubsub_unread, 3);

        app.toggle_pubsub();
        assert_eq!(unread(&app), vec![0, 2], "the first one is selected");
        assert_eq!(app.summary.pubsub_unread, 0, "no global badge in the tab");
        let text = render_screen(&mut app);
        assert!(text.contains("(2)"), "{text}");
        assert!(!text.contains("PubSub 2"), "{text}");

        app.scroll_down();
        assert_eq!(unread(&app), vec![0, 0]);
    }

    #[test]
    fn open_subscription_never_toasts() {
        let mut app = app_with_subs(&["a", "b"]);
        app.toggle_pubsub();
        publish(&app, "a", "x");
        app.pubsub_tick(Instant::now(), SystemTime::now());
        assert_eq!(app.toast.message(), None);
        assert_eq!(unread(&app), vec![0, 0]);

        // Another subscription toasts, even in the tab.
        publish(&app, "b", "y");
        app.pubsub_tick(Instant::now(), SystemTime::now());
        assert_eq!(app.toast.message(), Some("b: y"));
        assert_eq!(unread(&app), vec![0, 1]);
    }

    #[test]
    fn clear_restarts_the_count() {
        let mut app = app_with_subs(&["a"]);
        publish(&app, "a", "x");
        app.pubsub_tick(Instant::now(), SystemTime::now());
        app.state.pubsub.lock().clear(0);
        publish(&app, "a", "y");
        app.pubsub_tick(Instant::now(), SystemTime::now());
        assert_eq!(unread(&app), vec![1]);
    }

    #[test]
    fn toggle_notify_cycles_and_unmutes() {
        use crate::pubsub_notify::NotifyState;
        let mut app = app_with_subs(&["a"]);
        app.toggle_pubsub();
        let state = |app: &App| app.state.pubsub.lock().subs[0].notify;
        assert!(matches!(state(&app), NotifyState::On { .. }));
        app.toggle_notify();
        assert_eq!(state(&app), NotifyState::Off);
        app.state.pubsub.lock().subs[0].notify = NotifyState::AutoMuted;
        assert!(render_screen(&mut app).contains('🔕'));
        app.toggle_notify();
        assert!(matches!(state(&app), NotifyState::On { .. }));
    }

    #[test]
    fn tab_space_and_enter_drive_the_log() {
        use crate::widgets::pubsub::Pane;

        let mut app = app_on_orders();
        assert_eq!(app.pubsub.focus(), Pane::List);
        tap(&mut app, KeyCode::Tab);
        assert_eq!(app.pubsub.focus(), Pane::Log);

        tap(&mut app, KeyCode::Char(' '));
        assert!(app.pubsub.log_cursor().is_paused());
        app.state
            .pubsub
            .lock()
            .push("orders:new", Some("orders:*"), b"4", SystemTime::now());
        assert!(render_screen(&mut app).contains("paused · +1 new"));
        tap(&mut app, KeyCode::Char(' '));
        assert!(!app.pubsub.log_cursor().is_paused());

        // k pauses on the previous row; j twice moves past the end and follows.
        tap(&mut app, KeyCode::Char('k'));
        assert!(app.pubsub.log_cursor().is_paused());
        tap(&mut app, KeyCode::Char('j'));
        assert!(app.pubsub.log_cursor().is_paused());
        tap(&mut app, KeyCode::Char('j'));
        assert!(!app.pubsub.log_cursor().is_paused());

        tap(&mut app, KeyCode::Tab);
        assert_eq!(app.pubsub.focus(), Pane::List);
        // Enter on a subscription in the list focuses its log.
        tap(&mut app, KeyCode::Enter);
        assert_eq!(app.pubsub.focus(), Pane::Log);
        tap(&mut app, KeyCode::Char('k'));
        tap(&mut app, KeyCode::Enter);
        assert!(app.pubsub.is_detail());
        let text = render_screen(&mut app);
        assert!(text.contains("\"id\": 3"), "{text}");
    }

    #[test]
    fn esc_closes_the_detail_before_leaving_the_tab() {
        let mut app = app_on_orders();
        tap(&mut app, KeyCode::Tab);
        tap(&mut app, KeyCode::Enter);
        assert!(app.pubsub.is_detail());
        tap(&mut app, KeyCode::Esc);
        assert!(!app.pubsub.is_detail());
        assert_eq!(app.mode, Mode::PubSub);
        tap(&mut app, KeyCode::Esc);
        assert_eq!(app.mode, Mode::KeySpace);

        // `b` leaves at once and drops the detail.
        app.toggle_pubsub();
        tap(&mut app, KeyCode::Enter);
        assert!(app.pubsub.is_detail());
        tap(&mut app, KeyCode::Char('b'));
        assert_eq!(app.mode, Mode::KeySpace);
        assert!(!app.pubsub.is_detail());
    }
}
