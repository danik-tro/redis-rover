use super::App;
use std::time::{Duration, Instant};

use crate::{
    config,
    mode::PopupMode,
    widgets::keyspace::{TypeMenuOutcome, SORT_MENU},
};
use crossterm::event::KeyCode;
use ratatui::crossterm::event::KeyEvent;

use crate::{action::Action, command::Command, keybindings::KeyResolution, mode::Mode};

/// How long a partially typed key chord waits for its next key.
const CHORD_TIMEOUT: Duration = Duration::from_secs(1);

/// The text input a paste goes into. Checked in the order
/// `handle_key_event` routes keys, so a paste reaches the input that typing
/// would reach, and nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PasteTarget {
    /// The typed confirm on Prod.
    TypedConfirm,
    /// The Pub/Sub add / publish popup; `multiline` in the payload field.
    PubSubPopup {
        multiline: bool,
    },
    /// The filter popup.
    Filter,
    /// The edit-value / TTL overlay.
    Overlay,
    /// The add-key wizard on its name or element step.
    Wizard,
    AddItem,
    EditItem,
}

impl App {
    pub(super) fn handle_key_event(&mut self, key: KeyEvent) -> Option<Action> {
        let is_confirm = key.code == KeyCode::Enter;
        let is_cancel = key.code == KeyCode::Esc;

        // The error popup is modal: Esc/Enter dismiss it, and only global
        // bindings (e.g. Ctrl-c) pass through.
        if self.mode == Mode::Popup(PopupMode::Error) {
            if is_confirm || is_cancel {
                return Some(Action::PreviousMode);
            }
            return self.handle_keybindings(key);
        }

        // A yes / no question (lifting read-only) is modal above everything.
        if self.pending.is_some() {
            return self.handle_confirm_key(key);
        }

        if self.page_help(key) {
            return None;
        }

        // The DB picker is modal: Enter switches, Esc cancels, the rest moves
        // the selection. Global bindings (Ctrl-c) still work.
        if self.db_picker.is_open() {
            if is_confirm {
                self.confirm_db_picker();
                return None;
            }
            if is_cancel {
                self.db_picker.close();
                return None;
            }
            if self.handle_keybindings(key) == Some(Action::Quit) {
                return Some(Action::Quit);
            }
            self.db_picker.handle_key(key);
            return None;
        }

        if self.mode == Mode::PubSub {
            return self.handle_pubsub_key(key);
        }

        // Keep a half-open keyspace overlay from swallowing dashboard keys.
        if matches!(self.mode, Mode::Dashboard | Mode::Monitor | Mode::Slowlog) {
            // Esc backs out of the help overlay first; any other key (`m`)
            // goes through as a real command.
            if is_cancel && self.help_is_open() {
                self.help_opened_at = None;
                return None;
            }
            return self.handle_keybindings(key);
        }

        // The sort menu and the type checklist own every key; Ctrl-c quits.
        if self.keyspace.is_sort_menu_open() || self.keyspace.is_type_menu_open() {
            if Self::is_quit_key(key) {
                return Some(Action::Quit);
            }
            self.handle_menu_key(key, is_cancel);
            return None;
        }

        // While the filter popup is open, raw keys feed its text-area; only
        // Enter (confirm) and Esc (cancel) escape to the dispatcher.
        if self.keyspace.is_popup() && !is_confirm && !is_cancel {
            self.keyspace.handle_key(key, self.clock.now());
            return None;
        }

        // While a text-input action overlay (edit / TTL) is open, raw keys feed
        // it; Enter confirms, Esc cancels.
        if self.keyspace.is_input_active() && !is_confirm && !is_cancel {
            if key.code == KeyCode::Tab && self.keyspace.is_copy_open() {
                self.pick_copy_db();
            } else {
                self.keyspace.handle_overlay_key(key);
            }
            return None;
        }

        // The add-key wizard owns ALL keys except Esc (which cancels the whole
        // flow). Enter/Tab/j/k and text drive its multi-step machine internally;
        // after each key we check whether it asked to finalize.
        if self.keyspace.is_wizard_active() {
            if is_cancel {
                return Some(Action::DiscardKeyspacePopup);
            }
            self.keyspace.handle_wizard_key(key);
            if self.keyspace.wizard_wants_finalize() {
                return Some(Action::ConfirmKeyspacePopup);
            }
            return None;
        }

        // The in-collection add-item overlay: Enter confirms, Esc cancels, the
        // rest is input (incl. Tab to switch fields).
        if self.keyspace.is_add_item_active() {
            if is_confirm {
                return Some(Action::ConfirmKeyspacePopup);
            }
            if is_cancel {
                return Some(Action::DiscardKeyspacePopup);
            }
            self.keyspace.handle_add_item_key(key);
            return None;
        }

        // The in-collection edit-item overlay: Enter saves, Esc cancels, the
        // rest is input.
        if self.keyspace.is_edit_item_active() {
            if is_confirm {
                return Some(Action::ConfirmKeyspacePopup);
            }
            if is_cancel {
                return Some(Action::DiscardKeyspacePopup);
            }
            self.keyspace.handle_edit_item_key(key);
            return None;
        }

        // Enter/Esc act on whatever modal layer is open before falling back to
        // the configured bindings (where Enter is `EnterValue`).
        if self.keyspace.is_popup() || self.keyspace.is_overlay_open() {
            if is_confirm {
                return Some(Action::ConfirmKeyspacePopup);
            }
            if is_cancel {
                return Some(Action::DiscardKeyspacePopup);
            }
        }

        self.handle_keybindings(key)
    }

