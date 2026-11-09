use super::App;

use crate::redis_client::{event::RedisEvent, types::RedisType};

use crate::{clipboard::Via, mode::Mode, widgets::keyspace::format_bytes};

/// Values above this size (`MEMORY USAGE`) ask before being copied whole.
const YANK_WARN_BYTES: u128 = 10 * 1024 * 1024;

/// Text for the clipboard, what it is (for the toast) and an optional note.
struct Copy {
    text: String,
    what: String,
    note: Option<&'static str>,
}

/// `Copied value of k (12 B)`, with `note` next to the size and how it got
/// there when not via the OS clipboard.
fn copied_toast(what: &str, len: usize, note: Option<&str>, via: Via) -> String {
    let how = if via == Via::Terminal {
        " via terminal (OSC 52)"
    } else {
        ""
    };
    let note = note.map(|n| format!(", {n}")).unwrap_or_default();
    format!("Copied {what} ({}{note}){how}", format_bytes(len))
}

impl App {
    /// `y`: copy the selected element (inside a collection) or the selected
    /// key's whole value (#27). A value that isn't fully loaded is fetched in
    /// the background; above [`YANK_WARN_BYTES`] the user confirms first.
    pub(super) fn yank_value(&mut self) {
        if self.mode == Mode::PubSub {
            self.yank_pubsub_payload();
            return;
        }
        if self.mode != Mode::KeySpace {
            return;
        }
        let Some((key, r_type)) = self.keyspace.selected_key() else {
            return;
        };
        if self.keyspace.is_value_focused() {
            if let Some(text) = self.keyspace.selected_element_text() {
                self.copy_text(&text, &format!("item of {key}"));
            }
            return;
        }
        if let Some(text) = self.keyspace.loaded_value_text() {
            self.copy_text(&text, &format!("value of {key}"));
            return;
        }
        if matches!(r_type, RedisType::Stream | RedisType::Unknown(_)) {
            self.toast.show(
                format!("Copying {} values isn't supported yet", r_type.name()),
                false,
            );
            return;
        }
        match self.keyspace.selected_size() {
            Some(size) if size > YANK_WARN_BYTES => {
                self.keyspace.open_yank_confirm(key, r_type, size);
            }
            _ => self.start_full_yank(key, r_type),
        }
    }

    /// Ask the runner for `key`'s whole value; `YankReady` copies it.
    pub(super) fn start_full_yank(&mut self, key: String, r_type: RedisType) {
        self.toast.show(format!("Copying {key}…"), true);
        self.send_redis_event(RedisEvent::FetchFullValue { key, r_type });
    }

    /// The runner left a whole value in `SharedState::yank`: copy it.
    pub(super) fn finish_yank(&mut self) {
        let Some((key, text)) = self.state.yank.lock().take() else {
            return;
        };
        self.copy_text(&text, &format!("value of {key}"));
    }

    /// `Shift-y`: copy the selected key's name.
    pub(super) fn yank_key(&mut self) {
        if self.mode == Mode::PubSub {
            if let Some(copy) = self.pubsub_copy(false) {
                self.copy_text_noted(&copy.text, &copy.what, copy.note);
            }
            return;
        }
        if self.mode != Mode::KeySpace {
            return;
        }
        if let Some((key, _)) = self.keyspace.selected_key() {
            self.copy_text(&key, &format!("key name {key}"));
        }
    }

    /// `y` in the Pub/Sub tab: copy the selected message's stored payload
    /// (binary as hex). A payload cut at `max_payload` says so.
    fn yank_pubsub_payload(&mut self) {
        if let Some(copy) = self.pubsub_copy(true) {
            self.copy_text_noted(&copy.text, &copy.what, copy.note);
        }
    }

    /// What `y` (`payload`) / `Shift-y` copy in the Pub/Sub tab: the
    /// selected message's payload or channel.
    fn pubsub_copy(&self, payload: bool) -> Option<Copy> {
        let msg = self.pubsub.selected_message()?;
        Some(if payload {
            Copy {
                text: msg.payload.copy_text(),
                what: format!("payload of {}", msg.channel),
                note: msg.truncated.then_some("truncated"),
            }
        } else {
            Copy {
                what: format!("channel {}", msg.channel),
                text: msg.channel,
                note: None,
            }
        })
    }

    /// Put `text` on the clipboard and confirm with a toast.
    fn copy_text(&mut self, text: &str, what: &str) {
        self.copy_text_noted(text, what, None);
    }

    /// [`Self::copy_text`] with a `note` next to the size, e.g.
    /// `Copied payload of c (64.00 KiB, truncated)`.
    fn copy_text_noted(&mut self, text: &str, what: &str, note: Option<&str>) {
        let msg = match self.clipboard.copy(text) {
            Ok(via) => copied_toast(what, text.len(), note, via),
            Err(err) => format!("Copy failed: {err}"),
        };
        self.toast.show(msg, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    use crate::config;

    use crossterm::event::KeyCode;

    use ratatui::crossterm::event::KeyEvent;

    use crate::app::tests::{app_on_orders, key, tap};
    use crate::{action::Action, clipboard::Via};

    #[test]
    fn y_and_shift_y_pick_the_payload_and_channel() {
        let mut app = app_on_orders();
        assert_eq!(
            app.handle_key_event(key(KeyCode::Char('y'))),
            Some(Action::YankValue)
        );
        assert_eq!(
            app.handle_key_event(KeyEvent::new(
                KeyCode::Char('Y'),
                crossterm::event::KeyModifiers::SHIFT
            )),
            Some(Action::YankKey)
        );
        // Following: the newest message.
        let copy = app.pubsub_copy(true).unwrap();
        assert_eq!(copy.text, r#"{"id":3}"#);
        assert_eq!(copy.what, "payload of orders:new");
        assert_eq!(copy.note, None);
        let copy = app.pubsub_copy(false).unwrap();
        assert_eq!(
            (copy.text.as_str(), copy.what.as_str()),
            ("orders:new", "channel orders:new")
        );

        // The selected row, not the newest one.
        tap(&mut app, KeyCode::Tab);
        tap(&mut app, KeyCode::Char('k'));
        assert_eq!(app.pubsub_copy(true).unwrap().text, r#"{"id":2}"#);

        // A payload cut at `max_payload` says so.
        let big = "x".repeat(config::get().pubsub.max_payload() + 10);
        app.state.pubsub.lock().push(
            "orders:big",
            Some("orders:*"),
            big.as_bytes(),
            SystemTime::now(),
        );
        tap(&mut app, KeyCode::Char(' '));
        let copy = app.pubsub_copy(true).unwrap();
        assert_eq!(copy.note, Some("truncated"));
        assert_eq!(copy.text.len(), config::get().pubsub.max_payload());
        assert_eq!(
            copied_toast(&copy.what, 65_536, copy.note, Via::System),
            "Copied payload of orders:big (64.00 KiB, truncated)"
        );
        assert_eq!(
            copied_toast("key name k", 1, None, Via::Terminal),
            "Copied key name k (1 B) via terminal (OSC 52)"
        );
    }
}
