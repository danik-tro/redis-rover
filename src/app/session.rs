//! Remembering where the user was: saved on quit and every 30 s when it
//! changed, restored once the first page has loaded. Restoring only reads.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use super::{load::LOAD_ALL_ASK_ABOVE, App};
use crate::{
    action::{self, Action},
    config,
    redis_client::{bytes::shown, pubsub_store::Target, types::TypeFilter},
    session::{self, SavedSub, SessionState},
};

/// How often a changed session is written.
const SAVE_INTERVAL: Duration = Duration::from_secs(30);

/// How far the restore has got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Waiting for the first page.
    FirstPage,
    /// The saved DB was asked for; waiting for its first page.
    Db,
    /// The filter, sort and tree were applied; waiting for their page.
    View,
    /// Searching the sweep for the saved key.
    Seek,
    /// Nothing left to restore: saving is on.
    Done,
}

/// The App's side of the session file.
#[derive(Debug)]
pub(super) struct SessionLink {
    path: PathBuf,
    key: String,
    /// What to restore, then what was last written.
    saved: SessionState,
    stage: Stage,
    /// The DB wasn't asked for on the command line, URL or profile.
    restore_db: bool,
    last_check: Option<Instant>,
}

/// The loaded name closest to `target`: the first one after it, or else the
/// last one before it.
fn nearest_name<'a>(names: impl Iterator<Item = &'a str>, target: &str) -> Option<&'a str> {
    let (mut after, mut before): (Option<&str>, Option<&str>) = (None, None);
    for name in names {
        if name >= target {
            after = Some(after.map_or(name, |best| best.min(name)));
        } else {
            before = Some(before.map_or(name, |best| best.max(name)));
        }
    }
    after.or(before)
}

impl App {
    /// Save to and restore from the session file at `path`, under `key`.
    /// Without this call (`--no-restore`, `ui.restore = false`) the file is
    /// neither read nor written.
    #[must_use]
    pub fn session(mut self, path: PathBuf, key: String, restore_db: bool) -> Self {
        let saved = session::load(&path)
            .targets
            .remove(&key)
            .unwrap_or_default();
        self.session = Some(SessionLink {
            path,
            key,
            saved,
            stage: Stage::FirstPage,
            restore_db,
            last_check: None,
        });
        self
    }

    fn session_stage(&self) -> Stage {
        self.session.as_ref().map_or(Stage::Done, |link| link.stage)
    }

    fn set_session_stage(&mut self, stage: Stage) {
        if let Some(link) = &mut self.session {
            link.stage = stage;
        }
    }

    /// A page landed: take the next step of the restore. Pages still on
    /// their way would replace what this step shows, so it waits for them.
    pub(super) fn advance_restore(&mut self) {
        if self.keys_in_flight > 0 {
            return;
        }
        match self.session_stage() {
            Stage::FirstPage => self.restore_db(),
            Stage::Db => self.restore_view(),
            Stage::View => self.restore_key(),
            Stage::Seek | Stage::Done => {}
        }
    }

    /// Go back to the saved DB, unless this launch named one.
    fn restore_db(&mut self) {
        let Some(link) = &self.session else {
            return;
        };
        let db = link.saved.db.filter(|_| link.restore_db);
        let current = *self.state.db.lock();
        let count = i64::try_from(*self.state.db_count.lock()).unwrap_or(i64::MAX);
        match db {
            Some(db) if db != current && (0..count).contains(&db) && !self.cluster_enabled() => {
                self.set_session_stage(Stage::Db);
                self.switch_db(db);
            }
            _ => self.restore_view(),
        }
    }

    /// Filter, types, sort, tree and subscriptions. Fetches a page only if
    /// one of them changes what the list shows.
    fn restore_view(&mut self) {
        let Some(saved) = self.session.as_ref().map(|link| link.saved.clone()) else {
            return;
        };
        let separator = config::get().keyspace.separator.clone();
        let (changed, needs_meta) = {
            let mut keyspace = self.state.keyspace_state.lock();
            let mut changed = false;
            if keyspace.pattern != saved.filter {
                keyspace.set_pattern(saved.filter.clone());
                changed = true;
            }
            changed |= keyspace.set_type_filter(TypeFilter::new(&saved.types));
            let sort = saved.sort.map(Into::into);
            if keyspace.sort() != sort {
                keyspace.set_sort(sort);
                changed = true;
            }
            if saved.tree {
                keyspace.restore_tree(&separator, &saved.expanded);
                changed = true;
            }
            (changed, sort.is_some_and(|sort| sort.key.needs_meta()))
        };
        for sub in saved.subscriptions {
            self.subscribe_target(Target::from(sub));
        }
        if changed {
            self.set_session_stage(Stage::View);
            self.fetch_keys();
            if needs_meta {
                self.request_backfill();
            }
        } else {
            self.restore_key();
        }
    }