    /// A `?` help too big for the window pages with n / p (or j / k,
    /// arrows, `PageUp` / `PageDown`). Returns whether `key` turned a page;
    /// any other key acts as before.
    fn page_help(&mut self, key: KeyEvent) -> bool {
        if !self.help_is_open() || self.help_pages < 2 {
            return false;
        }
        let forward = match key.code {
            KeyCode::Char('n' | 'j') | KeyCode::Down | KeyCode::PageDown => true,
            KeyCode::Char('p' | 'k') | KeyCode::Up | KeyCode::PageUp => false,
            _ => return false,
        };
        self.help_page = if forward {
            (self.help_page + 1) % self.help_pages
        } else {
            (self.help_page + self.help_pages - 1) % self.help_pages
        };
        // Paging keeps it open as long as a fresh `?` would.
        self.help_opened_at = Some(self.clock.now());
        true
    }

    /// The focused text input, if a paste may go anywhere at all.
    fn paste_target(&self) -> Option<PasteTarget> {
        if self.mode == Mode::Popup(PopupMode::Error) {
            return None;
        }
        if self.pending.is_some() {
            return self
                .typed_confirm_open()
                .then_some(PasteTarget::TypedConfirm);
        }
        if self.db_picker.is_open() {
            return None;
        }
        if self.mode == Mode::PubSub {
            return self.pubsub.is_popup().then(|| PasteTarget::PubSubPopup {
                multiline: self.pubsub.is_payload_focused(),
            });
        }
        if matches!(self.mode, Mode::Dashboard | Mode::Monitor | Mode::Slowlog)
            || self.keyspace.is_sort_menu_open()
            || self.keyspace.is_type_menu_open()
        {
            return None;
        }
        if self.keyspace.is_popup() {
            return Some(PasteTarget::Filter);
        }
        if self.keyspace.is_input_active() {
            return Some(PasteTarget::Overlay);
        }
        if self.keyspace.is_wizard_active() {
            return self
                .keyspace
                .wizard_takes_text()
                .then_some(PasteTarget::Wizard);
        }
        if self.keyspace.is_add_item_active() {
            return self
                .keyspace
                .item_input_ready()
                .then_some(PasteTarget::AddItem);
        }
        if self.keyspace.is_edit_item_active() {
            return self
                .keyspace
                .item_input_ready()
                .then_some(PasteTarget::EditItem);
        }
        None
    }

