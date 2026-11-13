//! `Shift-a`: load the whole keyspace in the background, and `g o`: go back
//! to the key whose value was opened (searching the sweep for it if needed).

use super::{confirm::PendingConfirm, App};
use crate::{
    redis_client::{
        event::RedisEvent,
        types::{glob_match, TypeFilter},
    },
    widgets::keyspace::LoadProgress,
};

/// Above this many keys in the DB, `Shift-a` asks first.
const LOAD_ALL_ASK_ABOVE: u64 = 2_000_000;

/// Memory per loaded key besides its name: the sweep and its index (66 B,
/// from the 10M-key `merge_batch` bench), the metadata columns a sort
/// fills (13 B) and the sort order (4 B).
const BYTES_PER_KEY: u64 = 83;

/// Name length assumed before any key is loaded.
const DEFAULT_NAME_LEN: u64 = 24;

/// What the running load is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Loading {
    /// `Shift-a`: to the end of the sweep.
    All,
    /// `g o`: until this key is loaded.
    Seek(String),
}

impl App {
    /// `Shift-a`: load every key of the sweep in the background. Above
    /// [`LOAD_ALL_ASK_ABOVE`] keys it asks first, with a memory estimate.
    pub(super) fn request_load_all(&mut self) {
        if self.loading == Some(Loading::All) {
            return;
        }
        let (done, name_len) = {
            let keyspace = self.state.keyspace_state.lock();
            (keyspace.sweep_done, keyspace.average_name_len())
        };
        if done {
            let now = self.clock.now();
            self.toast.show("Every key is loaded", false, now);
            return;
        }
        let db = *self.state.db.lock();
        let keys = self
            .state
            .info
            .lock()
            .as_ref()
            .map_or(0, |info| info.db_keys(db));
        if keys > LOAD_ALL_ASK_ABOVE {
            let name_len = name_len.map_or(DEFAULT_NAME_LEN, |n| n as u64);
            let bytes = keys.saturating_mul(BYTES_PER_KEY + name_len);
            self.pending = Some(PendingConfirm::LoadAll { db, keys, bytes });
            return;
        }
        self.start_load(None);
    }

    /// Start the background load: to the end, or until `until` is loaded.
    /// Replaces a load already running.
    pub(super) fn start_load(&mut self, until: Option<String>) {
        let loaded = self.state.keyspace_state.lock().scanned.len();
        self.keyspace.set_load_progress(Some(LoadProgress {
            loaded,
            seeking: until.is_some(),
        }));
        self.loading = Some(match &until {
            Some(key) => Loading::Seek(key.clone()),
            None => Loading::All,
        });
        self.send_redis_event(RedisEvent::LoadAll { until });
    }

    /// Esc on the key list: stop the load, keeping what it loaded. Returns
    /// `false` when none is running.
    pub(super) fn stop_load(&mut self) -> bool {
        if self.loading.take().is_none() {
            return false;
        }
        self.send_redis_event(RedisEvent::StopLoadAll);
        self.keyspace.set_load_progress(None);
        self.refresh_page();
        true
    }

    /// The load stored more keys: update the counter and the page info, and
    /// redraw the page if it holds other keys now (a sorted list).
    pub(super) fn load_progress(&mut self, loaded: usize) {
        let Some(loading) = &self.loading else {
            return;
        };
        self.keyspace.set_load_progress(Some(LoadProgress {
            loaded,
            seeking: matches!(loading, Loading::Seek(_)),
        }));
        self.meta_backfilled();
    }

    /// The load stopped by itself. A search that found its key goes there;
    /// otherwise the page is fetched again with the new counts.
    pub(super) fn load_done(&mut self, complete: bool) {
        let Some(loading) = self.loading.take() else {
            return;
        };
        self.keyspace.set_load_progress(None);
        let now = self.clock.now();
        match loading {
            Loading::All => {
                if complete {
                    let loaded = self.state.keyspace_state.lock().scanned.len();
                    self.toast
                        .show(format!("Loaded all {loaded} keys"), false, now);
                }
                self.refresh_page();
            }
            Loading::Seek(key) => {
                if self.state.keyspace_state.lock().index_of(&key).is_some() {
                    self.reveal_key(key);
                    return;
                }
                if complete {
                    self.toast
                        .show(format!("{key} no longer exists"), false, now);
                }
                self.refresh_page();
            }
        }
    }

    /// The DB changed: the runner dropped the load, and the open key
    /// belongs to the old DB.
    pub(super) fn forget_load_and_open_key(&mut self) {
        self.loading = None;
        self.open_key = None;
        self.keyspace.set_load_progress(None);
    }

    /// Fetch the page again, keeping the cursor on its key.
    fn refresh_page(&mut self) {
        self.reselect_key = self.keyspace.selected_key().map(|(key, _)| key);
        self.fetch_keys();
    }

