//! Windows shell resolution (`pwsh` when on `PATH`, otherwise `powershell.exe`).
//!
//! Commands are delivered via `-EncodedCommand` wrapped to force UTF-8 stdout without a BOM and
//! preserve native exit codes, avoiding `-Command`'s OEM code-page output and 0/1 exit collapse.
//! Inherited `PSModulePath` is cleared because a value inherited from PowerShell 7 puts PowerShell
//! 7 modules ahead of Windows PowerShell 5.1's built-ins and breaks cmdlets such as `Get-Acl` and
//! `Get-CimInstance`. Both PowerShell editions rebuild their own default module path when it is unset.

use std::ffi::OsString;

use super::{
    ResolvedCommand, build_powershell_shell_wrapper, encode_utf16le_base64,
    find_executable_in_paths, parse_pathext,
};

pub(super) fn resolve_shell(command: &str) -> ResolvedCommand {
    let path_var = std::env::var_os("PATH");
    let exts = parse_pathext(std::env::var_os("PATHEXT").as_deref());
    let program = find_executable_in_paths("pwsh", path_var.as_deref(), &exts)
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from("powershell.exe"));

    let wrapper = build_powershell_shell_wrapper(command);
    let encoded = encode_utf16le_base64(&wrapper);

    ResolvedCommand {
        program,
        args: vec![
            OsString::from("-NoProfile"),
            OsString::from("-NonInteractive"),
            OsString::from("-EncodedCommand"),
            OsString::from(encoded),
        ],
        env_remove: vec![OsString::from("PSModulePath")],
    }
}
