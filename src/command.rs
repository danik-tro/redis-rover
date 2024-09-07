use serde::{Deserialize, Serialize};
use strum::Display;

#[derive(Debug, Display, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Command {
    Quit,
    EnterCmd,
    PreviousMode,
    ScrollDown,
    ScrollUp,
    RefreshSpace,
    Ignore,
    LoadNextPage,
    LoadPreviousPage,
}
