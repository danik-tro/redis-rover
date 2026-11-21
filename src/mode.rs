use serde::{Deserialize, Serialize};

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PopupMode {
    Error,
    Info,
    Confirm,
}

#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Mode {
    Common,
    #[default]
    KeySpace,
    Dashboard,
    PubSub,
    Monitor,
    Slowlog,
    Popup(PopupMode),
}

impl Mode {
    /// A server screen hides the key list, so work that only serves the list
    /// (metadata backfill, live tracking of the open key) waits meanwhile.
    #[must_use]
    pub fn hides_key_list(self) -> bool {
        match self {
            Mode::Dashboard | Mode::PubSub | Mode::Monitor | Mode::Slowlog => true,
            Mode::Common | Mode::KeySpace | Mode::Popup(_) => false,
        }
    }
}
