//! Windows shell resolution (`pwsh` when on `PATH`, otherwise `powershell.exe`).
//!
//! Inherited `PSModulePath` is cleared because a value inherited from PowerShell 7 puts PowerShell
//! 7 modules ahead of Windows PowerShell 5.1's built-ins and breaks cmdlets such as `Get-Acl` and
//! `Get-CimInstance`. Both PowerShell editions rebuild their own default module path when it is unset.

use std::ffi::OsString;

use super::{ResolvedCommand, find_executable_in_paths, parse_pathext};

pub(super) fn resolve_shell(command: &str) -> ResolvedCommand {
    let path_var = std::env::var_os("PATH");
    let exts = parse_pathext(std::env::var_os("PATHEXT").as_deref());
    let program = find_executable_in_paths("pwsh", path_var.as_deref(), &exts)
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from("powershell.exe"));

    ResolvedCommand {
        program,
        args: vec![
            OsString::from("-NoProfile"),
            OsString::from("-NonInteractive"),
            OsString::from("-Command"),
            OsString::from(command),
        ],
        env_remove: vec![OsString::from("PSModulePath")],
    }
}