    /// Pasted text (bracketed paste). It is typed into the focused text
    /// input and goes nowhere else: on the key list or in a yes / no question
    /// its letters would run as commands (`d`, then `y`, deletes a key).
    /// Line breaks and tabs become spaces, so a pasted line break never
    /// confirms an input; only the publish payload keeps its lines. Other
    /// control characters are dropped.
    pub(super) fn handle_paste(&mut self, text: &str) {
        let now = self.clock.now();
        let Some(target) = self.paste_target() else {
            self.toast
                .show("Paste ignored: no text field is open", false, now);
            return;
        };
        let multiline = target == PasteTarget::PubSubPopup { multiline: true };
        let text = text.trim_end_matches(['\r', '\n']).replace("\r\n", "\n");
        for c in text.chars() {
            let code = match c {
                '\n' | '\r' if multiline => KeyCode::Enter,
                '\n' | '\r' | '\t' => KeyCode::Char(' '),
                c if c.is_control() => continue,
                c => KeyCode::Char(c),
            };
            let key = KeyEvent::new(code, crossterm::event::KeyModifiers::empty());
            match target {
                PasteTarget::TypedConfirm => {
                    self.handle_confirm_key(key);
                }
                PasteTarget::PubSubPopup { .. } => self.pubsub.handle_popup_key(key),
                PasteTarget::Filter => self.keyspace.handle_key(key, now),
                PasteTarget::Overlay => self.keyspace.handle_overlay_key(key),
                PasteTarget::Wizard => self.keyspace.handle_wizard_key(key),
                PasteTarget::AddItem => self.keyspace.handle_add_item_key(key),
                PasteTarget::EditItem => self.keyspace.handle_edit_item_key(key),
            }
        }
    }