    /// Go to the page that shows `key` (opening its tree groups) and put
    /// the cursor on it.
    fn reveal_key(&mut self, key: String) {
        self.state.keyspace_state.lock().reveal(key.clone());
        self.reselect_key = Some(key);
        self.fetch_keys();
    }

    /// Enter opened a value: remember its key for `g o`.
    pub(super) fn remember_open_key(&mut self) {
        if self.keyspace.is_value_focused() || self.keyspace.is_text_focused() {
            self.open_key = self.keyspace.selected_key();
        }
    }

    /// `g o`: back to the key last opened with Enter. A loaded key is shown
    /// at once; one a filter hides asks to clear the filter first; any other
    /// is searched for in the background (Esc stops).
    pub(super) fn go_to_open_key(&mut self) {
        let Some((key, r_type)) = self.open_key.clone() else {
            let now = self.clock.now();
            self.toast
                .show("No value opened yet (Enter opens one)", false, now);
            return;
        };
        let (loaded, hidden) = {
            let keyspace = self.state.keyspace_state.lock();
            let hidden_by_pattern = keyspace
                .pattern
                .as_deref()
                .is_some_and(|pattern| !glob_match(pattern, &key));
            (
                keyspace.index_of(&key).is_some(),
                hidden_by_pattern || !keyspace.type_filter().matches(r_type.name()),
            )
        };
        if loaded {
            self.reveal_key(key);
        } else if hidden {
            self.pending = Some(PendingConfirm::ClearFilterFor(key));
        } else {
            self.start_load(Some(key));
        }
    }

    /// `y` on "clear the filter": drop the pattern and the type filter, then
    /// search the new sweep for `key`.
    pub(super) fn clear_filter_for(&mut self, key: String) {
        {
            let mut keyspace = self.state.keyspace_state.lock();
            keyspace.set_type_filter(TypeFilter::default());
            keyspace.delete_pattern();
        }
        self.fetch_keys();
        self.start_load(Some(key));
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers};
    use tokio::sync::broadcast;

    use super::Loading;
    use crate::{
        action::Action,
        app::{confirm::PendingConfirm, tests::app_with_channels, App},
        redis_client::{
            event::RedisEvent,
            types::{
                KeyMeta, KeyValue, LoadedValue, RedisInfo, RedisType, TypeFilter, ValuePageInfo,
            },
        },
        widgets::keyspace::LoadProgress,
    };

    fn drain(events: &mut broadcast::Receiver<RedisEvent>) -> Vec<RedisEvent> {
        std::iter::from_fn(|| events.try_recv().ok()).collect()
    }

    fn meta(key: &str) -> KeyMeta {
        KeyMeta {
            key: key.into(),
            r_type: RedisType::Hash,
            size: 10,
            ttl: -1,
            value: KeyValue::Unknown,
        }
    }

    /// An app whose sweep holds `keys` (one page, more to come).
    fn app_on(keys: &[&str]) -> (App, broadcast::Receiver<RedisEvent>) {
        let (mut app, _, events) = app_with_channels();
        app.state
            .keyspace_state
            .lock()
            .merge_batch(7, keys.iter().map(|&k| k.to_owned()).collect());
        *app.state.keys.lock() = Some(keys.iter().map(|&k| meta(k)).collect());
        app.load_new_keys();
        (app, events)
    }

    fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
        if let Some(action) = app.handle_key_event(crossterm::event::KeyEvent::new(code, modifiers))
        {
            app.update(&action);
        }
    }

    fn info_with_keys(keys: u64) -> RedisInfo {
        let info = std::collections::HashMap::from([("db0", format!("keys={keys},expires=0"))]);
        RedisInfo::from(
            info.iter()
                .map(|(k, v)| (*k, v.as_str()))
                .collect::<std::collections::HashMap<_, _>>(),
        )
    }

    #[test]
    fn shift_a_loads_and_esc_stops() {
        let (mut app, mut events) = app_on(&["a", "b"]);
        drain(&mut events);
        press(&mut app, KeyCode::Char('A'), KeyModifiers::SHIFT);
        assert_eq!(app.loading, Some(Loading::All));
        assert!(matches!(
            drain(&mut events).as_slice(),
            [RedisEvent::LoadAll { until: None }]
        ));
        assert_eq!(
            app.keyspace.load_progress(),
            Some(&LoadProgress {
                loaded: 2,
                seeking: false
            })
        );

        app.update(&Action::LoadAllProgress(1500));
        assert_eq!(app.keyspace.load_progress().map(|p| p.loaded), Some(1500));

        press(&mut app, KeyCode::Esc, KeyModifiers::empty());
        assert_eq!(app.loading, None);
        assert_eq!(app.keyspace.load_progress(), None);
        let sent = drain(&mut events);
        assert!(matches!(sent[0], RedisEvent::StopLoadAll), "{sent:?}");
        assert!(matches!(sent[1], RedisEvent::FetchKeys), "{sent:?}");
        // A late Done from the stopped task changes nothing.
        app.update(&Action::LoadAllDone { complete: true });
        assert!(drain(&mut events).is_empty());
    }

    #[test]
    fn shift_a_asks_first_above_two_million_keys() {
        let (mut app, mut events) = app_on(&["user:0001"]);
        drain(&mut events);
        *app.state.info.lock() = Some(info_with_keys(3_000_000));
        press(&mut app, KeyCode::Char('A'), KeyModifiers::SHIFT);
        let Some(PendingConfirm::LoadAll { keys, bytes, .. }) = &app.pending else {
            panic!("no question: {:?}", app.pending);
        };
        assert_eq!(*keys, 3_000_000);
        // (83 + 9) B per key.
        assert_eq!(*bytes, 3_000_000 * 92);
        assert!(drain(&mut events).is_empty(), "nothing loads before y");

        press(&mut app, KeyCode::Char('y'), KeyModifiers::empty());
        assert_eq!(app.loading, Some(Loading::All));
        assert!(matches!(
            drain(&mut events).as_slice(),
            [RedisEvent::LoadAll { until: None }]
        ));
    }

    #[test]
    fn go_to_open_key_reveals_a_loaded_key() {
        let (mut app, mut events) = app_on(&["a", "b", "c"]);
        assert!(app.keyspace.select_key_by_name("b"));
        app.keyspace.set_selected_value(Some(LoadedValue {
            value: KeyValue::Hash(vec![("f".into(), "v".into())]),
            page: ValuePageInfo {
                start: 0,
                len: 1,
                total: Some(1),
                has_next: false,
                has_prev: false,
            },
        }));
        app.update(&Action::EnterValue);
        assert_eq!(app.open_key, Some(("b".into(), RedisType::Hash)));
        app.keyspace.exit_value_focus();
        assert!(app.keyspace.select_key_by_name("c"));
        drain(&mut events);

        press(&mut app, KeyCode::Char('g'), KeyModifiers::empty());
        press(&mut app, KeyCode::Char('o'), KeyModifiers::empty());
        assert_eq!(app.reselect_key.as_deref(), Some("b"));
        assert!(matches!(
            drain(&mut events).as_slice(),
            [RedisEvent::FetchKeys]
        ));
    }

    #[test]
    fn go_to_open_key_searches_or_asks_to_clear_the_filter() {
        let (mut app, mut events) = app_on(&["user:1"]);
        app.open_key = Some(("user:9".into(), RedisType::Hash));
        drain(&mut events);

        // Not loaded, not hidden: search for it.
        app.go_to_open_key();
        assert_eq!(app.loading, Some(Loading::Seek("user:9".into())));
        assert!(matches!(
            drain(&mut events).as_slice(),
            [RedisEvent::LoadAll { until: Some(k) }] if k == "user:9"
        ));
        // Found: go there.
        app.state
            .keyspace_state
            .lock()
            .merge_batch(0, vec!["user:9".into()]);
        app.update(&Action::LoadAllDone { complete: false });
        assert_eq!(app.loading, None);
        assert_eq!(app.reselect_key.as_deref(), Some("user:9"));

        // Hidden by the type filter: ask first, then clear and search.
        app.state
            .keyspace_state
            .lock()
            .set_type_filter(TypeFilter::new(["string"]));
        drain(&mut events);
        app.go_to_open_key();
        assert!(matches!(
            app.pending,
            Some(PendingConfirm::ClearFilterFor(_))
        ));
        press(&mut app, KeyCode::Char('y'), KeyModifiers::empty());
        assert!(app.state.keyspace_state.lock().type_filter().is_empty());
        let sent = drain(&mut events);
        assert!(matches!(sent[0], RedisEvent::FetchKeys), "{sent:?}");
        assert!(
            matches!(&sent[1], RedisEvent::LoadAll { until: Some(k) } if k == "user:9"),
            "{sent:?}"
        );
    }

    #[test]
    fn a_pattern_that_does_not_match_hides_the_key() {
        let (mut app, _events) = app_on(&["user:1"]);
        app.state
            .keyspace_state
            .lock()
            .set_pattern(Some("order:*".into()));
        app.open_key = Some(("user:1".into(), RedisType::Hash));
        app.go_to_open_key();
        assert!(matches!(
            app.pending,
            Some(PendingConfirm::ClearFilterFor(_))
        ));
    }

    #[test]
    fn a_db_switch_forgets_the_open_key_and_the_load() {
        let (mut app, _events) = app_on(&["a"]);
        app.open_key = Some(("a".into(), RedisType::Hash));
        app.start_load(None);
        app.update(&Action::DbSwitched);
        assert_eq!(app.loading, None);
        assert_eq!(app.open_key, None);
        assert_eq!(app.keyspace.load_progress(), None);
    }
}