    /// Put the cursor back on the saved key, searching for it if it is not
    /// among the keys loaded so far.
    fn restore_key(&mut self) {
        let Some(key) = self
            .session
            .as_ref()
            .and_then(|link| link.saved.selected_key.clone())
        else {
            self.set_session_stage(Stage::Done);
            return;
        };
        if self.keyspace.select_key_by_name(&key) {
            self.set_session_stage(Stage::Done);
            action::try_send_action(&self.tx, Action::RequestSelectedValue);
            return;
        }
        let (loaded, sweep_done) = {
            let keyspace = self.state.keyspace_state.lock();
            (keyspace.index_of(&key).is_some(), keyspace.sweep_done)
        };
        if loaded {
            self.set_session_stage(Stage::Done);
            self.reveal_key(key);
        } else if sweep_done {
            self.set_session_stage(Stage::Done);
            self.saved_key_gone(&key);
        } else if self.db_size() > LOAD_ALL_ASK_ABOVE {
            // Not worth an unasked sweep of a huge DB.
            self.set_session_stage(Stage::Done);
            let now = self.clock.now();
            self.toast.show(
                format!("last key {} is not loaded", shown(&key)),
                false,
                now,
            );
        } else {
            self.set_session_stage(Stage::Seek);
            self.start_load(Some(key));
        }
    }

    fn db_size(&self) -> u64 {
        let db = *self.state.db.lock();
        self.state
            .info
            .lock()
            .as_ref()
            .map_or(0, |info| info.db_keys(db))
    }

    /// The search for the saved key ended. Returns whether it was running.
    pub(super) fn end_restore_seek(&mut self) -> bool {
        let seeking = self.session_stage() == Stage::Seek;
        if seeking {
            self.set_session_stage(Stage::Done);
        }
        seeking
    }

    /// The saved key no longer exists: select the nearest name and say so.
    pub(super) fn saved_key_gone(&mut self, key: &str) {
        let now = self.clock.now();
        self.toast
            .show(format!("last key {} is gone", shown(key)), false, now);
        let nearest = {
            let keyspace = self.state.keyspace_state.lock();
            nearest_name(keyspace.scanned.iter().map(String::as_str), key).map(str::to_owned)
        };
        if let Some(nearest) = nearest {
            self.reveal_key(nearest);
        }
    }

    /// What to save now. While a page loads no key is selected, so the last
    /// one stands in, unless the DB changed and it belongs to the old one.
    fn capture_session(&self, previous: &SessionState) -> SessionState {
        let db = *self.state.db.lock();
        let selected = self.keyspace.selected_key().map(|(key, _)| key);
        let selected_key = selected.or_else(|| {
            (previous.db == Some(db))
                .then(|| previous.selected_key.clone())
                .flatten()
        });
        let subscriptions = self
            .state
            .pubsub
            .lock()
            .subs
            .iter()
            .map(|sub| SavedSub::from(&sub.target))
            .collect();
        let keyspace = self.state.keyspace_state.lock();
        SessionState {
            filter: keyspace.pattern.clone(),
            types: keyspace.type_filter().names().to_vec(),
            sort: keyspace.sort().map(Into::into),
            tree: keyspace.is_tree(),
            expanded: keyspace.expanded_groups(),
            selected_key,
            db: Some(db),
            split: previous.split,
            subscriptions,
        }
    }

    /// Write the session if it changed. Before the restore has finished
    /// there is nothing of this run's to save, and writing would lose the
    /// saved one.
    pub(super) fn save_session(&mut self) {
        if self.session_stage() != Stage::Done {
            return;
        }
        let Some(previous) = self.session.as_ref().map(|link| link.saved.clone()) else {
            return;
        };
        let current = self.capture_session(&previous);
        if current == previous {
            return;
        }
        if let Some(link) = &mut self.session {
            session::save_logged(&link.path, &link.key, &current);
            link.saved = current;
        }
    }

    /// Every [`SAVE_INTERVAL`], save what changed.
    pub(super) fn session_tick(&mut self, now: Instant) {
        let Some(link) = &mut self.session else {
            return;
        };
        let last = *link.last_check.get_or_insert(now);
        if now.duration_since(last) < SAVE_INTERVAL {
            return;
        }
        link.last_check = Some(now);
        self.save_session();
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc, time::SystemTime};

