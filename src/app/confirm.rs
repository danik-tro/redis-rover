//! Yes / no questions the App asks itself, drawn above every mode: the
//! command preview before a write (`safety.preview`), lifting read-only
//! on Prod, loading a large keyspace and clearing a filter for `g o`.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};

use ratatui_textarea::TextArea;

use super::App;
use crate::{
    action::Action,
    redis_client::event::RedisEvent,
    target::Environment,
    widgets::{
        confirm::{typed_input, ConfirmPopup},
        keyspace::input_text,
    },
};

/// A modal question; `y` accepts, `n` / Esc cancels.
#[derive(Debug)]
pub(super) enum PendingConfirm {
    /// Allow writes on Prod.
    LiftReadOnly,
    /// Send a write once its commands were seen.
    Write(Box<PendingWrite>),
    /// `Shift-a` on a large DB: `keys` keys, about `bytes` once loaded.
    LoadAll { db: i64, keys: u64, bytes: u64 },
    /// `g o`: the filter hides the open key; clear it and go there.
    ClearFilterFor(String),
}

/// A write waiting for `y`, with what to do once it is sent.
#[derive(Debug)]
pub(super) struct PendingWrite {
    event: RedisEvent,
    /// `event.preview()`, built once.
    lines: Vec<String>,
    after: AfterWrite,
    /// On Prod, deleting a key needs its name typed first.
    typed: Option<TypedConfirm>,
}

/// Key names longer than this are confirmed by typing `delete` instead.
const TYPED_NAME_MAX: usize = 40;

/// The typed confirm: Enter runs the write only once `input` holds
/// `expected`. Every other key goes into the input (so `y` is a letter).
#[derive(Debug)]
pub(super) struct TypedConfirm {
    expected: String,
    input: TextArea<'static>,
}

impl TypedConfirm {
    /// The typed confirm `event` needs on `env`: deleting a key on Prod.
    fn for_event(event: &RedisEvent, env: Environment) -> Option<Self> {
        let RedisEvent::DeleteKey { key } = event else {
            return None;
        };
        if env != Environment::Prod {
            return None;
        }
        let expected = if key.chars().count() > TYPED_NAME_MAX {
            "delete".to_owned()
        } else {
            key.clone()
        };
        Some(Self {
            input: typed_input(&expected),
            expected,
        })
    }

    fn matches(&self) -> bool {
        input_text(&self.input) == self.expected
    }
}

/// Cursor bookkeeping a write applies only when it is sent, so a cancelled
/// preview leaves nothing behind.
#[derive(Debug, Default)]
pub(super) struct AfterWrite {
    pub reselect: Reselect,
    /// Sets `App::restore_value_row`.
    pub restore_value_row: Option<usize>,
}

/// What a sent write does to `App::reselect_key`.
#[derive(Debug, Default)]
pub(super) enum Reselect {
    /// Leave it (publish).
    #[default]
    Keep,
    /// The key is gone: land on whatever fills its place.
    Clear,
    /// Put the cursor back on this key after the refresh.
    Key(String),
}

impl AfterWrite {
    /// Keep the cursor on `key` after the refresh.
    pub(super) fn reselect(key: &str) -> Self {
        Self {
            reselect: Reselect::Key(key.to_owned()),
            restore_value_row: None,
        }
    }

    /// Keep the cursor on `key`, and inside its value on `row`.
    pub(super) fn reselect_row(key: &str, row: usize) -> Self {
        Self {
            reselect: Reselect::Key(key.to_owned()),
            restore_value_row: Some(row),
        }
    }
}

impl App {
    /// Whether `event` shows its commands first (`safety.preview`; always
    /// on Prod).
    pub(super) fn previews(&self, event: &RedisEvent) -> bool {
        self.preview.on(self.environment).previews(event)
    }

    /// Every write goes through here: shown in the preview popup when the
    /// policy says so, sent at once otherwise.
    pub(super) fn submit_write(&mut self, event: RedisEvent, after: AfterWrite) {
        let lines = event.preview();
        if lines.is_empty() || !self.previews(&event) {
            // Nothing to show (a no-op edit) or no preview wanted.
            self.send_write(event, after);
            return;
        }
        let typed = TypedConfirm::for_event(&event, self.environment);
        self.pending = Some(PendingConfirm::Write(Box::new(PendingWrite {
            event,
            lines,
            after,
            typed,
        })));
    }

