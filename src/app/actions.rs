use super::App;
use std::time::{Duration, Instant, SystemTime};

use crate::mode::PopupMode;
use color_eyre::eyre::Result;

use crate::{action::Action, mode::Mode, tui};

/// How long the `?` help overlay stays up before auto-dismissing.
const HELP_TIMEOUT: Duration = Duration::from_secs(7);

impl App {
    pub(super) fn handle_action(
        &mut self,
        action: &Action,
        tui: &mut tui::Tui,
    ) -> Result<Option<Action>> {
        if action != &Action::Tick && action != &Action::Render {
            log::debug!("{action:?}");
        }
        self.maybe_dismiss_help(action);
        match *action {
            Action::Tick => {
                self.expire_pending_chord();
                self.request_filter_count();
                let now = Instant::now();
                self.toast.expire(now);
                self.pubsub_tick(now, SystemTime::now());
            }
            Action::Quit => self.should_quit = true,
            Action::Resize(w, h) => Self::resize(tui, (w, h))?,
            Action::Render => {
                self.draw(tui)?;
                self.sync_page_size();
            }
            Action::PreviousMode => self.switch_to_previous_mode(),
            Action::ToggleDashboard => self.toggle_dashboard(),
            Action::NextChart => self.dashboard.next_chart(),
            Action::PrevChart => self.dashboard.prev_chart(),
            Action::ToggleAllCharts => self.dashboard.toggle_all(),
            Action::ToggleMetricsPause => self.dashboard.toggle_pause(),
            Action::ResetMetrics => self.reset_metrics(),
            Action::LoadKeySpace => self.load_keyspace(),
            Action::RefreshSpace => self.refresh_space(),
            Action::LoadKeysIntoKeySpace => {
                self.finish_keys_fetch();
                self.load_new_keys();
            }
            Action::KeysLoadAborted => self.finish_keys_fetch(),
            Action::RequestSelectedValue => self.request_selected_value(),
            Action::LoadSelectedValueIntoView => self.load_selected_value(),
            Action::ValueLoadFailed => self.keyspace.clear_value_loading(),
            Action::ScrollDown => self.scroll_down(),
            Action::ScrollUp => self.scroll_up(),
            Action::ScrollLeft | Action::ScrollRight => {
                let right = *action == Action::ScrollRight;
                if self.mode == Mode::PubSub {
                    self.pubsub.pan_detail(right);
                } else {
                    self.keyspace.pan_text(right);
                }
            }
            Action::ToggleWrap => {
                if self.mode == Mode::PubSub {
                    self.pubsub.toggle_detail_wrap();
                } else {
                    self.keyspace.toggle_wrap();
                }
            }
            Action::YankValue => self.yank_value(),
            Action::YankKey => self.yank_key(),
            Action::YankReady => self.finish_yank(),
            Action::YankFailed(ref msg) => self.toast.show(msg.clone(), false),
            Action::LoadNextPage => self.load_next_page(),
            Action::LoadPreviousPage => self.load_previous_page(),
            Action::SetKeyspaceFilter => self.enter_filter_popup(),
            Action::DiscardKeyspacePopup => self.close_popup(),
            Action::ConfirmKeyspacePopup => self.confirm_popup(),
            Action::DeleteKeyspaceFilter => self.delete_keyspace_filter(),
            Action::EnterValue => self.enter_value(),
            Action::RequestDeleteKey => self.request_delete_key(),
            Action::RequestSetTtl => self.request_set_ttl(),
            Action::RequestEditValue => self.request_edit_value(),
            Action::RequestAddKey => self.request_add_key(),
            Action::RequestAddItem => self.request_add_item(),
            Action::OpenDbPicker => self.open_db_picker(),
            Action::DbSwitched => {
                self.finish_keys_fetch();
                self.fetch_keys();
            }
            Action::Help => self.toggle_help(),
            Action::Error(ref msg) => self.show_error_popup(msg),
            Action::FilterMatchCount {
                ref pattern,
                count,
                complete,
            } => self
                .keyspace
                .set_filter_count(pattern.clone(), count, complete),
            Action::TogglePubSub
            | Action::PubSubAdd
            | Action::PubSubRemove
            | Action::PubSubClear
            | Action::PubSubRefresh
            | Action::PubSubPublish
            | Action::PubSubEnter
            | Action::PubSubFocus
            | Action::PubSubTogglePause
            | Action::PubSubChannelsLoaded
            | Action::PubSubToggleNotify
            | Action::Published { .. } => self.handle_pubsub_action(action),
            Action::Refresh => {}
        }

        Ok(None)
    }

    /// Actions of the Pub/Sub tab.
    fn handle_pubsub_action(&mut self, action: &Action) {
        match *action {
            Action::TogglePubSub => self.toggle_pubsub(),
            Action::PubSubAdd => self.pubsub.open_add(),
            Action::PubSubRemove => self.remove_subscription(),
            Action::PubSubClear => {
                if let Some(index) = self.pubsub.selected_sub() {
                    self.state.pubsub.lock().clear(index);
                    self.pubsub.reset_log();
                }
            }
            Action::PubSubRefresh => self.fetch_pubsub_channels(),
            Action::PubSubPublish => self.pubsub.open_publish(self.read_only),
            Action::PubSubEnter => self.pubsub_enter(),
            Action::PubSubFocus => self.pubsub.toggle_focus(),
            Action::PubSubTogglePause => {
                if !self.pubsub.is_detail() {
                    self.pubsub.toggle_pause();
                }
            }
            Action::PubSubChannelsLoaded => self.pubsub.clamp_selection(),
            Action::PubSubToggleNotify => self.toggle_notify(),
            Action::Published { receivers, .. } => self.toast.show(
                format!(
                    "delivered to {receivers} subscriber{}",
                    if receivers == 1 { "" } else { "s" }
                ),
                false,
            ),
            _ => {}
        }
    }
}

