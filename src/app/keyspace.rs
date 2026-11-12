use super::App;
use std::time::Instant;

use super::confirm::{AfterWrite, Reselect};
use crate::redis_client::{
    event::RedisEvent,
    types::{ItemDelete, RedisType},
};

use crate::{
    action::{self, Action},
    config,
    mode::Mode,
    widgets::keyspace::{OverlayRequest, PageInfo, ScrollOutcome},
};

impl App {
    pub(super) fn enter_filter_popup(&mut self) {
        let now = self.clock.now();
        self.keyspace.enter_filter_pattern(now);
        self.request_filter_count(now);
    }

    /// Ask the runner to count the filter popup's matches once its text has
    /// settled (#24).
    /// Returns whether a count was requested (the popup shows it counting).
    pub(super) fn request_filter_count(&mut self, now: Instant) -> bool {
        let Some(pattern) = self.keyspace.filter_count_request(now) else {
            return false;
        };
        self.send_redis_event(RedisEvent::CountMatches { pattern });
        true
    }

    /// Finalize the add-key wizard into a `CreateKey` event. Keeps the cursor on
    /// the new key via `reselect_key`. Existence is validated in the storage
    /// layer (surfacing as an `Action::Error` if the key already exists).
    pub(super) fn confirm_new_key(&mut self) {
        let Some((key, spec)) = self.keyspace.take_new_key_request() else {
            // Nothing valid supplied; leave the wizard closed.
            self.keyspace.close_new_key();
            return;
        };
        let after = AfterWrite::reselect(&key);
        self.submit_write(RedisEvent::CreateKey { key, spec }, after);
    }

    /// Finalize the in-collection add-item overlay into an `AddItem` event. On
    /// invalid input the widget keeps the overlay open with a notice, so a
    /// `None` here simply means "stay open".
    ///
    /// To keep the user inside the collection after the post-write refresh,
    /// remember the focused key (so the cursor stays put) and the value-table
    /// row (so focus is restored once the larger value reloads).
    pub(super) fn confirm_add_item(&mut self) {
        // Capture focus state *before* taking the request (which closes the
        // overlay but leaves value focus intact).
        let row = self.keyspace.value_selected_row().unwrap_or(0);
        if let Some((key, item)) = self.keyspace.take_add_item_request() {
            let after = AfterWrite::reselect_row(&key, row);
            self.submit_write(RedisEvent::AddItem { key, item }, after);
        }
    }

    /// Finalize the in-collection edit-item overlay into an `EditItem` event.
    /// On invalid input the widget keeps the overlay open with a notice, so a
    /// `None` here simply means "stay open". Keeps the user inside the collection
    /// on the same row after the post-write refresh, like [`Self::confirm_add_item`].
    pub(super) fn confirm_edit_item(&mut self) {
        let row = self.keyspace.value_selected_row().unwrap_or(0);
        if let Some((key, edit)) = self.keyspace.take_edit_item_request() {
            let after = AfterWrite::reselect_row(&key, row);
            self.submit_write(RedisEvent::EditItem { key, edit }, after);
        }
    }

    /// Translate the confirmed action overlay into a Redis write event.
    pub(super) fn confirm_action_overlay(&mut self) {
        // Capture the value-table row before taking the request (delete-item
        // leaves value focus intact so the user can be restored to it).
        let row = self.keyspace.value_selected_row().unwrap_or(0);
        let Some(request) = self.keyspace.take_overlay_request() else {
            // Notice-only overlay: nothing to do, it's already been dismissed.
            return;
        };
        if let OverlayRequest::Yank { key, r_type } = request {
            self.start_full_yank(key, r_type);
            return;
        }
        let Some((key, _)) = self.keyspace.selected_key() else {
            return;
        };

        let (event, after) = match request {
            OverlayRequest::Delete => Self::delete_key_write(key),
            OverlayRequest::DeleteItem(delete) => Self::delete_item_write(key, delete, row),
            OverlayRequest::SetString(value) => {
                // Non-destructive: keep the cursor on this key after the refresh.
                let after = AfterWrite::reselect(&key);
                (RedisEvent::SetString { key, value }, after)
            }
            OverlayRequest::SetTtl(secs) => {
                let after = AfterWrite::reselect(&key);
                (RedisEvent::SetTtl { key, secs }, after)
            }
            OverlayRequest::Yank { .. } => return,
        };
        self.submit_write(event, after);
    }