    fn send_write(&mut self, event: RedisEvent, after: AfterWrite) {
        match after.reselect {
            Reselect::Keep => {}
            Reselect::Clear => self.reselect_key = None,
            Reselect::Key(key) => self.reselect_key = Some(key),
        }
        if let Some(row) = after.restore_value_row {
            self.restore_value_row = Some(row);
        }
        self.send_redis_event(event);
    }

    /// `R`: turn read-only on at once; lifting it on Prod asks first.
    pub(super) fn toggle_read_only(&mut self, now: Instant) {
        if !self.is_read_only() {
            self.set_read_only(true);
            self.toast
                .show("Read-only on: writes are disabled", false, now);
        } else if self.environment == Environment::Prod {
            self.pending = Some(PendingConfirm::LiftReadOnly);
        } else {
            self.lift_read_only(now);
        }
    }

    fn lift_read_only(&mut self, now: Instant) {
        self.set_read_only(false);
        self.toast
            .show("Read-only off: writes are enabled", false, now);
    }

    /// Keys while a question is open. Popup-local like Enter / Esc in the
    /// other popups: `y` accepts, `n` / Esc cancels, the quit binding quits.
    pub(super) fn handle_confirm_key(&mut self, key: KeyEvent) -> Option<Action> {
        if matches!(&self.pending, Some(PendingConfirm::Write(w)) if w.typed.is_some()) {
            return self.handle_typed_key(key);
        }
        let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
        match key.code {
            KeyCode::Char('y') if plain => {
                self.accept_confirm();
                None
            }
            KeyCode::Char('n') if plain => {
                self.pending = None;
                None
            }
            KeyCode::Esc => {
                self.pending = None;
                None
            }
            _ if Self::is_quit_key(key) => Some(Action::Quit),
            _ => None,
        }
    }

