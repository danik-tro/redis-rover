//! `Shift-e`: edit in `$VISUAL` / `$EDITOR` / `vi`.
//!
//! - On a STRING key: the whole value, saved as a `SetString`.
//! - On a hash / list / set / zset key: the whole value as JSON (what `y`
//!   copies), diffed on save ([`value_edit::diff`]) into one
//!   `ApplyValueEdit`.
//! - Inside a collection: the selected element (list element, hash field
//!   value, set member, zset score), saved as an `EditItem`.
//!
//! Whole values are fetched like a yank (`FetchFullValue`); the event loop
//! (which owns the terminal) suspends the TUI around
//! [`external_editor::edit`]. Every save goes through the preview.

use color_eyre::eyre::Result;

use super::{confirm::AfterWrite, App};
use crate::{
    external_editor::{self, Edited, MAX_EDIT_BYTES},
    mode::Mode,
    redis_client::{
        bytes::is_binary,
        escape::printable,
        event::RedisEvent,
        types::RedisType,
        value_edit::{self, Diff},
    },
    tui::Tui,
    widgets::keyspace::{format_bytes, ItemSlot},
};

/// What an edit writes back to.
#[derive(Debug)]
enum Target {
    /// A whole STRING value.
    String,
    /// A whole hash / list / set / zset, as JSON.
    Whole(RedisType),
    /// One element; `row` is where the cursor goes back to.
    Item { slot: ItemSlot, row: usize },
}

/// An edit waiting for the event loop, which owns the terminal.
#[derive(Debug)]
pub(super) struct EditorJob {
    key: String,
    text: String,
    target: Target,
}

impl App {
    /// `Shift-e`: the selected element inside a collection, else the whole
    /// value of the selected key (fetched first).
    pub(super) fn request_edit_external(&mut self) {
        if self.mode != Mode::KeySpace {
            return;
        }
        let Some((key, r_type)) = self.keyspace.selected_key() else {
            return;
        };
        if self.deny_write() {
            return;
        }
        if self.keyspace.is_value_focused() {
            if let Some((text, slot)) = self.keyspace.selected_item_for_editor() {
                if is_binary(&text) {
                    self.keyspace
                        .show_notice("Binary element (not valid UTF-8): can't be edited as text.");
                    return;
                }
                let row = self.keyspace.value_selected_row().unwrap_or(0);
                self.editor_job = Some(EditorJob {
                    key,
                    text,
                    target: Target::Item { slot, row },
                });
                return;
            }
        }
        if !matches!(
            r_type,
            RedisType::String
                | RedisType::Hash
                | RedisType::List
                | RedisType::Set
                | RedisType::Zset
        ) {
            self.keyspace
                .show_notice("Strings, hashes, lists, sets and zsets open in $EDITOR.");
            return;
        }
        let too_big = self
            .keyspace
            .selected_size()
            .is_some_and(|size| size > MAX_EDIT_BYTES as u128);
        if too_big {
            self.keyspace.show_notice(format!(
                "Values over {} can't be edited in $EDITOR.",
                format_bytes(MAX_EDIT_BYTES)
            ));
            return;
        }
        self.toast.show(
            format!("Opening {} in $EDITOR…", printable(&key)),
            true,
            self.clock.now(),
        );
        self.external_edit = Some((key.clone(), r_type.clone()));
        self.send_redis_event(RedisEvent::FetchFullValue { key, r_type });
    }

    /// `YankReady` for a value fetched for the editor: queue the edit.
    /// Returns `false` when the yank was a plain copy.
    pub(super) fn yank_for_editor(&mut self) -> bool {
        let Some((waiting, r_type)) = self.external_edit.take() else {
            return false;
        };
        let Some((key, text)) = self.state.yank.lock().take() else {
            return true;
        };
        let now = self.clock.now();
        if key != waiting {
            self.toast
                .show("The key changed meanwhile: not opened", false, now);
            return true;
        }
        if is_binary(&text) {
            self.toast.show(
                "Binary value (not valid UTF-8): can't be edited as text",
                false,
                now,
            );
            return true;
        }
        // Replaces the sticky "Opening…"; gone by the time the editor exits.
        self.toast.show("Opening the editor…", false, now);
        let target = if r_type == RedisType::String {
            Target::String
        } else {
            Target::Whole(r_type)
        };
        self.editor_job = Some(EditorJob { key, text, target });
        true
    }

