use crate::{action::Action, command::Command};

impl From<Command> for Action {
    fn from(cmd: Command) -> Self {
        match cmd {
            Command::Quit => Self::Quit,
            Command::PreviousMode => Self::PreviousMode,
            Command::ScrollDown => Self::ScrollDown,
            Command::ScrollUp => Self::ScrollUp,
            Command::RefreshSpace => Self::RefreshSpace,
            Command::LoadNextPage => Self::LoadNextPage,
            Command::LoadPreviousPage => Self::LoadPreviousPage,
            Command::ClosePopup => Self::DiscardKeyspacePopup,
            Command::SetPattern => Self::SetKeyspaceFilter,
            Command::DeletePattern => Self::DeleteKeyspaceFilter,
            Command::EnterPopup => Self::ConfirmKeyspacePopup,
        }
    }
}
