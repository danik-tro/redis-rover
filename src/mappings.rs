use crate::{action::Action, command::Command};

impl From<Command> for Action {
    fn from(cmd: Command) -> Self {
        match cmd {
            Command::Quit => Self::Quit,
        }
    }
}