/// Handling events logic
impl App {
    pub(super) fn switch_to_previous_mode(&mut self) {
        self.mode = self.previous_mode.take().unwrap_or(Mode::KeySpace);
        self.sync_hint();
        if !matches!(self.mode, Mode::Popup(_)) {
            self.error_message = None;
        }
    }

    pub(super) fn show_error_popup(&mut self, msg: &str) {
        self.error_message = Some(msg.to_owned());
        if !matches!(self.mode, Mode::Popup(_)) {
            self.previous_mode = Some(self.mode);
            self.mode = Mode::Popup(PopupMode::Error);
        }
    }

    pub(super) fn toggle_help(&mut self) {
        self.help_opened_at = if self.help_opened_at.is_some() {
            None
        } else {
            Some(Instant::now())
        };
    }

    pub(super) fn help_is_open(&self) -> bool {
        self.help_opened_at.is_some()
    }

    /// Hide the help overlay when it has been up longer than [`HELP_TIMEOUT`]
    /// (checked on `Tick`) or as soon as the user issues any real command —
    /// `Help` itself toggles it, and `Tick`/`Render` are background noise.
    fn maybe_dismiss_help(&mut self, action: &Action) {
        let Some(opened_at) = self.help_opened_at else {
            return;
        };

        match action {
            // `Help` toggles the overlay; `Tick`/`Render` are background noise.
            // `DiscardKeyspacePopup` (Esc) is handled by `close_popup`, which
            // already gives the help overlay precedence — leave it alone here so
            // a single Esc doesn't also dismiss an underlying keyspace popup.
            Action::Help
            | Action::Render
            | Action::DiscardKeyspacePopup
            | Action::KeysLoadAborted
            | Action::DbSwitched
            | Action::ValueLoadFailed
            | Action::FilterMatchCount { .. }
            | Action::YankReady
            | Action::YankFailed(_) => {}
            Action::Tick => {
                if opened_at.elapsed() >= HELP_TIMEOUT {
                    self.help_opened_at = None;
                }
            }
            _ => self.help_opened_at = None,
        }
    }

    /// Esc backs out one modal layer at a time, in precedence order:
    /// help overlay → action overlay (delete/edit/TTL/notice) → value focus →
    /// filter popup.
    fn close_popup(&mut self) {
        if self.help_is_open() {
            self.help_opened_at = None;
            return;
        }

        if self.keyspace.is_overlay_open() {
            self.keyspace.close_overlay();
            return;
        }

        // The add-key wizard / add-item overlays cancel as a whole on Esc. (Their
        // internal multi-step back-out is handled inside the widget for non-Esc
        // keys; one Esc dismisses the whole flow.)
        if self.keyspace.is_wizard_active() {
            self.keyspace.close_new_key();
            return;
        }

        if self.keyspace.is_add_item_active() {
            self.keyspace.close_add_item();
            return;
        }

        if self.keyspace.is_edit_item_active() {
            self.keyspace.close_edit_item();
            return;
        }

        if self.keyspace.is_value_focused() || self.keyspace.is_text_focused() {
            self.keyspace.exit_value_focus();
            return;
        }

        if !self.keyspace.is_popup() {
            return;
        }

        self.keyspace.exit_popup();
    }

    /// Enter confirms whichever modal is open. Precedence: add-key wizard
    /// finalize → add-item overlay → action overlay (delete/edit/TTL) → filter
    /// popup.
    fn confirm_popup(&mut self) {
        if self.keyspace.is_wizard_active() {
            self.confirm_new_key();
            return;
        }
        if self.keyspace.is_add_item_active() {
            self.confirm_add_item();
            return;
        }
        if self.keyspace.is_edit_item_active() {
            self.confirm_edit_item();
            return;
        }
        if self.keyspace.is_overlay_open() {
            self.confirm_action_overlay();
            return;
        }
        self.set_keyspace_filter();
    }
}

#[cfg(test)]
mod tests {

    use crate::mode::PopupMode;

    use crate::app::tests::app;
    use crate::mode::Mode;

    #[test]
    fn error_popup_returns_to_the_screen_it_opened_on() {
        let mut app = app();
        assert_eq!(app.mode, Mode::KeySpace);

        app.toggle_dashboard();
        assert_eq!(app.mode, Mode::Dashboard);

        app.show_error_popup("boom");
        assert_eq!(app.mode, Mode::Popup(PopupMode::Error));

        app.switch_to_previous_mode();
        assert_eq!(app.mode, Mode::Dashboard);
        assert!(app.error_message.is_none());

        app.toggle_dashboard();
        assert_eq!(app.mode, Mode::KeySpace);
    }
}
