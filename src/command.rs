use serde::{Deserialize, Serialize};
use strum::{Display, EnumIter};

#[derive(Debug, Display, EnumIter, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Command {
    Quit,
    ScrollDown,
    ScrollUp,
    RefreshSpace,
    LoadNextPage,
    LoadPreviousPage,
    SetPattern,
    DeletePattern,
    ClosePopup,
    EnterPopup,
    ToggleHelp,
    EnterValue,
    DeleteKey,
    SetTtl,
    EditValue,
    AddKey,
    AddItem,
    ToggleDashboard,
    ToggleMetricsPause,
    ResetMetrics,
    NextChart,
    PrevChart,
    NextNode,
    PrevNode,
    ToggleAllCharts,
    SelectDb,
    ScrollLeft,
    ScrollRight,
    ToggleWrap,
    YankValue,
    YankKey,
    TogglePubSub,
    PubSubAdd,
    PubSubRemove,
    PubSubClear,
    PubSubRefresh,
    PubSubPublish,
    PubSubEnter,
    PubSubToggleNotify,
    PubSubFocus,
    PubSubTogglePause,
    ToggleReadOnly,
    OpenSortMenu,
    TypeFilter,
    ToggleTree,
    /// Move the pane divider left: a narrower key list.
    GrowLeft,
    /// Move the pane divider right: a wider key list.
    GrowRight,
    LoadAll,
    GoToOpenKey,
    /// Rename the selected key, or the field / member in a hash, set, zset.
    Rename,
    /// Copy the selected key, to this DB or another one.
    CopyKey,
    /// Mark / unmark the selected key for a bulk delete.
    ToggleMark,
    /// Mark every key on the page (again: unmark them).
    MarkAllVisible,
    ClearMarks,
    /// Edit the selected STRING in `$VISUAL` / `$EDITOR` / `vi`.
    EditExternal,
    /// Jump to the MONITOR screen.
    GoToMonitor,
    /// Jump to the Slowlog screen.
    GoToSlowlog,
    /// Jump to the Pub/Sub tab.
    GoToPubSub,
    /// Jump to the metrics dashboard.
    GoToDashboard,
    /// Leave a server screen for the key list.
    LeaveScreen,
    /// Start / stop MONITOR.
    MonitorToggle,
    /// Filter the MONITOR log.
    MonitorFilter,
    /// Pause / follow the MONITOR log.
    MonitorTogglePause,
    /// Clear the MONITOR log.
    MonitorClear,
    /// Open the selected MONITOR line.
    MonitorEnter,
    /// Load the slowlog again.
    SlowlogRefresh,
    /// Sort the slowlog by time / duration.
    SlowlogSort,
    /// Open the selected slowlog entry.
    SlowlogEnter,
    /// `SLOWLOG RESET`, after its preview.
    SlowlogReset,
    /// In a stream: entries ↔ consumer groups.
    StreamTab,
}
