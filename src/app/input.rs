use super::App;
use std::time::{Duration, Instant};

use crate::{config, mode::PopupMode};
use crossterm::event::KeyCode;
use ratatui::crossterm::event::KeyEvent;

use crate::{action::Action, command::Command, keybindings::KeyResolution, mode::Mode};

/// How long a partially typed key chord waits for its next key.
const CHORD_TIMEOUT: Duration = Duration::from_secs(1);

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
        if self.mode == Mode::Dashboard {
            // Esc backs out of the help overlay first; any other key (`m`)
            // goes through as a real command.
            if is_cancel && self.help_is_open() {
                self.help_opened_at = None;
                return None;
            }
            return self.handle_keybindings(key);
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
            self.keyspace.handle_overlay_key(key);
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
    fn is_quit_key(key: KeyEvent) -> bool {
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
