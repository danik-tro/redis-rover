//! `Shift-e`: edit a STRING in `$VISUAL` / `$EDITOR` / `vi`. The whole
//! value is fetched like a yank (`FetchFullValue`), the TUI is suspended
//! around [`external_editor::edit`], and the saved text goes through the
//! preview as a `SetString`, its size change noted.

use color_eyre::eyre::Result;

use super::{confirm::AfterWrite, App};
use crate::{
    external_editor::{self, Edited, MAX_EDIT_BYTES},
    mode::Mode,
    redis_client::{bytes::is_binary, escape::printable, event::RedisEvent, types::RedisType},
    tui::Tui,
    widgets::keyspace::format_bytes,
};

/// A value fetched for the editor, waiting for the event loop (which owns
/// the terminal) to run it.
#[derive(Debug)]
pub(super) struct EditorJob {
    key: String,
    text: String,
}

impl App {
    /// `Shift-e` on a STRING key: fetch the whole value for the editor.
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
        if r_type != RedisType::String {
            self.keyspace
                .show_notice("Only string values open in $EDITOR.");
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
        self.external_edit = Some(key.clone());
        self.send_redis_event(RedisEvent::FetchFullValue { key, r_type });
    }

    /// `YankReady` for a value fetched for the editor: queue the edit.
    /// Returns `false` when the yank was a plain copy.
    pub(super) fn yank_for_editor(&mut self) -> bool {
        let Some(waiting) = self.external_edit.take() else {
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
        self.editor_job = Some(EditorJob { key, text });
        true
    }

    /// Run a queued edit: hand the terminal to the editor and take it back,
    /// then preview the change. A `SIGTERM` / `SIGHUP` meanwhile quits.
    pub(super) async fn run_editor_job(&mut self, tui: &mut Tui) -> Result<()> {
        let Some(EditorJob { key, text }) = self.editor_job.take() else {
            return Ok(());
        };
        let editor = external_editor::editor_command(
            std::env::var("VISUAL").ok(),
            std::env::var("EDITOR").ok(),
        );
        tui.suspend().await?;
        let edited = external_editor::edit(&text, &editor).await;
        if edited == Edited::Stopped {
            self.should_quit = true;
            return Ok(());
        }
        tui.resume()?;
        self.redraw.dirty = true;
        let now = self.clock.now();
        match edited {
            Edited::Changed(value) => {
                let note = size_note(text.len(), value.len());
                let after = AfterWrite::reselect(&key);
                self.submit_write_noted(
                    RedisEvent::SetString { key, value },
                    after,
                    None,
                    vec![note],
                );
            }
            Edited::Unchanged => self.toast.show("No change", false, now),
            Edited::Failed(reason) => self.toast.show(format!("Not saved: {reason}"), false, now),
            Edited::Stopped => {}
        }
        Ok(())
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

    use super::size_note;
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
    fn only_writable_strings_open_in_the_editor() {
        let (mut app, mut events) = on_key(RedisType::Hash);
        shift_e(&mut app);
        assert!(app.keyspace.is_overlay_open(), "a notice");
        assert!(sent(&mut events).is_empty());

        let (mut app, mut events) = on_key(RedisType::String);
        app.set_read_only(true);
        shift_e(&mut app);
        assert!(sent(&mut events).is_empty());
        assert!(app.external_edit.is_none());
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
