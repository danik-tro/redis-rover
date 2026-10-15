use serde::{Deserialize, Serialize};
use strum::Display;

#[derive(Debug, Display, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    ToggleAllCharts,
}
