use super::App;

use crate::redis_client::event::RedisEvent;

use crate::mode::Mode;

impl App {
    /// `s`: open the DB picker. Cluster mode only has db 0.
    pub(super) fn open_db_picker(&mut self) {
        if self.mode != Mode::KeySpace {
            return;
        }
        if self.cluster_enabled() {
            self.keyspace.show_notice("Cluster mode only has db 0.");
            return;
        }
        let keyspace = self
            .state
            .info
            .lock()
            .as_ref()
            .map(|info| info.keyspace.clone())
            .unwrap_or_default();
        let db_count = *self.state.db_count.lock();
        let current = *self.state.db.lock();
        self.db_picker.open(db_count, current, &keyspace);
    }

    /// Enter in the DB picker: ask the runner to switch. The old DB's keys are
    /// cleared right away so nothing stale can be edited; the runner answers
    /// with `DbSwitched` (or an error and a reload of the old DB's page).
    pub(super) fn confirm_db_picker(&mut self) {
        let selected = self.db_picker.selected();
        self.db_picker.close();
        let Some(db) = selected else {
            return;
        };
        if db == *self.state.db.lock() {
            return;
        }
        self.reselect_key = None;
        self.restore_value_row = None;
        self.restore_value_pending = false;
        self.page_turn_row = None;
        self.keyspace.clear_for_db_switch();
        self.send_redis_event(RedisEvent::SelectDb(db));
        self.keys_in_flight += 1;
        self.keyspace.set_loading(true);
    }
}