    /// Run a queued edit: hand the terminal to the editor and take it back,
    /// then preview the change. A `SIGTERM` / `SIGHUP` meanwhile quits.
    pub(super) async fn run_editor_job(&mut self, tui: &mut Tui) -> Result<()> {
        let Some(job) = self.editor_job.take() else {
            return Ok(());
        };
        let editor = external_editor::editor_command(
            std::env::var("VISUAL").ok(),
            std::env::var("EDITOR").ok(),
        );
        tui.suspend().await?;
        let edited = external_editor::edit(&job.text, &editor).await;
        if edited == Edited::Stopped {
            self.should_quit = true;
            return Ok(());
        }
        tui.resume()?;
        self.redraw.dirty = true;
        self.save_edit(job, edited);
        Ok(())
    }

    /// What the editor left, as a previewed write (or a toast saying why
    /// not).
    fn save_edit(&mut self, job: EditorJob, edited: Edited) {
        let now = self.clock.now();
        let value = match edited {
            Edited::Changed(value) => value,
            Edited::Unchanged => return self.toast.show("No change", false, now),
            Edited::Failed(reason) => {
                return self.toast.show(format!("Not saved: {reason}"), false, now);
            }
            Edited::Stopped => return,
        };
        let EditorJob { key, text, target } = job;
        match target {
            Target::String => {
                let note = size_note(text.len(), value.len());
                let after = AfterWrite::reselect(&key);
                self.submit_write_noted(
                    RedisEvent::SetString { key, value },
                    after,
                    None,
                    vec![note],
                );
            }
            Target::Item { slot, row } => match slot.into_edit(value) {
                Ok(edit) => {
                    let after = AfterWrite::reselect_row(&key, row);
                    self.submit_write(RedisEvent::EditItem { key, edit }, after);
                }
                Err(reason) => self.toast.show(format!("Not saved: {reason}"), false, now),
            },
            Target::Whole(r_type) => match value_edit::diff(&r_type, &text, &value) {
                Ok(Diff::Same) => self.toast.show("No change", false, now),
                Ok(Diff::Changed { edit, empties }) => {
                    let warning = empties.then(|| {
                        format!("the {} ends up empty, which deletes the key", r_type.name())
                    });
                    let after = AfterWrite::reselect(&key);
                    self.submit_write_warned(
                        RedisEvent::ApplyValueEdit { key, edit },
                        after,
                        warning,
                    );
                }
                Err(reason) => self.toast.show(format!("Not saved: {reason}"), false, now),
            },
        }
    }
}

