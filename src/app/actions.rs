use super::App;
use std::time::{Duration, Instant};

use crate::mode::PopupMode;
use color_eyre::eyre::Result;

use crate::{action::Action, mode::Mode, tui};

/// How long the `?` help overlay stays up before auto-dismissing.
const HELP_TIMEOUT: Duration = Duration::from_secs(7);

impl App {
    /// Run `action`: the state change in [`Self::update`], then the terminal
    /// work (`Render` draws, `Resize` resizes).
    pub(super) fn handle_action(
        &mut self,
        action: &Action,
        tui: &mut tui::Tui,
    ) -> Result<Option<Action>> {
        self.update(action);
        match *action {
            Action::Resize(w, h) => Self::resize(tui, (w, h))?,
            Action::Render => {
                let now = self.clock.now();
                let draw = self.should_draw(now);
                if draw {
                    self.draw(tui)?;
                    self.sync_page_size();
                }
                self.count_frame(draw, now);
            }
            _ => {}
        }
        Ok(None)
    }

    /// The state change of `action`, without a terminal. Every action but
    /// `Tick` / `Render` marks the screen dirty; `Tick` only when it changed
    /// something shown.
    pub(super) fn update(&mut self, action: &Action) {
        let background = matches!(action, Action::Tick | Action::Render);
        if !background {
            log::debug!("{action:?}");
            self.redraw.dirty = true;
        }
        // One clock reading per action, passed down.
        let now = self.clock.now();
        let help_open = self.help_is_open();
        let mode = self.mode;
        self.maybe_dismiss_help(action, now);
        self.apply(action, now, help_open);
        if self.mode != mode {
            self.sync_backfill_pause();
        }
    }

    fn apply(&mut self, action: &Action, now: Instant, help_open: bool) {
        match *action {
            Action::Tick => {
                let mut changed = help_open != self.help_is_open();
                changed |= self.expire_pending_chord(now);
                changed |= self.request_filter_count(now);
                changed |= self.toast.expire(now);
                changed |= self.pubsub_tick(now, self.clock.wall());
                self.redraw.dirty |= changed;
            }
            Action::Quit => self.should_quit = true,
            // `Resize` / `Render` are terminal work, done by `handle_action`.
            Action::Resize(..) | Action::Render | Action::Refresh => {}
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
                self.request_backfill();
            }
            Action::KeyMetaBackfilled => self.meta_backfilled(),
            Action::OpenSortMenu => self.keyspace.open_sort_menu(),
            Action::OpenTypeFilter => self.open_type_filter(),
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
            Action::YankFailed(ref msg) => self.toast.show(msg.clone(), false, now),
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
            Action::Help => self.toggle_help(now),
            Action::ToggleReadOnly => self.toggle_read_only(now),
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
        }
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
            Action::PubSubPublish => self.pubsub.open_publish(self.is_read_only()),
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
                self.clock.now(),
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

    pub(super) fn toggle_help(&mut self, now: Instant) {
        self.help_opened_at = if self.help_opened_at.is_some() {
            None
        } else {
            Some(now)
        };
    }

    pub(super) fn help_is_open(&self) -> bool {
        self.help_opened_at.is_some()
    }