    /// Keys in the typed confirm: Enter deletes once the text matches, Esc
    /// cancels, the quit binding quits, everything else is typed.
    fn handle_typed_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.code == KeyCode::Esc {
            self.pending = None;
            return None;
        }
        if Self::is_quit_key(key) {
            return Some(Action::Quit);
        }
        let Some(PendingConfirm::Write(write)) = &mut self.pending else {
            return None;
        };
        let Some(typed) = &mut write.typed else {
            return None;
        };
        if key.code == KeyCode::Enter {
            if typed.matches() {
                self.accept_confirm();
            }
            return None;
        }
        typed.input.input(key);
        None
    }

    fn accept_confirm(&mut self) {
        let now = self.clock.now();
        match self.pending.take() {
            Some(PendingConfirm::LiftReadOnly) => self.lift_read_only(now),
            Some(PendingConfirm::LoadAll { .. }) => self.start_load(None),
            Some(PendingConfirm::ClearFilterFor(key)) => self.clear_filter_for(key),
            Some(PendingConfirm::Write(write)) => {
                let PendingWrite { event, after, .. } = *write;
                self.send_write(event, after);
            }
            None => {}
        }
    }

    /// Draw the open question, centred in `area`.
    pub(super) fn render_confirm(&self, area: Rect, buf: &mut Buffer) {
        let env = self.environment;
        match &self.pending {
            Some(PendingConfirm::LiftReadOnly) => {
                ConfirmPopup::lift_read_only(env).render(area, buf);
            }
            Some(PendingConfirm::Write(write)) => match &write.typed {
                Some(typed) => ConfirmPopup::typed_delete(
                    &write.lines,
                    env,
                    &typed.expected,
                    &typed.input,
                    typed.matches(),
                )
                .render(area, buf),
                None => ConfirmPopup::write_preview(&write.lines, env).render(area, buf),
            },
            Some(PendingConfirm::LoadAll { db, keys, bytes }) => {
                ConfirmPopup::load_all(*db, *keys, *bytes, env).render(area, buf);
            }
            Some(PendingConfirm::ClearFilterFor(key)) => {
                ConfirmPopup::clear_filter_for(key, env).render(area, buf);
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent};
    use tokio::sync::broadcast;

    use super::PendingConfirm;
    use crate::{
        action::Action,
        app::{
            tests::{app, app_with_channels, key},
            App,
        },
        config::PreviewPolicy,
        redis_client::{
            event::RedisEvent,
            types::{
                ItemDelete, ItemEdit, KeyMeta, KeyValue, LoadedValue, RedisType, ValuePageInfo,
            },
        },
        target::Environment,
    };

    /// A key press as the event loop handles it: the key, then its action.
    fn press(app: &mut App, k: KeyEvent) {
        if let Some(action) = app.handle_key_event(k) {
            app.update(&action);
        }
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, key(KeyCode::Char(c)));
        }
    }

    /// An app on the hash `user:1` = {name: Bob}, drilled into its value.
    fn on_hash(env: Environment, policy: PreviewPolicy) -> (App, broadcast::Receiver<RedisEvent>) {
        let (app, _, events) = app_with_channels();
        let mut app = app.environment(env);
        app.preview = policy;
        *app.state.keys.lock() = Some(vec![KeyMeta {
            key: "user:1".into(),
            r_type: RedisType::Hash,
            size: 10,
            ttl: -1,
            value: KeyValue::Unknown,
        }]);
        app.load_new_keys();
        assert!(app.keyspace.select_key_by_name("user:1"));
        app.keyspace.set_selected_value(Some(LoadedValue {
            value: KeyValue::Hash(vec![("name".into(), "Bob".into())]),
            page: ValuePageInfo {
                start: 0,
                len: 1,
                total: Some(1),
                has_next: false,
                has_prev: false,
            },
        }));
        app.keyspace.enter_value();
        (app, events)
    }

    fn drain(events: &mut broadcast::Receiver<RedisEvent>) -> Vec<RedisEvent> {
        std::iter::from_fn(|| events.try_recv().ok()).collect()
    }

    fn writes(events: &mut broadcast::Receiver<RedisEvent>) -> Vec<RedisEvent> {
        drain(events)
            .into_iter()
            .filter(RedisEvent::is_write)
            .collect()
    }

    /// Edit `name` to `Ada` with `e`, then Enter.
    fn edit_name_to_ada(app: &mut App) {
        press(app, key(KeyCode::Char('e')));
        for _ in 0..3 {
            press(app, key(KeyCode::Backspace));
        }
        type_text(app, "Ada");
        press(app, key(KeyCode::Enter));
    }

    /// AC3: editing a hash field shows `HSET "user:1" "name" "Ada"`, and
    /// nothing is written before `y`.
    #[test]
    fn edit_waits_for_y_with_the_exact_command() {
        let (mut app, mut events) = on_hash(Environment::Local, PreviewPolicy::Writes);
        drain(&mut events);
        edit_name_to_ada(&mut app);
        let Some(PendingConfirm::Write(write)) = &app.pending else {
            panic!("no preview: {:?}", app.pending);
        };
        assert_eq!(write.lines, vec![r#"HSET "user:1" "name" "Ada""#]);
        assert!(writes(&mut events).is_empty(), "sent before y");

        // Keys other than y / n / Esc do nothing; Enter doesn't confirm.
        press(&mut app, key(KeyCode::Enter));
        press(&mut app, key(KeyCode::Char('d')));
        assert!(writes(&mut events).is_empty());
        assert!(app.pending.is_some());

        press(&mut app, key(KeyCode::Char('y')));
        assert!(app.pending.is_none());
        match writes(&mut events).as_slice() {
            [RedisEvent::EditItem {
                key,
                edit: ItemEdit::HashSet { field, value },
            }] => assert_eq!(
                (key.as_str(), field.as_str(), value.as_str()),
                ("user:1", "name", "Ada")
            ),
            other => panic!("expected one HSET edit, got {other:?}"),
        }
        assert_eq!(app.reselect_key.as_deref(), Some("user:1"));
        assert_eq!(app.restore_value_row, Some(0));
    }

    #[test]
    fn n_and_esc_cancel_without_side_effects() {
        for cancel in [KeyCode::Char('n'), KeyCode::Esc] {
            let (mut app, mut events) = on_hash(Environment::Local, PreviewPolicy::Writes);
            drain(&mut events);
            edit_name_to_ada(&mut app);
            press(&mut app, key(cancel));
            assert!(app.pending.is_none(), "{cancel:?}");
            assert!(writes(&mut events).is_empty(), "{cancel:?}");
            assert_eq!(app.reselect_key, None, "{cancel:?}: no stale reselect");
            assert_eq!(app.restore_value_row, None, "{cancel:?}");
        }
    }

    #[test]
    fn policy_off_sends_at_once_but_prod_still_previews() {
        let (mut app, mut events) = on_hash(Environment::Local, PreviewPolicy::Off);
        drain(&mut events);
        edit_name_to_ada(&mut app);
        assert!(app.pending.is_none());
        assert_eq!(writes(&mut events).len(), 1);

        let (mut app, mut events) = on_hash(Environment::Prod, PreviewPolicy::Off);
        drain(&mut events);
        edit_name_to_ada(&mut app);
        assert!(matches!(app.pending, Some(PendingConfirm::Write(_))));
        assert!(writes(&mut events).is_empty());

        // "destructive": the edit goes straight through.
        let (mut app, mut events) = on_hash(Environment::Staging, PreviewPolicy::Destructive);
        drain(&mut events);
        edit_name_to_ada(&mut app);
        assert!(app.pending.is_none());
        assert_eq!(writes(&mut events).len(), 1);
    }

    /// With a preview, `d` goes straight to it (one confirmation, not two).
    /// With `off`, the old "Delete item?" box asks instead.
    #[test]
    fn delete_previews_instead_of_the_old_box() {
        let (mut app, mut events) = on_hash(Environment::Local, PreviewPolicy::Destructive);
        drain(&mut events);
        press(&mut app, key(KeyCode::Char('d')));
        assert!(!app.keyspace.is_overlay_open());
        let Some(PendingConfirm::Write(write)) = &app.pending else {
            panic!("no preview");
        };
        assert_eq!(write.lines, vec![r#"HDEL "user:1" "name""#]);
        press(&mut app, key(KeyCode::Char('y')));
        assert!(matches!(
            writes(&mut events).as_slice(),
            [RedisEvent::DeleteItem {
                delete: ItemDelete::HashField(f),
                ..
            }] if f == "name"
        ));

        let (mut app, mut events) = on_hash(Environment::Local, PreviewPolicy::Off);
        drain(&mut events);
        press(&mut app, key(KeyCode::Char('d')));
        assert!(app.pending.is_none());
        assert!(app.keyspace.is_overlay_open(), "the old confirm box");
        press(&mut app, key(KeyCode::Enter));
        assert_eq!(writes(&mut events).len(), 1);
    }

    /// An app on the key list with `name` selected.
    fn on_key(env: Environment, name: &str) -> (App, broadcast::Receiver<RedisEvent>) {
        let (app, _, mut events) = app_with_channels();
        let mut app = app.environment(env);
        *app.state.keys.lock() = Some(vec![crate::app::tests::meta(name)]);
        app.load_new_keys();
        assert!(app.keyspace.select_key_by_name(name));
        drain(&mut events);
        (app, events)
    }

    /// AC4: on Prod, `d` needs the key name typed; a wrong name keeps
    /// Enter (and `y`, which is just a letter here) from deleting.
    #[test]
    fn prod_delete_needs_the_name_typed() {
        let (mut app, mut events) = on_key(Environment::Prod, "orders:2024");
        press(&mut app, key(KeyCode::Char('d')));
        let Some(PendingConfirm::Write(write)) = &app.pending else {
            panic!("no confirm");
        };
        assert_eq!(
            write.typed.as_ref().map(|t| t.expected.as_str()),
            Some("orders:2024")
        );

        // `y` is typed, not a confirm; Enter on a wrong name does nothing.
        press(&mut app, key(KeyCode::Char('y')));
        press(&mut app, key(KeyCode::Enter));
        assert!(writes(&mut events).is_empty());
        assert!(app.pending.is_some());
        press(&mut app, key(KeyCode::Backspace));
        type_text(&mut app, "orders:2023");
        press(&mut app, key(KeyCode::Enter));
        assert!(writes(&mut events).is_empty(), "wrong name deleted");

        press(&mut app, key(KeyCode::Backspace));
        press(&mut app, key(KeyCode::Char('4')));
        press(&mut app, key(KeyCode::Enter));
        assert!(app.pending.is_none());
        assert!(matches!(
            writes(&mut events).as_slice(),
            [RedisEvent::DeleteKey { key }] if key == "orders:2024"
        ));
    }

    #[test]
    fn long_names_are_confirmed_with_delete_and_esc_cancels() {
        let long = format!("cache:{}", "x".repeat(40));
        let (mut app, mut events) = on_key(Environment::Prod, &long);
        press(&mut app, key(KeyCode::Char('d')));
        type_text(&mut app, "delete");
        press(&mut app, key(KeyCode::Esc));
        assert!(app.pending.is_none());
        assert!(writes(&mut events).is_empty());

        press(&mut app, key(KeyCode::Char('d')));
        type_text(&mut app, "delete");
        press(&mut app, key(KeyCode::Enter));
        assert_eq!(writes(&mut events).len(), 1);
    }

    /// Local, staging and unknown never ask for typing: `y` deletes.
    #[test]
    fn typed_confirm_is_prod_only() {
        for env in [
            Environment::Local,
            Environment::Staging,
            Environment::Unknown,
        ] {
            let (mut app, mut events) = on_key(env, "orders:2024");
            press(&mut app, key(KeyCode::Char('d')));
            let Some(PendingConfirm::Write(write)) = &app.pending else {
                panic!("{env:?}: no preview");
            };
            assert!(write.typed.is_none(), "{env:?}");
            press(&mut app, key(KeyCode::Char('y')));
            assert_eq!(writes(&mut events).len(), 1, "{env:?}");
        }
    }

    #[test]
    fn read_only_never_reaches_the_preview() {
        let (app, mut events) = on_hash(Environment::Prod, PreviewPolicy::Writes);
        let mut app = app.read_only(true);
        drain(&mut events);
        press(&mut app, key(KeyCode::Char('d')));
        press(&mut app, key(KeyCode::Char('e')));
        assert!(app.pending.is_none());
        assert!(writes(&mut events).is_empty());
    }

    #[test]
    fn toggle_flips_read_only_and_the_hint() {
        let mut app = app();
        let writable_hint = app.summary.hint.clone();
        app.update(&Action::ToggleReadOnly);
        assert!(app.is_read_only());
        assert!(app.summary.read_only);
        assert_ne!(app.summary.hint, writable_hint, "write keys leave the hint");
        app.update(&Action::ToggleReadOnly);
        assert!(!app.is_read_only());
        assert_eq!(app.summary.hint, writable_hint);
    }

    #[test]
    fn lifting_read_only_on_prod_asks_first() {
        let mut app = app().environment(Environment::Prod).read_only(true);
        app.update(&Action::ToggleReadOnly);
        assert!(app.is_read_only(), "not before the answer");
        assert!(matches!(app.pending, Some(PendingConfirm::LiftReadOnly)));

        // `n` keeps it read-only.
        assert_eq!(app.handle_key_event(key(KeyCode::Char('n'))), None);
        assert!(app.pending.is_none());
        assert!(app.is_read_only());

        // Other keys are swallowed; `y` lifts it.
        app.update(&Action::ToggleReadOnly);
        assert_eq!(app.handle_key_event(key(KeyCode::Char('d'))), None);
        assert!(app.is_read_only());
        app.handle_key_event(key(KeyCode::Char('y')));
        assert!(!app.is_read_only());

        // Turning it back on never asks.
        app.update(&Action::ToggleReadOnly);
        assert!(app.pending.is_none());
        assert!(app.is_read_only());
    }

    #[test]
    fn toggle_moves_the_shared_flag() {
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mut app = app().read_only_flag(flag.clone());
        app.update(&Action::ToggleReadOnly);
        assert!(!flag.load(std::sync::atomic::Ordering::Acquire));
    }
}
