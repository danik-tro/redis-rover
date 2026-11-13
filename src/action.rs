use serde::{Deserialize, Serialize};
use strum::{Display, EnumIter};
use tokio::sync::mpsc::{error::TrySendError, Sender};

use crate::redis_client::live::{KeyChange, Liveness};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Display, Deserialize, EnumIter)]
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
    /// Pan a focused text value left / right (wrap off).
    ScrollLeft,
    ScrollRight,
    /// Toggle line wrapping of text values.
    ToggleWrap,
    /// Copy the selected value / element to the clipboard.
    YankValue,
    /// Copy the selected key name to the clipboard.
    YankKey,
    /// A full value fetched for `YankValue` is waiting in `SharedState::yank`.
    YankReady,
    /// Fetching a full value for `YankValue` failed.
    YankFailed(String),
    LoadKeySpace,
    RefreshSpace,
    LoadKeysIntoKeySpace,
    /// A keyspace fetch ended without new keys (failed, or superseded by a
    /// newer fetch). Only clears the loading indicator.
    KeysLoadAborted,
    /// The metadata backfill stored another batch: the sort order (and the
    /// page) may have changed.
    KeyMetaBackfilled,
    /// `Shift-a`: load the whole keyspace in the background.
    LoadAll,
    /// A running load stored more keys: this many are loaded now.
    LoadAllProgress(usize),
    /// The load stopped by itself: the sweep ended (`complete`), the key
    /// it looked for is loaded, or it failed.
    LoadAllDone {
        complete: bool,
    },
    /// `g o`: move the cursor back to the key whose value was opened.
    GoToOpenKey,
    /// `o`: open the sort menu.
    OpenSortMenu,
    /// `Shift-t`: open the type filter checklist.
    OpenTypeFilter,
    /// `v`: flat list ↔ prefix tree.
    ToggleTree,
    RequestSelectedValue,
    LoadSelectedValueIntoView,
    /// A value fetch failed. Only clears the value's loading indicator.
    ValueLoadFailed,
    LoadNextPage,
    LoadPreviousPage,
    SetKeyspaceFilter,
    DeleteKeyspaceFilter,
    ConfirmKeyspacePopup,
    DiscardKeyspacePopup,
    EnterValue,
    RequestDeleteKey,
    RequestSetTtl,
    RequestEditValue,
    RequestAddKey,
    RequestAddItem,
    ToggleDashboard,
    ToggleMetricsPause,
    ResetMetrics,
    NextChart,
    PrevChart,
    ToggleAllCharts,
    OpenDbPicker,
    /// The runner connected to the newly selected DB; reload the keyspace.
    DbSwitched,
    /// Another client changed (or deleted) the open key; the live task
    /// already re-armed it.
    OpenKeyChanged {
        key: String,
        change: KeyChange,
    },
    /// The open key's liveness changed (live, polling, not live).
    LiveStatus(Liveness),
    /// Live match count for the filter popup. `complete` is false when the
    /// count stopped early (budget hit), so `count` is a lower bound.
    FilterMatchCount {
        pattern: String,
        count: usize,
        complete: bool,
    },
    /// Open / leave the Pub/Sub tab.
    TogglePubSub,
    /// Open the add-subscription popup.
    PubSubAdd,
    /// Unsubscribe the selected subscription and drop its messages.
    PubSubRemove,
    /// Clear the selected subscription's messages and stats.
    PubSubClear,
    /// Re-list the server's channels (`PUBSUB CHANNELS`).
    PubSubRefresh,
    /// Open the publish popup.
    PubSubPublish,
    /// Enter in the tab: subscribe to the selected server channel.
    PubSubEnter,
    /// `N`: toggle notifications of the selected subscription (also un-mutes).
    PubSubToggleNotify,
    /// Tab in the tab: move focus between the list and the log.
    PubSubFocus,
    /// Space in the tab: pause / follow the log.
    PubSubTogglePause,
    /// `R`: toggle read-only (lifting it on Prod asks first).
    ToggleReadOnly,
    /// A server channel listing is waiting in `SharedState::pubsub`.
    PubSubChannelsLoaded,
    /// `PUBLISH` went through and reached `receivers` subscribers.
    Published {
        channel: String,
        receivers: i64,
    },
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
