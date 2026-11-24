//! Unix shell resolution (`sh -c <cmd>`).

use std::ffi::OsString;

use super::ResolvedCommand;

pub(super) fn resolve_shell(command: &str) -> ResolvedCommand {
    ResolvedCommand {
        program: OsString::from("sh"),
        args: vec![OsString::from("-c"), OsString::from(command)],
        env_remove: Vec::new(),
    }
}
