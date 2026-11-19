//! Power edits (sol 08): rename and copy a key. Every one goes through
//! [`App::submit_write`]; a taken name comes back as `Action::TargetExists`
//! and is asked about again ([`App::target_exists`]).

use super::{confirm::AfterWrite, App};
use crate::{
    redis_client::{event::RedisEvent, types::Ttl},
    utils::TtlInput,
    widgets::keyspace::OverlayRequest,
};

impl App {
    /// `F2`: rename the selected key, or inside a hash / set / zset the
    /// selected field / member (routed by focus, like `y`).
    pub(super) fn request_rename(&mut self) {
        let Some((key, _)) = self.keyspace.selected_key() else {
            return;
        };
        if self.deny_write() {
            return;
        }
        if self.keyspace.is_value_focused() {
            self.keyspace.open_rename_item(key);
        } else {
            self.keyspace.open_rename_key(key);
        }
    }

    /// `c`: copy the selected key (`Tab` in the input picks the DB).
    pub(super) fn request_copy_key(&mut self) {
        let Some((key, _)) = self.keyspace.selected_key() else {
            return;
        };
        if self.deny_write() {
            return;
        }
        self.keyspace.open_copy_key(key);
    }

    /// `Tab` in the copy input: the DB picker, choosing a target this time.
    pub(super) fn pick_copy_db(&mut self) {
        if self.cluster_enabled() {
            self.toast
                .show("Cluster mode only has db 0", false, self.clock.now());
            return;
        }
        self.open_db_picker();
        self.db_picker.set_title(" Copy to database ", "choose");
    }

    /// Enter in the DB picker opened by [`Self::pick_copy_db`].
    pub(super) fn confirm_copy_db(&mut self, selected: Option<i64>) {
        let current = *self.state.db.lock();
        self.keyspace
            .set_copy_db(selected.filter(|&db| db != current));
    }

    /// The TTL input was confirmed: a duration, a shift of the current TTL
    /// or `persist`. `0` deletes the key, so it says so above the preview.
    pub(super) fn submit_ttl(&mut self, key: String, ttl: TtlInput) {
        let after = AfterWrite::reselect(&key);
        let (event, warning) = match ttl {
            TtlInput::Set(0) => (
                RedisEvent::SetTtl {
                    key,
                    ttl: Ttl::Expire(0),
                },
                Some("a TTL of 0 deletes the key".to_owned()),
            ),
            TtlInput::Set(secs) => (
                RedisEvent::SetTtl {
                    key,
                    ttl: Ttl::Expire(secs),
                },
                None,
            ),
            TtlInput::Persist => (
                RedisEvent::SetTtl {
                    key,
                    ttl: Ttl::Persist,
                },
                None,
            ),
            TtlInput::Shift(delta_secs) => (RedisEvent::ShiftTtl { key, delta_secs }, None),
        };
        self.submit_write_warned(event, after, warning);
    }

    /// The rename / copy input was confirmed: preview the write. An empty
    /// or unchanged name does nothing.
    pub(super) fn submit_move(&mut self, request: OverlayRequest, row: usize) {
        let now = self.clock.now();
        let (event, after) = match request {
            OverlayRequest::Rename { from, to } => {
                if to.is_empty() || to == from {
                    self.toast.show("Same name: nothing to rename", false, now);
                    return;
                }
                let after = AfterWrite::reselect(&to);
                let event = RedisEvent::RenameKey {
                    from,
                    to,
                    overwrite: false,
                };
                (event, after)
            }
            OverlayRequest::Copy { from, to, db } => {
                if to.is_empty() || (to == from && db.is_none()) {
                    self.toast.show(
                        "Pick another name or another DB (Tab) to copy to",
                        false,
                        now,
                    );
                    return;
                }
                let after = if db.is_none() {
                    AfterWrite::reselect(&to)
                } else {
                    AfterWrite::default()
                };
                let event = RedisEvent::CopyKey {
                    from,
                    to,
                    db,
                    overwrite: false,
                };
                (event, after)
            }
            OverlayRequest::RenameItem {
                key,
                kind,
                from,
                to,
            } => {
                if to.is_empty() || to == from {
                    self.toast.show("Same name: nothing to rename", false, now);
                    return;
                }
                let after = AfterWrite::reselect_row(&key, row);
                let event = match kind {
                    None => RedisEvent::RenameField { key, from, to },
                    Some(kind) => RedisEvent::RenameMember {
                        key,
                        kind,
                        from,
                        to,
                    },
                };
                (event, after)
            }
            _ => return,
        };
        self.submit_write(event, after);
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent};
    use tokio::sync::mpsc;