    /// A key in the open sort menu (one letter picks, Esc closes) or type
    /// checklist (Esc cancels, Enter applies, the rest is the widget's).
    fn handle_menu_key(&mut self, key: KeyEvent, is_cancel: bool) {
        if self.keyspace.is_sort_menu_open() {
            if is_cancel {
                self.keyspace.close_sort_menu();
            } else if let KeyCode::Char(c) = key.code {
                if let Some(choice) = SORT_MENU.iter().find(|item| item.0 == c) {
                    self.pick_sort(choice.1);
                }
            }
        } else if is_cancel {
            self.keyspace.close_type_menu();
        } else if let TypeMenuOutcome::Apply(filter) = self.keyspace.handle_type_menu_key(key) {
            self.apply_type_filter(filter);
        }
    }
    /// Keys in the Pub/Sub tab. Popups take raw keys (Esc cancels, Ctrl-c
    /// still quits): Enter subscribes in the add popup, Ctrl-s sends in the
    /// publish popup. Otherwise Esc closes the help overlay first, then keys
    /// go through the bindings.
    fn handle_pubsub_key(&mut self, key: KeyEvent) -> Option<Action> {
        if self.pubsub.is_popup() {
            let publish = self.pubsub.is_publish_popup();
            let ctrl_s = key.code == KeyCode::Char('s')
                && key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Esc => self.pubsub.close_popup(),
                _ if Self::is_quit_key(key) => return Some(Action::Quit),
                KeyCode::Enter if !publish => self.confirm_add_subscription(),
                _ if publish && ctrl_s => self.confirm_publish(),
                _ => self.pubsub.handle_popup_key(key),
            }
            return None;
        }
        if key.code == KeyCode::Esc && self.help_is_open() {
            self.help_opened_at = None;
            return None;
        }
        if key.code == KeyCode::Esc && self.pubsub.is_detail() {
            self.pubsub.close_detail();
            return None;
        }
        self.handle_keybindings(key)
    }

    /// `key` alone is the global quit binding (Ctrl-c).
    pub(super) fn is_quit_key(key: KeyEvent) -> bool {
        config::get().keybindings.resolve(Mode::Common, &[key])
            == KeyResolution::Command(Command::Quit)
    }

    fn handle_keybindings(&mut self, mut key: KeyEvent) -> Option<Action> {
        // Symbol keys like `?` arrive with `SHIFT` set on most terminals, but
        // the config writes them literally (no modifier). Drop a lone `SHIFT`
        // on punctuation/symbol chars so those bindings match.
        if let KeyCode::Char(c) = key.code {
            if !c.is_alphanumeric() && key.modifiers == crossterm::event::KeyModifiers::SHIFT {
                key.modifiers = crossterm::event::KeyModifiers::empty();
            }
        }

        self.pending_keys.push(key);
        self.last_key_at = Some(self.clock.now());

        let keybindings = &config::get().keybindings;
        let mut resolution = keybindings.resolve(self.mode, &self.pending_keys);
        if resolution == KeyResolution::NoMatch && self.pending_keys.len() > 1 {
            // The new key broke a pending chord: start over from that key alone.
            self.pending_keys = vec![key];
            resolution = keybindings.resolve(self.mode, &self.pending_keys);
        }

        match resolution {
            KeyResolution::Command(command) => {
                self.pending_keys.clear();
                Some(command.into())
            }
            KeyResolution::Pending => None,
            KeyResolution::NoMatch => {
                self.pending_keys.clear();
                None
            }
        }
    }

    /// Drop a partially typed chord once it has waited [`CHORD_TIMEOUT`].
    pub(super) fn expire_pending_chord(&mut self, now: Instant) -> bool {
        let expired = self
            .last_key_at
            .is_some_and(|at| now.duration_since(at) >= CHORD_TIMEOUT);
        if expired {
            self.pending_keys.clear();
            self.last_key_at = None;
        }
        expired
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent};
    use tokio::sync::mpsc;

    use crate::{
        app::{
            tests::{app_with_channels, key, meta},
            App,
        },
        redis_client::event::RedisEvent,
        target::Environment,
    };

    /// An app on the key list with `name` selected.
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

    fn writes(events: &mut mpsc::UnboundedReceiver<RedisEvent>) -> Vec<RedisEvent> {
        std::iter::from_fn(|| events.try_recv().ok())
            .filter(RedisEvent::is_write)
            .collect()
    }

    /// A help too tall for the window pages with `n` / `p` (and wraps);
    /// the page keys don't scroll the key list underneath.
    #[test]
    fn a_help_that_does_not_fit_pages() {
        use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};

        let (mut app, _events) = on_key(Environment::Local, "k");
        press(&mut app, key(KeyCode::Char('?')));
        assert!(app.help_is_open());
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        crate::app::render::AppWidget.render(area, &mut buf, &mut app);
        assert_eq!(app.help_pages, 2);

        press(&mut app, key(KeyCode::Char('n')));
        assert!(app.help_is_open());
        assert_eq!(app.help_page, 1);
        press(&mut app, key(KeyCode::Char('n')));
        assert_eq!(app.help_page, 0, "wraps around");
        press(&mut app, key(KeyCode::Char('p')));
        assert_eq!(app.help_page, 1);
        press(&mut app, key(KeyCode::Char('?')));
        assert!(!app.help_is_open());
        assert_eq!(app.keyspace.selected_key().unwrap().0, "k");
    }

    /// #67: the same letters delete the key when typed, and do nothing when
    /// pasted, on the key list and in the yes / no preview.
    #[test]
    fn paste_never_runs_as_commands() {
        let (mut app, mut events) = on_key(Environment::Local, "orders:2024");
        app.handle_paste("d\ny\n");
        assert!(app.pending.is_none(), "paste opened the delete preview");
        assert!(!app.keyspace.is_overlay_open());
        assert!(writes(&mut events).is_empty());

        // The preview is open: a pasted `y` is not an answer.
        press(&mut app, key(KeyCode::Char('d')));
        assert!(app.pending.is_some());
        app.handle_paste("y");
        assert!(app.pending.is_some());
        assert!(writes(&mut events).is_empty(), "paste confirmed a delete");

        // Typed, `y` does delete: the paste above is what was at stake.
        press(&mut app, key(KeyCode::Char('y')));
        assert_eq!(writes(&mut events).len(), 1);
    }

    #[test]
    fn paste_goes_into_the_focused_input_and_never_confirms_it() {
        let (mut app, mut events) = on_key(Environment::Local, "orders:2024");
        let now = app.clock.now();
        app.keyspace.enter_filter_pattern(now);
        app.handle_paste("user:*\r\nsession:\t*\u{1b}[31m\n");
        assert!(app.keyspace.is_popup(), "a line break confirmed the filter");
        assert_eq!(
            app.keyspace.confirm_filter_pattern().as_deref(),
            Some("user:* session: *[31m"),
            "line breaks and tabs are spaces, ESC is dropped"
        );
        assert!(writes(&mut events).is_empty());
    }

    /// The typed confirm on Prod takes the pasted name, and still needs
    /// Enter.
    #[test]
    fn paste_into_the_typed_confirm_does_not_delete() {
        let (mut app, mut events) = on_key(Environment::Prod, "orders:2024");
        press(&mut app, key(KeyCode::Char('d')));
        app.handle_paste("orders:2024\n");
        assert!(app.pending.is_some());
        assert!(writes(&mut events).is_empty());
        press(&mut app, key(KeyCode::Enter));
        assert_eq!(writes(&mut events).len(), 1);
    }
}