    /// Hide the help overlay when it has been up longer than [`HELP_TIMEOUT`]
    /// (checked on `Tick`) or as soon as the user issues any real command —
    /// `Help` itself toggles it, and `Tick`/`Render` are background noise.
    fn maybe_dismiss_help(&mut self, action: &Action, now: Instant) {
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
                if now.duration_since(opened_at) >= HELP_TIMEOUT {
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
            // Inside an opened JSON field, Esc goes back one level first.
            if !self.close_json_field() {
                self.keyspace.exit_value_focus();
            }
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
    use std::{sync::Arc, time::UNIX_EPOCH};

    use super::HELP_TIMEOUT;
    use crate::action::Action;
    use crate::app::tests::app;
    use crate::clock::{Clock, FixedClock};
    use crate::mode::Mode;
    use crate::mode::PopupMode;

    #[test]
    fn help_closes_on_the_tick_after_its_timeout() {
        let mut app = app();
        let clock = Arc::new(FixedClock::new(UNIX_EPOCH));
        app.clock = clock.clone();
        app.toggle_help(clock.now());

        clock.advance(HELP_TIMEOUT / 2);
        app.maybe_dismiss_help(&Action::Tick, clock.now());
        assert!(app.help_is_open());

        clock.advance(HELP_TIMEOUT / 2);
        app.maybe_dismiss_help(&Action::Tick, clock.now());
        assert!(!app.help_is_open());
    }

    /// #49: Enter on a field of a JSON preview opens it, Esc goes back to the
    /// preview on the same row.
    #[test]
    fn json_preview_field_opens_and_closes() {
        use crate::app::tests::{app_with_channels, meta};
        use crate::redis_client::{
            event::RedisEvent,
            types::{
                JsonField, JsonPreview, JsonPreviewKind, KeyValue, LoadedValue, RedisType,
                ValuePageInfo,
            },
        };

        let (mut app, _cmds, mut redis_rx) = app_with_channels();
        let mut doc = meta("doc");
        doc.r_type = RedisType::Json;
        doc.size = 50 << 20;
        app.keyspace.set_keys(vec![doc]);
        assert!(app.keyspace.select_key_by_name("doc"));
        app.request_selected_value();
        assert_eq!(
            app.state.value_state.lock().size,
            Some(50 << 20),
            "KeyMeta's size reaches the fetch"
        );

        let field = |name: &str| JsonField {
            name: name.into(),
            json_type: "object".into(),
            bytes: 10,
        };
        let preview = LoadedValue {
            value: KeyValue::JsonPreview(JsonPreview {
                bytes: 50 << 20,
                kind: JsonPreviewKind::Object {
                    len: 2,
                    fields: vec![field("a"), field("b")],
                },
            }),
            page: ValuePageInfo::default(),
        };
        app.keyspace.set_selected_value(Some(preview.clone()));
        app.keyspace.enter_value();
        app.keyspace.scroll_next();
        while redis_rx.try_recv().is_ok() {}

        app.enter_value();
        assert_eq!(app.state.value_state.lock().json_path, vec!["b"]);
        assert!(matches!(redis_rx.try_recv(), Ok(RedisEvent::FetchValue)));

        // The field loads as JSON text and gets the focus.
        *app.state.selected_value.lock() = Some(LoadedValue {
            value: KeyValue::Json(serde_json::json!({ "x": 1 })),
            page: ValuePageInfo::default(),
        });
        app.load_selected_value();
        assert!(app.keyspace.is_text_focused());

        // Esc: back to the preview, on row 1 ("b").
        app.close_popup();
        assert!(app.state.value_state.lock().json_path.is_empty());
        assert!(matches!(redis_rx.try_recv(), Ok(RedisEvent::FetchValue)));
        *app.state.selected_value.lock() = Some(preview);
        app.load_selected_value();
        assert_eq!(app.keyspace.selected_json_field().as_deref(), Some("b"));

        // Esc at the root leaves the value as before.
        app.close_popup();
        assert!(!app.keyspace.is_value_focused());
        assert!(redis_rx.try_recv().is_err(), "nothing to fetch");
    }

    /// #50: every action marks the screen dirty, so a new one can't leave a
    /// stale frame up. `Tick` / `Render` are background noise: a `Tick`
    /// marks it only when it changed something (not on an idle app).
    #[test]
    fn every_action_marks_dirty() {
        use strum::IntoEnumIterator;
        const BACKGROUND: [Action; 2] = [Action::Tick, Action::Render];
        for action in Action::iter() {
            let mut app = app();
            app.redraw.dirty = false;
            app.update(&action);
            assert_eq!(
                app.redraw.dirty,
                !BACKGROUND.contains(&action),
                "{action:?}"
            );
        }
    }

    /// A `Tick` that closes the help or a toast marks the screen dirty.
    #[test]
    fn tick_marks_dirty_only_on_change() {
        let mut app = app();
        let clock = Arc::new(FixedClock::new(UNIX_EPOCH));
        app.clock = clock.clone();
        app.toast.show("Copied", false, clock.now());
        app.toggle_help(clock.now());
        app.redraw.dirty = false;

        app.update(&Action::Tick);
        assert!(!app.redraw.dirty, "nothing expired yet");

        clock.advance(crate::widgets::toast::TOAST_TIMEOUT);
        app.update(&Action::Tick);
        assert!(app.redraw.dirty, "the toast went away");
        assert_eq!(app.toast.message(), None);

        app.redraw.dirty = false;
        clock.advance(HELP_TIMEOUT);
        app.update(&Action::Tick);
        assert!(app.redraw.dirty, "the help went away");
        assert!(!app.help_is_open());
    }

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
