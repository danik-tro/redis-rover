use super::App;
use std::time::Instant;

use super::confirm::{AfterWrite, Reselect};
use crate::redis_client::{
    event::RedisEvent,
    types::{ItemDelete, RedisType, Sort, SortKey, TypeFilter},
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
        // Keys and tree rows are taken together under the keyspace lock,
        // the way the runner writes them.
        let (page_info, page) = {
            let keyspace_state = self.state.keyspace_state.lock();
            let keys = self.state.keys.lock().take();
            let rows = self.state.page_rows.lock().take();
            (
                PageInfo::from(&*keyspace_state),
                keys.map(|keys| (keys, rows)),
            )
        };
        self.keyspace.set_page_info(page_info);
        // Move, don't copy. `None` means an earlier LoadKeysIntoKeySpace
        // already took this page (two fetches finished back to back), so the
        // table already shows it.
        if let Some((keys, rows)) = page {
            self.keyspace.set_page(keys, rows);
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

    /// A choice in the sort menu (`None` = SCAN order): the active column
    /// again reverses it. Closes the menu and shows page 1.
    pub(super) fn pick_sort(&mut self, key: Option<SortKey>) {
        self.keyspace.close_sort_menu();
        let current = self.state.keyspace_state.lock().sort();
        let next = Sort::pick(current, key);
        if next == current {
            return;
        }
        self.state.keyspace_state.lock().set_sort(next);
        self.fetch_keys();
        self.request_backfill();
    }

    /// `Shift-t`: the type checklist, ticked as the active filter.
    pub(super) fn open_type_filter(&mut self) {
        let filter = self.state.keyspace_state.lock().type_filter().clone();
        self.keyspace.open_type_menu(&filter);
    }

    /// Enter in the type checklist: show only those types (none ticked: all).
    /// Restarts the sweep from page 1, like a new pattern.
    pub(super) fn apply_type_filter(&mut self, filter: TypeFilter) {
        self.keyspace.close_type_menu();
        if self.state.keyspace_state.lock().set_type_filter(filter) {
            self.fetch_keys();
        }
    }

    /// `v`: flat list ↔ prefix tree, from page 1. The tree groups the keys
    /// loaded so far; the cursor stays on the selected key if it is shown.
    pub(super) fn toggle_tree(&mut self) {
        let separator = config::get().keyspace.separator.clone();
        self.state.keyspace_state.lock().toggle_tree(&separator);
        self.reselect_key = self.keyspace.selected_key().map(|(key, _)| key);
        self.fetch_keys();
    }

    /// Row of the list (all pages) the cursor is on.
    fn cursor_position(&self) -> Option<usize> {
        let row = self.keyspace.selected_row()?;
        Some(self.state.keyspace_state.lock().page_range().start + row)
    }

    /// `←` / `→` on the key list in tree mode: close the group (or the one
    /// the cursor is in) / open it. Returns `false` when it isn't for the
    /// tree (flat list, focus on the value), so the arrow pans instead.
    pub(super) fn fold_tree(&mut self, open: bool) -> bool {
        if !self.keyspace.is_keys_focused() || !self.state.keyspace_state.lock().is_tree() {
            return false;
        }
        let Some(position) = self.cursor_position() else {
            return true;
        };
        let prefix = {
            let mut keyspace_state = self.state.keyspace_state.lock();
            if open {
                keyspace_state.set_group(position, Some(true))
            } else {
                keyspace_state.fold(position)
            }
        };
        if let Some(prefix) = prefix {
            self.reselect_key = Some(prefix);
            self.fetch_keys();
        }
        true
    }

    /// Enter on a group row: open or close it. Returns `false` for a key.
    pub(super) fn toggle_group(&mut self) -> bool {
        if !self.keyspace.is_keys_focused() || self.keyspace.selected_group().is_none() {
            return false;
        }
        let Some(position) = self.cursor_position() else {
            return false;
        };
        let prefix = self.state.keyspace_state.lock().set_group(position, None);
        if let Some(prefix) = prefix {
            self.reselect_key = Some(prefix);
            self.fetch_keys();
        }
        true
    }

    /// Start the metadata backfill if the sort still lacks some (the runner
    /// ignores it while one runs).
    pub(super) fn request_backfill(&mut self) {
        let missing = self
            .state
            .keyspace_state
            .lock()
            .meta_progress()
            .is_some_and(|(known, loaded)| known < loaded);
        if missing && !self.backfill_paused() {
            self.send_redis_event(RedisEvent::BackfillMeta);
        }
    }

    /// The backfill waits while the user is on the dashboard or Pub/Sub.
    fn backfill_paused(&self) -> bool {
        matches!(self.mode, Mode::Dashboard | Mode::PubSub)
    }

    /// The mode changed: pause the backfill on another screen, resume it on
    /// the way back.
    pub(super) fn sync_backfill_pause(&mut self) {
        let paused = self.backfill_paused();
        self.state.keyspace_state.lock().set_backfill_paused(paused);
        if !paused {
            self.request_backfill();
        }
    }

    /// The backfill stored a batch: update the progress note and, if the
    /// page now holds other keys, show them. A page whose keys all have
    /// metadata is drawn from it with no round trip, keeping the cursor (and
    /// the open value) on the same key; otherwise the page is fetched again.
    pub(super) fn meta_backfilled(&mut self) {
        let (info, names, rows, tree_rows) = {
            let state = self.state.keyspace_state.lock();
            (
                PageInfo::from(&*state),
                state.page_keys(),
                state.page_from_meta(),
                state.page_rows(),
            )
        };
        self.keyspace.set_page_info(info);
        if self.keys_in_flight > 0 || self.keyspace.shows_keys(&names) {
            return;
        }
        let selected = self.keyspace.selected_key().map(|(key, _)| key);
        if let Some(rows) = rows {
            self.keyspace.set_page(rows, tree_rows);
            let kept = selected
                .as_deref()
                .is_some_and(|key| self.keyspace.select_key_by_name(key));
            if !kept {
                self.keyspace.refresh();
                self.keyspace.clear_selected_value();
            }
        } else {
            self.reselect_key = selected;
            self.fetch_keys();
        }
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

    /// What the runner does for a `FetchKeys`, without Redis: build the
    /// order, hand over the page (and its tree rows), load it.
    fn serve_page(app: &mut super::App) {
        {
            let mut keyspace_state = app.state.keyspace_state.lock();
            keyspace_state.rebuild_view();
            *app.state.page_rows.lock() = keyspace_state.page_rows();
            *app.state.keys.lock() =
                Some(keyspace_state.page_keys().iter().map(|k| meta(k)).collect());
        }
        app.load_new_keys();
    }

    #[test]
    fn tree_arrows_and_enter_fold_groups_on_the_key_list() {
        use crate::action::Action;

        let mut app = app();
        app.state.keyspace_state.lock().merge_batch(
            0,
            ["user:1:name", "user:1:email", "order:9", "solo"]
                .map(String::from)
                .to_vec(),
        );
        serve_page(&mut app);
        assert!(!app.fold_tree(true), "flat list: the arrows pan");

        app.update(&Action::ToggleTree);
        serve_page(&mut app);
        assert!(app.keyspace.select_key_by_name("user:"));
        assert_eq!(app.keyspace.selected_key(), None, "a group is not a key");
        assert_eq!(app.keyspace.selected_group(), Some(("user:", false)));

        app.update(&Action::ScrollRight);
        serve_page(&mut app);
        assert_eq!(
            app.keyspace.selected_group(),
            Some(("user:", true)),
            "cursor kept"
        );

        // Enter on the group closes it; on a key it doesn't.
        app.update(&Action::EnterValue);
        serve_page(&mut app);
        assert_eq!(app.keyspace.selected_group(), Some(("user:", false)));

        app.update(&Action::ScrollRight);
        serve_page(&mut app);
        assert!(app.keyspace.select_key_by_name("user:1:"));
        app.update(&Action::ScrollRight);
        serve_page(&mut app);
        assert!(app.keyspace.select_key_by_name("user:1:name"));
        assert!(!app.toggle_group(), "Enter on a key opens it as before");

        // ← on a key closes its group and moves the cursor there.
        app.update(&Action::ScrollLeft);
        serve_page(&mut app);
        assert_eq!(app.keyspace.selected_group(), Some(("user:1:", false)));

        // Back to the flat list.
        app.update(&Action::ToggleTree);
        serve_page(&mut app);
        assert_eq!(app.keyspace.selected_group(), None);
        assert!(app.keyspace.select_key_by_name("order:9"));
    }

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

    #[test]
    fn o_opens_the_sort_menu_and_a_letter_picks() {
        use crate::app::tests::press;
        use crate::redis_client::types::SortKey;
        use crossterm::event::{KeyCode, KeyModifiers};

        let (mut app, _cmds, _redis_rx) = crate::app::tests::app_with_channels();
        app.mode = crate::mode::Mode::KeySpace;
        let none = KeyModifiers::empty();

        press(&mut app, KeyCode::Char('o'), none);
        assert!(app.keyspace.is_sort_menu_open());
        press(&mut app, KeyCode::Char('j'), none);
        assert!(app.keyspace.is_sort_menu_open(), "other keys do nothing");
        press(&mut app, KeyCode::Char('s'), none);
        assert!(!app.keyspace.is_sort_menu_open());
        let sort = app.state.keyspace_state.lock().sort().unwrap();
        assert_eq!((sort.key, sort.descending), (SortKey::Size, true));

        press(&mut app, KeyCode::Char('o'), none);
        press(&mut app, KeyCode::Esc, none);
        assert!(!app.keyspace.is_sort_menu_open());
        assert!(app.state.keyspace_state.lock().sort().is_some());

        press(&mut app, KeyCode::Char('o'), none);
        press(&mut app, KeyCode::Char('x'), none);
        assert_eq!(app.state.keyspace_state.lock().sort(), None);
    }

    #[test]
    fn shift_t_opens_the_type_checklist_and_enter_applies() {
        use crate::app::tests::press;
        use crate::redis_client::{event::RedisEvent, types::TypeFilter};
        use crossterm::event::{KeyCode, KeyModifiers};

        let (mut app, _cmds, mut redis_rx) = crate::app::tests::app_with_channels();
        app.mode = crate::mode::Mode::KeySpace;
        let none = KeyModifiers::empty();

        press(&mut app, KeyCode::Char('T'), KeyModifiers::SHIFT);
        assert!(app.keyspace.is_type_menu_open());
        press(&mut app, KeyCode::Esc, none);
        assert!(!app.keyspace.is_type_menu_open());
        assert!(redis_rx.try_recv().is_err(), "Esc changes nothing");

        // string, list, set, zset, hash: tick hash.
        press(&mut app, KeyCode::Char('T'), KeyModifiers::SHIFT);
        for _ in 0..4 {
            press(&mut app, KeyCode::Char('j'), none);
        }
        press(&mut app, KeyCode::Char(' '), none);
        assert!(app.keyspace.is_type_menu_open(), "Space only ticks");
        press(&mut app, KeyCode::Enter, none);
        assert!(!app.keyspace.is_type_menu_open());
        assert_eq!(
            app.state.keyspace_state.lock().type_filter(),
            &TypeFilter::new(["hash"])
        );
        assert!(matches!(redis_rx.try_recv(), Ok(RedisEvent::FetchKeys)));

        // Reopened ticked as applied; x + Enter shows every type again.
        press(&mut app, KeyCode::Char('T'), KeyModifiers::SHIFT);
        press(&mut app, KeyCode::Char('x'), none);
        press(&mut app, KeyCode::Enter, none);
        assert!(app.state.keyspace_state.lock().type_filter().is_empty());
        assert!(matches!(redis_rx.try_recv(), Ok(RedisEvent::FetchKeys)));
    }

    #[test]
    fn sort_menu_picks_reverses_resets_and_starts_the_backfill() {
        use crate::redis_client::{event::RedisEvent, types::SortKey};

        let (mut app, _cmds, mut redis_rx) = crate::app::tests::app_with_channels();
        app.state
            .keyspace_state
            .lock()
            .merge_batch(0, vec!["a".into(), "b".into()]);

        app.keyspace.open_sort_menu();
        app.pick_sort(Some(SortKey::Name));
        assert!(!app.keyspace.is_sort_menu_open());
        let sort = app.state.keyspace_state.lock().sort().unwrap();
        assert_eq!((sort.key, sort.descending), (SortKey::Name, false));
        assert!(matches!(redis_rx.try_recv(), Ok(RedisEvent::FetchKeys)));
        assert!(
            redis_rx.try_recv().is_err(),
            "a name sort needs no metadata"
        );

        app.pick_sort(Some(SortKey::Name));
        assert!(app.state.keyspace_state.lock().sort().unwrap().descending);
        while redis_rx.try_recv().is_ok() {}
        app.pick_sort(Some(SortKey::Ttl));
        assert!(matches!(redis_rx.try_recv(), Ok(RedisEvent::FetchKeys)));
        assert!(matches!(redis_rx.try_recv(), Ok(RedisEvent::BackfillMeta)));

        // On the dashboard the backfill waits; back on the keys it resumes.
        app.mode = crate::mode::Mode::Dashboard;
        app.sync_backfill_pause();
        assert!(app
            .state
            .keyspace_state
            .lock()
            .backfill_request(1)
            .is_none());
        app.mode = crate::mode::Mode::KeySpace;
        app.sync_backfill_pause();
        assert!(matches!(redis_rx.try_recv(), Ok(RedisEvent::BackfillMeta)));

        while redis_rx.try_recv().is_ok() {}
        app.pick_sort(None);
        assert_eq!(app.state.keyspace_state.lock().sort(), None);
        assert!(matches!(redis_rx.try_recv(), Ok(RedisEvent::FetchKeys)));
        app.pick_sort(None);
        assert!(redis_rx.try_recv().is_err(), "already in SCAN order");
    }
}