    /// The key is going away: the refresh lands on whatever fills its place
    /// (top of the page) rather than reselecting it.
    fn delete_key_write(key: String) -> (RedisEvent, AfterWrite) {
        let after = AfterWrite {
            reselect: Reselect::Clear,
            restore_value_row: None,
        };
        (RedisEvent::DeleteKey { key }, after)
    }

    /// The key survives: keep the user inside the collection on the same
    /// (clamped) row after the refresh.
    fn delete_item_write(key: String, delete: ItemDelete, row: usize) -> (RedisEvent, AfterWrite) {
        let after = AfterWrite::reselect_row(&key, row);
        (RedisEvent::DeleteItem { key, delete }, after)
    }

    /// Enter: focus the value, or, on a field of a JSON preview, open that
    /// field (#49).
    pub(super) fn enter_value(&mut self) {
        let Some(field) = self.keyspace.selected_json_field() else {
            self.keyspace.enter_value();
            return;
        };
        self.json_rows
            .push(self.keyspace.value_selected_row().unwrap_or(0));
        self.state.value_state.lock().open_json_field(field);
        self.load_json_level(0);
    }

    /// Esc on an opened JSON field: back to the preview it was opened from.
    /// Returns `false` when no field is open.
    pub(super) fn close_json_field(&mut self) -> bool {
        if !self.state.value_state.lock().close_json_field() {
            return false;
        }
        let row = self.json_rows.pop().unwrap_or(0);
        self.load_json_level(row);
        true
    }

    /// Fetch the JSON level just opened / closed, focusing `row` once it loads.
    fn load_json_level(&mut self, row: usize) {
        self.json_focus_row = Some(row);
        self.page_turn_row = None;
        self.keyspace.set_value_loading();
        self.send_redis_event(RedisEvent::FetchValue);
    }

    /// `d`: delete. From the key list this deletes the whole selected key; while
    /// drilled into a collection value it deletes only the selected element.
    pub(super) fn request_delete_key(&mut self) {
        if self.keyspace.selected_key().is_none() || self.deny_write() {
            return;
        }
        let Some((key, _)) = self.keyspace.selected_key() else {
            return;
        };
        // The preview is the confirmation; the old "Delete …?" box is
        // only for `safety.preview = "off"`.
        let probe = RedisEvent::DeleteKey { key: String::new() };
        if !self.previews(&probe) {
            if self.keyspace.is_value_focused() {
                self.keyspace.open_delete_item_confirm();
            } else {
                self.keyspace.open_delete_confirm();
            }
            return;
        }
        let (event, after) = if self.keyspace.is_value_focused() {
            let Some(delete) = self.keyspace.selected_item_delete() else {
                return;
            };
            let row = self.keyspace.value_selected_row().unwrap_or(0);
            Self::delete_item_write(key, delete, row)
        } else {
            Self::delete_key_write(key)
        };
        self.submit_write(event, after);
    }

    pub(super) fn request_set_ttl(&mut self) {
        if self.keyspace.selected_key().is_some() && !self.deny_write() {
            self.keyspace.open_set_ttl();
        }
    }

    /// `e`: edit. STRING opens the whole-value editor. For a collection, the
    /// first `e` drills into the value (so the user sees the elements) and the
    /// second `e` — now value-focused — edits the selected element.
    pub(super) fn request_edit_value(&mut self) {
        let Some((key, r_type)) = self.keyspace.selected_key() else {
            return;
        };
        match r_type {
            RedisType::List | RedisType::Set | RedisType::Hash | RedisType::Zset => {
                if self.keyspace.is_value_focused() {
                    if !self.deny_write() {
                        self.keyspace.open_edit_item(key);
                    }
                } else {
                    // Drill in first; a second `e` edits the selected element.
                    self.keyspace.enter_value();
                }
            }
            // STRING edits the whole value; JSON/Unknown fall through to the
            // editor (which shows a notice for non-STRING scalars).
            RedisType::String | RedisType::Json | RedisType::Stream | RedisType::Unknown(_) => {
                if !self.deny_write() {
                    self.keyspace.open_edit_value();
                }
            }
        }
    }

    /// `a`: open the add-key wizard. Available from the key list at any time.
    pub(super) fn request_add_key(&mut self) {
        if !self.deny_write() {
            self.keyspace.open_new_key();
        }
    }

    /// `i`: append an item to the focused collection. No-op unless the user has
    /// drilled into a collection value (`is_value_focused()`), so it can't fire
    /// from the key list.
    pub(super) fn request_add_item(&mut self) {
        if !self.keyspace.is_value_focused() || self.deny_write() {
            return;
        }
        if let Some((key, r_type)) = self.keyspace.selected_key() {
            self.keyspace.open_add_item(key, r_type);
        }
    }

