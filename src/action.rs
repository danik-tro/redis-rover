use serde::{Deserialize, Serialize};
use strum::Display;

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
    NextTab,
    PreviousTab,
    PreviousMode,
    EnterCmd,
    Ignore,
    ScrollDown,
    ScrollUp,
    LoadKeySpace,
    RefreshSpace,
    LoadKeysIntoKeySpace,
}