/// `size 1.2 KiB → 1.4 KiB (+212 B)`.
fn size_note(old: usize, new: usize) -> String {
    let diff = if new >= old {
        format!("+{}", format_bytes(new - old))
    } else {
        format!("−{}", format_bytes(old - new))
    };
    format!(
        "size {} → {} ({diff})",
        format_bytes(old),
        format_bytes(new)
    )
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::sync::mpsc;

    use super::{size_note, EditorJob, Target};
    use crate::external_editor::Edited;
    use crate::{
        action::Action,
        app::{
            tests::{app_with_channels, meta},
            App,
        },
        redis_client::{event::RedisEvent, types::RedisType},
        target::Environment,
    };

    fn on_key(r_type: RedisType) -> (App, mpsc::UnboundedReceiver<RedisEvent>) {
        let (app, _, mut events) = app_with_channels();
        let mut app = app.environment(Environment::Local);
        let mut row = meta("k");
        row.r_type = r_type;
        *app.state.keys.lock() = Some(vec![row]);
        app.load_new_keys();
        assert!(app.keyspace.select_key_by_name("k"));
        while events.try_recv().is_ok() {}
        (app, events)
    }

    fn shift_e(app: &mut App) {
        let k = KeyEvent::new(KeyCode::Char('E'), KeyModifiers::SHIFT);
        if let Some(action) = app.handle_key_event(k) {
            app.update(&action);
        }
    }

    fn sent(events: &mut mpsc::UnboundedReceiver<RedisEvent>) -> Vec<RedisEvent> {
        std::iter::from_fn(|| events.try_recv().ok()).collect()
    }

    /// `Shift-e` fetches the whole string; when it arrives the edit is
    /// queued for the event loop (not copied to the clipboard).
    #[test]
    fn shift_e_fetches_the_string_for_the_editor() {
        let (mut app, mut events) = on_key(RedisType::String);
        shift_e(&mut app);
        assert!(matches!(
            &sent(&mut events)[..],
            [RedisEvent::FetchFullValue { key, r_type: RedisType::String }] if key == "k"
        ));
        *app.state.yank.lock() = Some(("k".into(), "hello".into()));
        app.update(&Action::YankReady);
        let job = app.editor_job.as_ref().expect("queued");
        assert_eq!((job.key.as_str(), job.text.as_str()), ("k", "hello"));
        assert!(app.external_edit.is_none());
    }

    #[test]
    fn only_editable_types_open_and_never_read_only() {
        let (mut app, mut events) = on_key(RedisType::Json);
        shift_e(&mut app);
        assert!(app.keyspace.is_overlay_open(), "a notice");
        assert!(sent(&mut events).is_empty());

        let (mut app, mut events) = on_key(RedisType::Hash);
        app.set_read_only(true);
        shift_e(&mut app);
        assert!(sent(&mut events).is_empty());
        assert!(app.external_edit.is_none());
    }

    /// On a hash key the whole value is fetched; the saved JSON becomes one
    /// `ApplyValueEdit` with only what changed.
    #[test]
    fn a_whole_hash_is_diffed_into_one_previewed_write() {
        use crate::redis_client::value_edit::ValueEdit;

        let (mut app, mut events) = on_key(RedisType::Hash);
        shift_e(&mut app);
        assert!(matches!(
            &sent(&mut events)[..],
            [RedisEvent::FetchFullValue {
                r_type: RedisType::Hash,
                ..
            }]
        ));
        let old = "{\n  \"name\": \"Ada\",\n  \"email\": \"a@x\"\n}";
        *app.state.yank.lock() = Some(("k".into(), old.into()));
        app.update(&Action::YankReady);
        let job = app.editor_job.take().expect("queued");
        assert!(matches!(job.target, Target::Whole(RedisType::Hash)));

        app.save_edit(job, Edited::Changed(r#"{"name": "Ada L"}"#.into()));
        let (lines, warning) = preview(&app);
        assert_eq!(
            lines,
            [
                "MULTI",
                r#"HSET "k" "name" "Ada L""#,
                r#"HDEL "k" "email""#,
                "EXEC"
            ]
        );
        assert_eq!(warning, None);
        press_y(&mut app);
        assert!(matches!(
            &sent(&mut events)[..],
            [RedisEvent::ApplyValueEdit {
                edit: ValueEdit::Hash { .. },
                ..
            }]
        ));
    }

    /// Emptying a collection says it deletes the key; JSON that doesn't fit
    /// the type writes nothing.
    #[test]
    fn emptying_warns_and_bad_json_writes_nothing() {
        let (mut app, mut events) = on_key(RedisType::List);
        let job = || EditorJob {
            key: "k".into(),
            text: r#"["a", "b"]"#.into(),
            target: Target::Whole(RedisType::List),
        };
        app.save_edit(job(), Edited::Changed("[]".into()));
        let (_, warning) = preview(&app);
        assert_eq!(
            warning.as_deref(),
            Some("the list ends up empty, which deletes the key")
        );
        app.pending = None;
        app.save_edit(job(), Edited::Changed(r#"{"a": 1}"#.into()));
        assert!(app.pending.is_none());
        assert!(sent(&mut events).is_empty());
    }

    /// Inside a zset, `Shift-e` opens the selected score; the saved number
    /// is a `ZsetScore` edit, and text that isn't a score writes nothing.
    #[test]
    fn inside_a_value_the_selected_item_is_edited() {
        use crate::redis_client::types::{ItemEdit, KeyMeta, KeyValue, LoadedValue, ValuePageInfo};

        let (mut app, mut events) = on_key(RedisType::Zset);
        *app.state.keys.lock() = Some(vec![KeyMeta {
            key: "k".into(),
            r_type: RedisType::Zset,
            size: 10,
            ttl: -1,
            value: KeyValue::Unknown,
        }]);
        app.load_new_keys();
        assert!(app.keyspace.select_key_by_name("k"));
        app.keyspace.set_selected_value(Some(LoadedValue {
            value: KeyValue::Zset(vec![("ada".into(), 42.0)]),
            page: ValuePageInfo {
                start: 0,
                len: 1,
                total: Some(1),
                has_next: false,
                has_prev: false,
            },
        }));
        app.keyspace.enter_value();
        while events.try_recv().is_ok() {}

        shift_e(&mut app);
        assert!(
            sent(&mut events).is_empty(),
            "no fetch: the item is on screen"
        );
        let job = app.editor_job.take().expect("queued");
        assert_eq!(job.text, "42");

        app.save_edit(
            EditorJob {
                key: job.key.clone(),
                text: job.text.clone(),
                target: Target::Item {
                    slot: crate::widgets::keyspace::ItemSlot::ZsetScore {
                        member: "ada".into(),
                    },
                    row: 0,
                },
            },
            Edited::Changed("x".into()),
        );
        assert!(app.pending.is_none(), "not a score");
        app.save_edit(job, Edited::Changed("43.5".into()));
        let (lines, _) = preview(&app);
        assert_eq!(lines, [r#"ZADD "k" "43.5" "ada""#]);
        press_y(&mut app);
        assert!(matches!(
            &sent(&mut events)[..],
            [RedisEvent::EditItem { edit: ItemEdit::ZsetScore { score, .. }, .. }] if (*score - 43.5).abs() < f64::EPSILON
        ));
    }

    fn preview(app: &App) -> (Vec<String>, Option<String>) {
        match &app.pending {
            Some(crate::app::confirm::PendingConfirm::Write(write)) => write.shown(),
            other => panic!("no write preview open: {other:?}"),
        }
    }

    fn press_y(app: &mut App) {
        let k = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        if let Some(action) = app.handle_key_event(k) {
            app.update(&action);
        }
    }

    /// A binary value isn't opened; a failed fetch doesn't leave the next
    /// `y` going to the editor.
    #[test]
    fn binary_values_and_failed_fetches_open_nothing() {
        let (mut app, _events) = on_key(RedisType::String);
        shift_e(&mut app);
        *app.state.yank.lock() = Some(("k".into(), "a\u{f780}b".into()));
        app.update(&Action::YankReady);
        assert!(app.editor_job.is_none());

        shift_e(&mut app);
        app.update(&Action::YankFailed("gone".into()));
        assert!(app.external_edit.is_none());
        *app.state.yank.lock() = Some(("k".into(), "copied".into()));
        app.update(&Action::YankReady);
        assert!(app.editor_job.is_none(), "a plain copy again");
    }

    #[test]
    fn the_size_note_shows_both_sizes_and_the_change() {
        assert_eq!(size_note(10, 12), "size 10 B → 12 B (+2 B)");
        assert_eq!(size_note(12, 10), "size 12 B → 10 B (−2 B)");
    }
}
