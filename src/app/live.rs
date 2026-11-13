//! Live updates of the open key: what the App tells the live task, and what
//! it does when the key changes.

use crate::redis_client::{
    event::RedisEvent,
    live::{KeyChange, LiveCmd, Liveness},
    types::RedisType,
};

use super::App;
use crate::mode::Mode;

impl App {
    /// Tell the live task which key is open now, if that changed.
    pub(super) fn track_open_key(&mut self) {
        let db = *self.state.db.lock();
        let open = self
            .state
            .value_state
            .lock()
            .key
            .clone()
            .map(|key| (key, db));
        if open == self.live.tracked {
            return;
        }
        self.live.tracked.clone_from(&open);
        let cmd = match open {
            Some((key, db)) => LiveCmd::Track { key, db },
            None => LiveCmd::Untrack,
        };
        self.send_live(cmd);
    }

    /// Polling waits while the key list is off screen (dashboard,
    /// Pub/Sub), like the metadata backfill.
    pub(super) fn sync_live_pause(&mut self) {
        let paused = matches!(self.mode, Mode::Dashboard | Mode::PubSub);
        if paused != self.live.paused {
            self.live.paused = paused;
            self.send_live(LiveCmd::Pause(paused));
        }
    }

    pub(super) fn send_live(&self, cmd: LiveCmd) {
        if let Some(tx) = &self.live.tx {
            // Closed when `live.mode` is `off`: nothing listens.
            let _ = tx.send(cmd);
        }
    }

    /// The live task saw `key` change. Refetch it in place: same page,
    /// same row, same scroll; a new type starts over.
    pub(super) fn open_key_changed(&mut self, key: &str, change: &KeyChange) {
        // A page reload can leave the cursor elsewhere (or nowhere) while
        // the value state still names the key; nothing is shown for it then.
        if self
            .keyspace
            .selected_key()
            .is_none_or(|(selected, _)| selected != key)
        {
            return;
        }
        {
            let mut value_state = self.state.value_state.lock();
            if value_state.key.as_deref() != Some(key) {
                return;
            }
            match change {
                KeyChange::Changed { r_type } => {
                    let r_type = RedisType::from(r_type.as_str());
                    if r_type == value_state.r_type {
                        value_state.rescan();
                    } else {
                        value_state.select(key, r_type);
                    }
                }
                KeyChange::Deleted | KeyChange::Expired => value_state.rescan(),
            }
        }
        self.send_redis_event(RedisEvent::FetchValue);
    }

    pub(super) fn set_liveness(&mut self, status: &Liveness) {
        self.live.status = Some(status.clone());
    }

    #[cfg(test)]
    pub(super) fn liveness(&self) -> Option<&Liveness> {
        self.live.status.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::*;
    use crate::{
        action::Action,
        app::tests::{app, meta},
    };

    /// An App with keys `a` and `b` on the page, the cursor on `a`, and a
    /// live channel.
    fn live_app() -> (App, mpsc::UnboundedReceiver<LiveCmd>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut app = app().live_tx(tx);
        *app.state.keys.lock() = Some(vec![meta("a"), meta("b")]);
        app.load_new_keys();
        assert!(app.keyspace.select_key_by_name("a"));
        (app, rx)
    }

    fn sent(rx: &mut mpsc::UnboundedReceiver<LiveCmd>) -> Vec<LiveCmd> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[test]
    fn opening_a_key_tracks_it_once() {
        let (mut app, mut rx) = live_app();
        app.update(&Action::RequestSelectedValue);
        app.update(&Action::RequestSelectedValue);
        assert_eq!(
            sent(&mut rx),
            vec![LiveCmd::Track {
                key: "a".into(),
                db: 0
            }],
            "re-requesting the same key (after a write) tracks nothing new"
        );

        app.update(&Action::ScrollDown);
        app.update(&Action::RequestSelectedValue);
        assert_eq!(
            sent(&mut rx),
            vec![LiveCmd::Track {
                key: "b".into(),
                db: 0
            }]
        );

        // The runner clears the value state on a DB switch.
        *app.state.db.lock() = 3;
        app.state.value_state.lock().clear();
        app.update(&Action::DbSwitched);
        assert_eq!(sent(&mut rx), vec![LiveCmd::Untrack]);
    }

    #[test]
    fn a_change_refetches_in_place() {
        let (mut app, _rx) = live_app();
        app.update(&Action::RequestSelectedValue);
        let generation = {
            let mut state = app.state.value_state.lock();
            state.page = 2;
            state.generation
        };

        let changed = |r_type: &str| Action::OpenKeyChanged {
            key: "a".into(),
            change: KeyChange::Changed {
                r_type: r_type.into(),
            },
        };
        app.update(&changed("string"));
        {
            let state = app.state.value_state.lock();
            assert!(state.generation > generation, "refetched");
            assert_eq!(state.page, 2, "same page");
        }

        app.update(&changed("hash"));
        {
            let state = app.state.value_state.lock();
            assert_eq!(state.r_type, RedisType::Hash);
            assert_eq!(state.page, 0, "another type starts over");
        }
    }

    #[test]
    fn a_change_to_another_key_is_ignored() {
        let (mut app, _rx) = live_app();
        app.update(&Action::RequestSelectedValue);
        let generation = app.state.value_state.lock().generation;
        app.update(&Action::OpenKeyChanged {
            key: "b".into(),
            change: KeyChange::Deleted,
        });
        assert_eq!(app.state.value_state.lock().generation, generation);

        // The cursor left the key (a page reload): nothing is shown for it.
        app.keyspace.refresh();
        app.update(&Action::OpenKeyChanged {
            key: "a".into(),
            change: KeyChange::Deleted,
        });
        assert_eq!(app.state.value_state.lock().generation, generation);
    }

    #[test]
    fn polling_pauses_off_the_key_list() {
        let (mut app, mut rx) = live_app();
        app.update(&Action::ToggleDashboard);
        app.update(&Action::ToggleDashboard);
        assert_eq!(
            sent(&mut rx),
            vec![LiveCmd::Pause(true), LiveCmd::Pause(false)]
        );
    }

    #[test]
    fn status_is_kept() {
        let (mut app, _rx) = live_app();
        assert_eq!(app.liveness(), None);
        app.update(&Action::LiveStatus(Liveness::Live));
        assert_eq!(app.liveness(), Some(&Liveness::Live));
    }
}