    use crate::{
        action::Action,
        app::{
            confirm::PendingConfirm,
            tests::{app_with_channels, key, meta},
            App,
        },
        redis_client::event::RedisEvent,
        target::Environment,
    };

    fn on_key(env: Environment, name: &str) -> (App, mpsc::UnboundedReceiver<RedisEvent>) {
        let (app, _, mut events) = app_with_channels();
        let mut app = app.environment(env);
        *app.state.keys.lock() = Some(vec![meta(name)]);
        app.load_new_keys();
        assert!(app.keyspace.select_key_by_name(name));
        while events.try_recv().is_ok() {}
        (app, events)
    }

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

    fn writes(events: &mut mpsc::UnboundedReceiver<RedisEvent>) -> Vec<RedisEvent> {
        std::iter::from_fn(|| events.try_recv().ok())
            .filter(RedisEvent::is_write)
            .collect()
    }

    fn preview(app: &App) -> (Vec<String>, Option<String>) {
        match &app.pending {
            Some(PendingConfirm::Write(write)) => write.shown(),
            other => panic!("no write preview open: {other:?}"),
        }
    }

    /// AC1 in the UI: F2 opens the name for editing, Enter previews
    /// `RENAMENX`, `y` sends it and the cursor follows the new name.
    #[test]
    fn f2_renames_the_selected_key_after_a_preview() {
        let (mut app, mut events) = on_key(Environment::Staging, "user:1");
        press(&mut app, key(KeyCode::F(2)));
        assert!(app.keyspace.is_input_active());
        // Seeded with the name, cursor at the end.
        press(&mut app, key(KeyCode::Backspace));
        type_text(&mut app, "9");
        press(&mut app, key(KeyCode::Enter));

        let (lines, warning) = preview(&app);
        assert_eq!(lines, [r#"RENAMENX "user:1" "user:9""#]);
        assert_eq!(warning, None);
        assert!(writes(&mut events).is_empty(), "nothing before y");
        press(&mut app, key(KeyCode::Char('y')));
        let sent = writes(&mut events);
        assert!(
            matches!(&sent[..], [RedisEvent::RenameKey { to, overwrite: false, .. }] if to == "user:9"),
            "{sent:?}"
        );
        assert_eq!(app.reselect_key.as_deref(), Some("user:9"));
    }

    /// AC2 in the UI: a taken name is asked about again, naming it; `n`
    /// replaces nothing, `y` sends the overwrite.
    #[test]
    fn a_taken_name_asks_again_naming_the_key() {
        let (mut app, mut events) = on_key(Environment::Staging, "user:1");
        let exists = Action::TargetExists {
            from: "user:1".into(),
            to: "user:9".into(),
            db: None,
            copy: false,
        };
        app.update(&exists);
        let (lines, warning) = preview(&app);
        assert_eq!(lines, [r#"RENAME "user:1" "user:9""#]);
        assert_eq!(
            warning.as_deref(),
            Some("user:9 already exists: this replaces it")
        );
        press(&mut app, key(KeyCode::Char('n')));
        assert!(writes(&mut events).is_empty());

        app.update(&exists);
        press(&mut app, key(KeyCode::Char('y')));
        assert!(matches!(
            &writes(&mut events)[..],
            [RedisEvent::RenameKey {
                overwrite: true,
                ..
            }]
        ));
    }

    /// The overwrite is asked even with previews off, and on Prod it needs
    /// the replaced key's name typed.
    #[test]
    fn the_overwrite_is_always_asked_and_typed_on_prod() {
        let (mut app, mut events) = on_key(Environment::Local, "a");
        app.preview = crate::config::PreviewPolicy::Off;
        app.update(&Action::TargetExists {
            from: "a".into(),
            to: "b".into(),
            db: Some(2),
            copy: true,
        });
        let (lines, warning) = preview(&app);
        assert_eq!(lines, [r#"COPY "a" "b" "DB" "2" "REPLACE""#]);
        assert_eq!(
            warning.as_deref(),
            Some("b already exists in DB 2: this replaces it")
        );
        press(&mut app, key(KeyCode::Char('y')));
        assert_eq!(writes(&mut events).len(), 1);

        let (mut app, mut events) = on_key(Environment::Prod, "a");
        app.update(&Action::TargetExists {
            from: "a".into(),
            to: "b".into(),
            db: None,
            copy: false,
        });
        assert!(app.typed_confirm_open());
        press(&mut app, key(KeyCode::Enter));
        assert!(writes(&mut events).is_empty(), "Enter before typing");
        type_text(&mut app, "b");
        press(&mut app, key(KeyCode::Enter));
        assert_eq!(writes(&mut events).len(), 1);
    }

    /// `c`, `Tab` picks DB 2 in the picker (which doesn't switch DBs), Enter
    /// previews `COPY … DB 2`.
    #[test]
    fn c_copies_to_the_db_picked_with_tab() {
        let (mut app, mut events) = on_key(Environment::Staging, "cart:1");
        press(&mut app, key(KeyCode::Char('c')));
        assert!(app.keyspace.is_copy_open());
        press(&mut app, key(KeyCode::Tab));
        assert!(app.db_picker.is_open());
        press(&mut app, key(KeyCode::Char('2')));
        press(&mut app, key(KeyCode::Enter));
        assert!(!app.db_picker.is_open());
        assert!(app.keyspace.is_copy_open(), "back in the copy input");
        assert!(writes(&mut events).is_empty());
        assert!(
            !std::iter::from_fn(|| events.try_recv().ok())
                .any(|e| matches!(e, RedisEvent::SelectDb(_))),
            "the picker didn't switch DBs"
        );

        press(&mut app, key(KeyCode::Enter));
        let (lines, _) = preview(&app);
        assert_eq!(lines, [r#"COPY "cart:1" "cart:1" "DB" "2""#]);
        press(&mut app, key(KeyCode::Char('y')));
        assert!(matches!(
            &writes(&mut events)[..],
            [RedisEvent::CopyKey { db: Some(2), .. }]
        ));
    }

    /// The same name in the same DB has nothing to copy or rename; a
    /// pasted name goes into the input.
    #[test]
    fn same_name_does_nothing_and_paste_fills_the_input() {
        let (mut app, mut events) = on_key(Environment::Local, "k");
        press(&mut app, key(KeyCode::Char('c')));
        press(&mut app, key(KeyCode::Enter));
        assert!(app.pending.is_none());
        press(&mut app, key(KeyCode::F(2)));
        press(&mut app, key(KeyCode::Enter));
        assert!(app.pending.is_none());
        assert!(writes(&mut events).is_empty());

        press(&mut app, key(KeyCode::F(2)));
        app.handle_paste(":copy\n");
        assert!(app.keyspace.is_input_active(), "the newline didn't confirm");
        press(&mut app, key(KeyCode::Enter));
        let (lines, _) = preview(&app);
        assert_eq!(lines, [r#"RENAMENX "k" "k:copy""#]);
    }

    /// AC4 in the UI: what the TTL popup sends for each kind of input.
    #[test]
    fn the_ttl_popup_takes_durations_shifts_and_persist() {
        use crate::redis_client::types::Ttl;

        let cases = [
            (
                "2h30m",
                RedisEvent::SetTtl {
                    key: "k".into(),
                    ttl: Ttl::Expire(9000),
                },
            ),
            (
                "+30m",
                RedisEvent::ShiftTtl {
                    key: "k".into(),
                    delta_secs: 1800,
                },
            ),
            (
                "-5m",
                RedisEvent::ShiftTtl {
                    key: "k".into(),
                    delta_secs: -300,
                },
            ),
            (
                "persist",
                RedisEvent::SetTtl {
                    key: "k".into(),
                    ttl: Ttl::Persist,
                },
            ),
        ];
        for (typed, want) in cases {
            let (mut app, mut events) = on_key(Environment::Local, "k");
            app.preview = crate::config::PreviewPolicy::Off;
            press(&mut app, key(KeyCode::Char('t')));
            type_text(&mut app, typed);
            press(&mut app, key(KeyCode::Enter));
            let sent = writes(&mut events);
            assert_eq!(format!("{sent:?}"), format!("[{want:?}]"), "{typed}");
        }
    }

    /// `0` says it deletes the key, even with previews off; bad input keeps
    /// the popup open and sends nothing.
    #[test]
    fn ttl_zero_warns_and_bad_input_stays_open() {
        let (mut app, mut events) = on_key(Environment::Local, "k");
        app.preview = crate::config::PreviewPolicy::Off;
        press(&mut app, key(KeyCode::Char('t')));
        type_text(&mut app, "0");
        press(&mut app, key(KeyCode::Enter));
        let (lines, warning) = preview(&app);
        assert_eq!(lines, [r#"EXPIRE "k" "0""#]);
        assert_eq!(warning.as_deref(), Some("a TTL of 0 deletes the key"));
        press(&mut app, key(KeyCode::Char('n')));

        press(&mut app, key(KeyCode::Char('t')));
        type_text(&mut app, "5x");
        press(&mut app, key(KeyCode::Enter));
        assert!(app.keyspace.is_input_active(), "still open to fix it");
        assert!(app.pending.is_none());
        press(&mut app, key(KeyCode::Backspace));
        press(&mut app, key(KeyCode::Enter));
        assert!(!app.keyspace.is_input_active());
        assert!(matches!(
            &writes(&mut events)[..],
            [RedisEvent::SetTtl { .. }]
        ));
    }

    /// An app drilled into the value of `name` (`r_type`, holding `value`).
    fn in_value(
        name: &str,
        r_type: crate::redis_client::types::RedisType,
        value: crate::redis_client::types::KeyValue,
    ) -> (App, mpsc::UnboundedReceiver<RedisEvent>) {
        use crate::redis_client::types::{KeyMeta, KeyValue, LoadedValue, ValuePageInfo};
        let (mut app, events) = on_key(Environment::Local, name);
        *app.state.keys.lock() = Some(vec![KeyMeta {
            key: name.into(),
            r_type,
            size: 10,
            ttl: -1,
            value: KeyValue::Unknown,
        }]);
        app.load_new_keys();
        assert!(app.keyspace.select_key_by_name(name));
        app.keyspace.set_selected_value(Some(LoadedValue {
            value,
            page: ValuePageInfo {
                start: 0,
                len: 1,
                total: Some(1),
                has_next: false,
                has_prev: false,
            },
        }));
        app.keyspace.enter_value();
        assert!(app.keyspace.is_value_focused());
        (app, events)
    }

    /// AC5 / AC6 in the UI: F2 inside a hash renames the field, inside a
    /// zset the member (the script keeps the score); the cursor stays in
    /// the value.
    #[test]
    fn f2_in_a_value_renames_the_field_or_member() {
        use crate::redis_client::types::{KeyValue, MemberKind, RedisType};

        let (mut app, mut events) = in_value(
            "user:1",
            RedisType::Hash,
            KeyValue::Hash(vec![("name".into(), "Ada".into())]),
        );
        press(&mut app, key(KeyCode::F(2)));
        assert!(app.keyspace.is_input_active());
        type_text(&mut app, "_full");
        press(&mut app, key(KeyCode::Enter));
        let (lines, _) = preview(&app);
        assert!(lines[0].starts_with(r#"EVAL "-- rename a hash field"#));
        press(&mut app, key(KeyCode::Char('y')));
        let sent = writes(&mut events);
        assert!(
            matches!(&sent[..], [RedisEvent::RenameField { from, to, .. }]
                if from == "name" && to == "name_full"),
            "{sent:?}"
        );
        assert_eq!(app.restore_value_row, Some(0));

        let (mut app, mut events) = in_value(
            "board",
            RedisType::Zset,
            KeyValue::Zset(vec![("ada".into(), 42.0)]),
        );
        app.preview = crate::config::PreviewPolicy::Off;
        press(&mut app, key(KeyCode::F(2)));
        type_text(&mut app, "_l");
        press(&mut app, key(KeyCode::Enter));
        let sent = writes(&mut events);
        assert!(
            matches!(&sent[..], [RedisEvent::RenameMember { kind: MemberKind::Zset, to, .. }]
                if to == "ada_l"),
            "{sent:?}"
        );
    }

    /// A list element has no name: a notice, no input.
    #[test]
    fn f2_on_a_list_element_explains() {
        use crate::redis_client::types::{KeyValue, RedisType};

        let (mut app, mut events) =
            in_value("q", RedisType::List, KeyValue::List(vec!["a".into()]));
        press(&mut app, key(KeyCode::F(2)));
        assert!(!app.keyspace.is_input_active());
        assert!(app.keyspace.is_overlay_open(), "the notice");
        assert!(writes(&mut events).is_empty());
    }

    #[test]
    fn read_only_opens_no_input() {
        let (mut app, mut events) = on_key(Environment::Local, "k");
        app.set_read_only(true);
        press(&mut app, key(KeyCode::F(2)));
        assert!(!app.keyspace.is_input_active());
        press(&mut app, key(KeyCode::Char('c')));
        assert!(!app.keyspace.is_input_active());
        assert!(writes(&mut events).is_empty());
    }
}
