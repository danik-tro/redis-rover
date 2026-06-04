use serde::{Deserialize, Serialize};
use strum::Display;
use tokio::sync::mpsc::{error::TrySendError, Sender};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Display, Deserialize)]
pub enum Action {
    Tick,
    Render,
    Resize(u16, u16),
    Refresh,
    Error(String),
    Help,
    // Commands actions
    Quit,
    PreviousMode,
    ScrollDown,
    ScrollUp,
    LoadKeySpace,
    RefreshSpace,
    LoadKeysIntoKeySpace,
    RequestSelectedValue,
    LoadSelectedValueIntoView,
    LoadNextPage,
    LoadPreviousPage,
    SetKeyspaceFilter,
    DeleteKeyspaceFilter,
    ConfirmKeyspacePopup,
    DiscardKeyspacePopup,
}

impl Action {
    /// `Render`/`Tick` are pushed at the frame/tick rate and are idempotent, so
    /// dropping one when the channel is full is harmless — it re-fires on the
    /// next interval.
    fn is_coalescible(&self) -> bool {
        matches!(self, Action::Render | Action::Tick)
    }
}

/// Sends an action over the bounded channel without blocking.
///
/// On a full channel, coalescible actions (`Render`/`Tick`) are silently
/// dropped; any other dropped action is logged as a warning since it may carry
/// state that won't re-fire on its own.
pub fn try_send_action(tx: &Sender<Action>, action: Action) {
    if let Err(err) = tx.try_send(action) {
        match err {
            TrySendError::Full(action) => {
                if action.is_coalescible() {
                    return;
                }
                log::warn!("action channel full, dropped action: {action:?}");
            }
            TrySendError::Closed(action) => {
                log::debug!("action channel closed, dropped action: {action:?}");
            }
        }
    }
}
