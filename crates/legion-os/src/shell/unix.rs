//! Unix shell resolution (`sh -c <cmd>`).

use std::ffi::OsString;

use crate::Result;

use super::ResolvedCommand;

pub(super) fn resolve_shell(command: &str) -> Result<ResolvedCommand> {
    Ok(ResolvedCommand::new(
        OsString::from("sh"),
        vec![OsString::from("-c"), OsString::from(command)],
        Vec::new(),
    ))
}
