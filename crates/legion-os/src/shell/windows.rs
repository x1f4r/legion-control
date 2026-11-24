//! Windows shell resolution (`pwsh` when on `PATH`, otherwise `powershell.exe`).
//!
//! Commands are delivered via `-EncodedCommand` wrapped to force UTF-8 stdout without a BOM and
//! preserve native exit codes, avoiding `-Command`'s OEM code-page output and 0/1 exit collapse.
//! Inherited `PSModulePath` is cleared because a value inherited from PowerShell 7 puts PowerShell
//! 7 modules ahead of Windows PowerShell 5.1's built-ins and breaks cmdlets such as `Get-Acl` and
//! `Get-CimInstance`. Both PowerShell editions rebuild their own default module path when it is unset.

use std::ffi::OsString;

use crate::Result;

use super::{ResolvedCommand, find_executable_in_paths, parse_pathext, resolve_powershell_command};

pub(super) fn resolve_shell(command: &str) -> Result<ResolvedCommand> {
    let path_var = std::env::var_os("PATH");
    let exts = parse_pathext(std::env::var_os("PATHEXT").as_deref());
    let program = find_executable_in_paths("pwsh", path_var.as_deref(), &exts)
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from("powershell.exe"));

    resolve_powershell_command(program, command, &std::env::temp_dir())
}
