use crate::{action::Action, command::Command};

impl From<Command> for Action {
    fn from(cmd: Command) -> Self {
        match cmd {
            Command::Quit => Self::Quit,
            Command::NextTab => Self::NextTab,
            Command::PreviousTab => Self::PreviousTab,
            Command::EnterCmd => Self::EnterCmd,
            Command::PreviousMode => Self::PreviousMode,
            Command::Ignore => Self::Ignore,
            Command::ScrollDown => Self::ScrollDown,
            Command::ScrollUp => Self::ScrollUp,
            Command::RefreshSpace => Self::RefreshSpace,
            Command::LoadNextPage => Self::LoadNextPage,
            Command::LoadPreviousPage => Self::LoadPreviousPage,
        }
    }
}
