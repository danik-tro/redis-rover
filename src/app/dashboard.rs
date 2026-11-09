use super::App;

use crate::mode::Mode;

impl App {
    pub(super) fn toggle_dashboard(&mut self) {
        match self.mode {
            Mode::Dashboard => self.switch_to_previous_mode(),
            Mode::KeySpace => {
                self.previous_mode = Some(self.mode);
                self.mode = Mode::Dashboard;
                self.sync_hint();
            }
            _ => {}
        }
    }

    pub(super) fn reset_metrics(&mut self) {
        self.state.metrics.lock().reset();
        // The footer's CPU figure comes from the metrics.
        self.state
            .info_seq
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        self.dashboard.unpause();
    }
}