    pub(super) fn set_keyspace_filter(&mut self) {
        if !self.keyspace.is_popup() {
            return;
        }

        let pattern = self.keyspace.confirm_filter_pattern();
        self.state.keyspace_state.lock().set_pattern(pattern);
        self.fetch_keys();
    }

    pub(super) fn delete_keyspace_filter(&mut self) {
        self.state.keyspace_state.lock().delete_pattern();
        self.fetch_keys();
    }

    /// `n`: next page of the value when drilled into one, else of the keyspace.
    pub(super) fn load_next_page(&mut self) {
        if self.keyspace.is_text_focused() {
            self.keyspace.scroll_text_page(true);
            return;
        }
        if self.keyspace.is_value_focused() {
            self.turn_value_page(true, 0);
            return;
        }
        // Paging reuses the sweep: no rescan, only the new page's metadata.
        if self.state.keyspace_state.lock().next_page() {
            self.fetch_keys();
        }
    }

    /// `p`: previous page of the value when drilled into one, else of the
    /// keyspace.
    pub(super) fn load_previous_page(&mut self) {
        if self.keyspace.is_text_focused() {
            self.keyspace.scroll_text_page(false);
            return;
        }
        if self.keyspace.is_value_focused() {
            self.turn_value_page(false, 0);
            return;
        }
        if self.state.keyspace_state.lock().previous_page() {
            self.fetch_keys();
        }
    }

    /// Move the selected value one page forward / back and fetch it, selecting
    /// `row` once it lands. Ignored while a page is already loading, so holding
    /// `j` can't skip pages the user never saw.
    fn turn_value_page(&mut self, forward: bool, row: usize) {
        if self.keyspace.is_value_loading() {
            return;
        }
        let moved = {
            let mut value_state = self.state.value_state.lock();
            if forward {
                value_state.next_page()
            } else {
                value_state.previous_page()
            }
        };
        if moved {
            self.page_turn_row = Some(row);
            self.keyspace.set_value_loading();
            self.send_redis_event(RedisEvent::FetchValue);
        }
    }

    pub(super) fn load_new_keys(&mut self) {
        let page_info = PageInfo::from(&*self.state.keyspace_state.lock());
        self.keyspace.set_page_info(page_info);
        // Move, don't copy. `None` means an earlier LoadKeysIntoKeySpace
        // already took this page (two fetches finished back to back), so the
        // table already shows it.
        if let Some(keys) = self.state.keys.lock().take() {
            self.keyspace.set_keys(keys);
        }
        self.keyspace.clear_selected_value();

        // After a non-destructive edit, keep the cursor on the edited key and
        // reload its (now updated) value instead of snapping back to the top.
        if let Some(key) = self.reselect_key.take() {
            if self.keyspace.select_key_by_name(&key) {
                // If an add-item is in flight, arm the focus restore for the
                // value load this re-request triggers (the final one).
                if self.restore_value_row.is_some() {
                    self.restore_value_pending = true;
                }
                action::try_send_action(&self.tx, Action::RequestSelectedValue);
            } else {
                // Key vanished (e.g. filtered out); drop the pending restore.
                self.restore_value_row = None;
            }
        }
    }

    /// Fetch the selected key's value. Re-requesting the same key (e.g. after a
    /// write) keeps its current page; a different key starts at page 1.
    pub(super) fn request_selected_value(&mut self) {
        let Some((key, r_type)) = self.keyspace.selected_key() else {
            return;
        };
        let size = self
            .keyspace
            .selected_size()
            .filter(|&size| size > 0)
            .and_then(|size| u64::try_from(size).ok());
        {
            let mut value_state = self.state.value_state.lock();
            value_state.select(&key, r_type);
            value_state.size = size;
            value_state.json_cap = config::get().keyspace.json_preview_bytes();
            if value_state.json_path.is_empty() {
                self.json_rows.clear();
            }
        }
        self.page_turn_row = None;
        self.json_focus_row = None;
        self.keyspace.set_value_loading();
        self.send_redis_event(RedisEvent::FetchValue);
    }

