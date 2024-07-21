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
    Info,
    KeySpace,
    Cmd,
    Popup(PopupMode),
}

impl Mode {
    pub fn is_cmd(&self) -> bool {
        *self == Mode::Cmd
    }
}