    use pretty_assertions::assert_eq;
    use tokio::sync::mpsc;

    use super::*;
    use crate::{
        app::tests::{app_with_channels, meta},
        clock::FixedClock,
        redis_client::{
            event::RedisEvent,
            types::{Sort, SortKey},
        },
        session::{SavedSort, SavedSortKey, SavedSubKind},
    };

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("rrover-app-session-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self) -> PathBuf {
            session::path_in(&self.0)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const KEY: &str = "localhost:6379/0";

    fn drain(events: &mut mpsc::UnboundedReceiver<RedisEvent>) -> Vec<RedisEvent> {
        std::iter::from_fn(|| events.try_recv().ok()).collect()
    }

    /// What the runner answers to the fetches sent so far: the sweep's
    /// current page, as `names` (those the filter shows).
    fn serve(app: &mut App, names: &[&str], done: bool) {
        {
            let mut keyspace = app.state.keyspace_state.lock();
            let pattern = keyspace.pattern.clone();
            let shown: Vec<String> = names
                .iter()
                .filter(|name| {
                    pattern
                        .as_deref()
                        .is_none_or(|p| crate::redis_client::types::glob_match(p, name))
                })
                .map(|name| (*name).to_owned())
                .collect();
            keyspace.merge_batch(u64::from(!done), shown);
            keyspace.rebuild_view();
            *app.state.page_rows.lock() = keyspace.page_rows();
            *app.state.keys.lock() = Some(keyspace.page_keys().iter().map(|k| meta(k)).collect());
        }
        // One answer per request sent so far; what they trigger waits for
        // the next `serve`.
        for _ in 0..app.keys_in_flight {
            app.update(&Action::LoadKeysIntoKeySpace);
        }
    }

    fn app_at(dir: &TempDir) -> (App, mpsc::UnboundedReceiver<RedisEvent>) {
        let (app, _, events) = app_with_channels();
        (app.session(dir.file(), KEY.to_owned(), true), events)
    }

    fn start(app: &mut App, names: &[&str], done: bool) {
        app.fetch_keys();
        serve(app, names, done);
    }

    fn saved(selected: &str) -> SessionState {
        SessionState {
            filter: Some("user:*".into()),
            sort: Some(SavedSort {
                key: SavedSortKey::Name,
                descending: true,
            }),
            selected_key: Some(selected.into()),
            db: Some(0),
            ..SessionState::default()
        }
    }

    /// AC4: filter, sort and selection are back after a relaunch.
    #[test]
    fn filter_sort_and_selection_come_back() {
        let dir = TempDir::new("ac4");
        session::save(&dir.file(), KEY, &saved("user:42")).unwrap();
        let (mut app, mut events) = app_at(&dir);
        let names = ["user:1", "user:42", "order:7"];

        start(&mut app, &names, true);
        serve(&mut app, &names, true);

        let keyspace = app.state.keyspace_state.lock();
        assert_eq!(keyspace.pattern.as_deref(), Some("user:*"));
        assert_eq!(
            keyspace.sort(),
            Some(Sort {
                key: SortKey::Name,
                descending: true
            })
        );
        drop(keyspace);
        assert_eq!(app.keyspace.selected_key().unwrap().0, "user:42");
        assert!(drain(&mut events).iter().all(|e| !e.is_write()));
    }

    #[test]
    fn a_gone_key_selects_the_nearest_name_with_a_toast() {
        let dir = TempDir::new("gone");
        session::save(&dir.file(), KEY, &saved("user:5")).unwrap();
        let (mut app, mut events) = app_at(&dir);
        let names = ["user:1", "user:7", "user:9"];

        start(&mut app, &names, true);
        serve(&mut app, &names, true);
        serve(&mut app, &names, true);

        assert_eq!(app.keyspace.selected_key().unwrap().0, "user:7");
        assert_eq!(app.toast.message(), Some("last key user:5 is gone"));
        assert!(drain(&mut events).iter().all(|e| !e.is_write()));
    }

    #[test]
    fn a_key_past_the_first_page_is_searched_for() {
        let dir = TempDir::new("seek");
        session::save(&dir.file(), KEY, &saved("user:42")).unwrap();
        let (mut app, mut events) = app_at(&dir);

        start(&mut app, &["user:1"], false);
        serve(&mut app, &["user:1"], false);

        let sent = drain(&mut events);
        assert!(sent.iter().any(|e| matches!(
            e,
            RedisEvent::LoadAll { until: Some(key) } if key == "user:42"
        )));
        assert!(sent.iter().all(|e| !e.is_write()));

        // The search ends without it, at the end of the sweep.
        app.update(&Action::LoadAllDone { complete: true });

        assert_eq!(app.toast.message(), Some("last key user:42 is gone"));
        assert_eq!(app.session_stage(), Stage::Done);
    }

    #[test]
    fn the_saved_db_comes_back_unless_this_launch_named_one() {
        let dir = TempDir::new("db");
        let state = SessionState {
            db: Some(3),
            ..SessionState::default()
        };
        session::save(&dir.file(), KEY, &state).unwrap();

        let (mut app, mut events) = app_at(&dir);
        start(&mut app, &["a"], true);
        assert!(drain(&mut events)
            .iter()
            .any(|e| matches!(e, RedisEvent::SelectDb(3))));

        let (app, _, mut events) = app_with_channels();
        let mut app = app.session(dir.file(), KEY.to_owned(), false);
        start(&mut app, &["a"], true);
        assert!(!drain(&mut events)
            .iter()
            .any(|e| matches!(e, RedisEvent::SelectDb(_))));
    }

    #[test]
    fn subscriptions_come_back_by_name() {
        let dir = TempDir::new("subs");
        let state = SessionState {
            subscriptions: vec![SavedSub {
                kind: SavedSubKind::Pattern,
                name: "orders:*".into(),
            }],
            ..SessionState::default()
        };
        session::save(&dir.file(), KEY, &state).unwrap();
        let (mut app, _events) = app_at(&dir);

        start(&mut app, &["a"], true);

        let store = app.state.pubsub.lock();
        assert_eq!(store.subs.len(), 1);
        assert_eq!(store.subs[0].target, Target::Pattern("orders:*".into()));
    }

    #[test]
    fn nothing_is_saved_before_the_restore_finished() {
        let dir = TempDir::new("early");
        session::save(&dir.file(), KEY, &saved("user:42")).unwrap();
        let before = fs::read(dir.file()).unwrap();
        let (mut app, _events) = app_at(&dir);

        app.save_session();

        assert_eq!(fs::read(dir.file()).unwrap(), before);
    }

    #[test]
    fn a_changed_session_is_saved_after_30_seconds_and_on_quit() {
        let dir = TempDir::new("tick");
        let (mut app, _events) = app_at(&dir);
        let clock = Arc::new(FixedClock::new(SystemTime::UNIX_EPOCH));
        app.clock = clock.clone();
        start(&mut app, &["user:1", "user:2"], true);
        assert!(app.keyspace.select_key_by_name("user:2"));

        app.session_tick(app.clock.now());
        clock.advance(Duration::from_secs(29));
        app.session_tick(app.clock.now());
        assert!(!dir.file().exists(), "too early");

        clock.advance(Duration::from_secs(1));
        app.session_tick(app.clock.now());
        let file = session::load(&dir.file());
        assert_eq!(file.targets[KEY].selected_key.as_deref(), Some("user:2"));

        assert!(app.keyspace.select_key_by_name("user:1"));
        app.save_session();
        let file = session::load(&dir.file());
        assert_eq!(file.targets[KEY].selected_key.as_deref(), Some("user:1"));
    }

    #[test]
    fn an_unchanged_session_is_not_written_again() {
        let dir = TempDir::new("same");
        let (mut app, _events) = app_at(&dir);
        start(&mut app, &["user:1"], true);
        assert!(app.keyspace.select_key_by_name("user:1"));
        app.save_session();
        fs::remove_file(dir.file()).unwrap();

        app.save_session();

        assert!(!dir.file().exists());
    }

    #[test]
    fn without_a_session_nothing_is_touched() {
        let (mut app, _, mut events) = app_with_channels();

        start(&mut app, &["a"], true);
        app.save_session();

        assert!(drain(&mut events)
            .iter()
            .all(|e| matches!(e, RedisEvent::FetchKeys)));
    }

    #[test]
    fn the_nearest_name_is_the_next_one_or_else_the_last() {
        let names = ["user:1", "user:7", "user:9"];
        let nearest = |target| nearest_name(names.iter().copied(), target);

        assert_eq!(nearest("user:5"), Some("user:7"));
        assert_eq!(nearest("user:7"), Some("user:7"));
        assert_eq!(nearest("zzz"), Some("user:9"));
        assert_eq!(nearest_name(std::iter::empty(), "a"), None);
    }
}