    pub(super) fn load_selected_value(&mut self) {
        let value = self.state.selected_value.lock().clone();
        let json_path = self.state.value_state.lock().json_path.clone();
        self.keyspace.set_json_path(json_path);
        self.keyspace.set_selected_value(value);

        if let Some(row) = self.json_focus_row.take() {
            self.keyspace.focus_value_at(row);
        }

        if let Some(row) = self.page_turn_row.take() {
            if self.keyspace.is_value_focused() {
                self.keyspace.restore_value_focus(row);
            }
        }

        // After an add-item, re-enter the collection at the row the user was on
        // (clamped to the new length) so adding doesn't bounce focus to the key
        // list. `restore_value_row` is armed by `load_new_keys` only once the
        // post-write keyspace refresh (which clears focus) has already run, so
        // this fires on the *final* value load and isn't undone afterwards.
        if self.restore_value_pending {
            self.restore_value_pending = false;
            if let Some(row) = self.restore_value_row.take() {
                self.keyspace.restore_value_focus(row);
            }
        }
    }

    /// Size keyspace pages to the rows the table can show (#45). Runs after
    /// each draw, so a resize (or the cluster banner appearing) re-pages the
    /// sweep. The selected key stays selected and on screen.
    pub(super) fn sync_page_size(&mut self) {
        if matches!(self.mode, Mode::Dashboard | Mode::PubSub) {
            return;
        }
        let rows = self.keyspace.visible_rows();
        if rows == 0 {
            return;
        }
        let changed = {
            let mut keyspace_state = self.state.keyspace_state.lock();
            let anchor =
                keyspace_state.page_range().start + self.keyspace.selected_row().unwrap_or(0);
            keyspace_state.set_page_size(rows, anchor)
        };
        if !changed {
            return;
        }
        if let Some((key, _)) = self.keyspace.selected_key() {
            self.reselect_key = Some(key);
        }
        self.fetch_keys();
    }

    pub(super) fn load_keyspace(&mut self) {
        self.fetch_keys();
    }

    /// Re-run the SCAN sweep (picking up writes) while staying on the current
    /// page.
    pub(super) fn refresh_space(&mut self) {
        self.state.keyspace_state.lock().rescan();
        self.fetch_keys();
    }

    pub(super) fn fetch_keys(&mut self) {
        self.keyspace.refresh();
        if let Err(err) = self.redis_tx.send(RedisEvent::FetchKeys) {
            log::error!("Failed to send redis event: {err:?}");
            return;
        }
        self.keys_in_flight += 1;
        self.keyspace.set_loading(true);
    }

    pub(super) fn finish_keys_fetch(&mut self) {
        self.keys_in_flight = self.keys_in_flight.saturating_sub(1);
        self.keyspace.set_loading(self.keys_in_flight > 0);
    }

    pub(super) fn scroll_down(&mut self) {
        if self.mode == Mode::PubSub {
            self.pubsub_move(true);
            return;
        }
        if self.mode != Mode::KeySpace {
            return;
        }
        // When focus is in the value pane, `j`/`k` scroll the value's rows in
        // place — the selected key (and its loaded value) must not change.
        if self.keyspace.is_text_focused() {
            self.keyspace.scroll_next();
            return;
        }
        if self.keyspace.is_value_focused() {
            // On the last row of a value page, `j` turns to the next page.
            if self.keyspace.scroll_next() == ScrollOutcome::PastEnd {
                self.turn_value_page(true, 0);
            }
            return;
        }
        self.keyspace.scroll_next();
        self.keyspace.clear_selected_value();
        action::try_send_action(&self.tx, Action::RequestSelectedValue);
    }

    pub(super) fn scroll_up(&mut self) {
        if self.mode == Mode::PubSub {
            self.pubsub_move(false);
            return;
        }
        if self.mode != Mode::KeySpace {
            return;
        }
        if self.keyspace.is_text_focused() {
            self.keyspace.scroll_previous();
            return;
        }
        if self.keyspace.is_value_focused() {
            // On the first row of a value page, `k` turns back and lands on the
            // previous page's last row.
            if self.keyspace.scroll_previous() == ScrollOutcome::PastStart {
                self.turn_value_page(false, usize::MAX);
            }
            return;
        }
        self.keyspace.scroll_previous();
        self.keyspace.clear_selected_value();
        action::try_send_action(&self.tx, Action::RequestSelectedValue);
    }
}

#[cfg(test)]
mod tests {

    use crate::app::tests::{app, meta};

    #[test]
    fn load_new_keys_moves_the_page_once() {
        let mut app = app();
        *app.state.keys.lock() = Some(vec![meta("a"), meta("b")]);

        app.load_new_keys();
        assert!(app.state.keys.lock().is_none(), "page moved, not copied");
        assert!(app.keyspace.select_key_by_name("b"));

        // A second LoadKeysIntoKeySpace for the same page keeps the table.
        app.load_new_keys();
        assert!(app.keyspace.select_key_by_name("a"));
        assert!(app.keyspace.select_key_by_name("b"));
    }
}
