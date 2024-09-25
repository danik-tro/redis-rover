use serde::{Deserialize, Serialize};
use strum::Display;

#[derive(Debug, Display, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Command {
    Quit,
    PreviousMode,
    ScrollDown,
    ScrollUp,
    RefreshSpace,
    LoadNextPage,
    LoadPreviousPage,
    SetPattern,
    DeletePattern,
    ClosePopup,
    EnterPopup,
}
