//! Current OS account identity, Windows Administrators-group checks, and SSH key-file ACLs.
//!
//! Isolates the platform differences needed when managing `authorized_keys` files across Linux,
//! macOS, and Windows.

use std::io;
use std::path::{Path, PathBuf};

use crate::{OsError, Result};

/// Returns the effective Unix user ID of the current process, or `None` on non-Unix platforms.
pub fn effective_uid() -> Option<u32> {
    #[cfg(unix)]
    {
        Some(nix::unistd::Uid::effective().as_raw())
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Looks up the Unix account name for `uid` in the system user database, returning `None` if no
/// entry exists or on non-Unix platforms.
pub fn username_for_uid(uid: u32) -> Option<String> {
    #[cfg(unix)]
    {
        nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
            .ok()
            .flatten()
            .map(|u| u.name)
            .filter(|name| !name.is_empty())
    }
    #[cfg(not(unix))]
    {
        let _ = uid;
        None
    }
}

/// Returns the owning Unix user ID of `path` (following symlinks), or `Ok(None)` on non-Unix.
pub fn path_owner_uid(path: &Path) -> io::Result<Option<u32>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(Some(std::fs::metadata(path)?.uid()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

/// Returns the operating-system account name that the current process runs as.
///
/// On Unix, looks up the effective UID in the system user database first (falling back to `USER`
/// and `LOGNAME` only when the lookup yields no entry). On Windows, resolves the current process
/// token's user SID via `OpenProcessToken` + `GetTokenInformation(TokenUser)` + `LookupAccountSidW`
/// first, falling back to `USERNAME` only when the Win32 API fails.
pub fn current_username() -> Result<String> {
    #[cfg(unix)]
    {
        if let Ok(Some(user)) = nix::unistd::User::from_uid(nix::unistd::Uid::effective())
            && !user.name.is_empty()
        {
            return Ok(user.name);
        }
        for var in ["USER", "LOGNAME"] {
            if let Ok(value) = std::env::var(var) {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    return Ok(trimmed.to_owned());
                }
            }
        }
        Err(OsError::Platform {
            detail: "cannot determine the current Unix account name".to_owned(),
        })
    }
    #[cfg(windows)]
    {
        if let Some(name) = windows_impl::token_username() {
            return Ok(name);
        }
        if let Ok(value) = std::env::var("USERNAME") {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Ok(trimmed.to_owned());
            }
        }
        Err(OsError::Platform {
            detail:
                "cannot determine the current Windows account name from process token or USERNAME"
                    .to_owned(),
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(OsError::Unsupported {
            detail: "determining the current account name is not supported on this platform"
                .to_owned(),
        })
    }
}

/// Returns the Windows `ProgramData` directory via `SHGetKnownFolderPath(FOLDERID_ProgramData)`
/// (falling back to the `ProgramData` environment variable and then `C:\ProgramData` only when
/// the Win32 Shell API fails).
///
/// Always returns `None` on non-Windows platforms.
pub fn windows_program_data_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        Some(
            windows_impl::known_folder_program_data()
                .or_else(|| {
                    std::env::var_os("ProgramData")
                        .filter(|v| !v.is_empty())
                        .map(PathBuf::from)
                })
                .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData")),
        )
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Returns `true` on Windows when the current account is a member of the local `Administrators`
/// group (`BUILTIN\Administrators`, `S-1-5-32-544`), including when running under an unelevated
/// UAC token where the group is present as deny-only.
///
/// Always returns `false` on non-Windows platforms.
pub fn is_windows_administrator() -> bool {
    #[cfg(windows)]
    {
        windows_impl::is_in_administrators_group()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Shell Windows OpenSSH `sshd` invokes to execute `authorized_keys` forced commands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowsSshShell {
    /// `cmd.exe` (Windows OpenSSH's built-in default when `DefaultShell` is unset or empty).
    Cmd,
    /// Windows PowerShell (`powershell.exe`) or PowerShell 7 (`pwsh.exe`).
    PowerShell,
    /// POSIX sh-compatible shell (such as Git Bash's `bash.exe`, `sh.exe`, `dash.exe`, `ash.exe`,
    /// or `zsh.exe`).
    Posix,
    /// Any other configured `DefaultShell` whose quoting syntax is not recognised.
    Unknown(String),
}

/// Classifies a Windows OpenSSH `HKLM\SOFTWARE\OpenSSH\DefaultShell` registry string into
/// [`WindowsSshShell`].
///
/// - Absent, empty, or `cmd` / `cmd.exe` -> [`WindowsSshShell::Cmd`].
/// - `powershell` / `powershell.exe` / `pwsh` / `pwsh.exe` -> [`WindowsSshShell::PowerShell`].
/// - `sh` / `bash` / `dash` / `ash` / `zsh` / `ksh` / `mksh` (with or without `.exe`) ->
///   [`WindowsSshShell::Posix`].
/// - Anything else -> [`WindowsSshShell::Unknown`].
pub fn classify_windows_default_shell(value: Option<&str>) -> WindowsSshShell {
    let Some(raw) = value else {
        return WindowsSshShell::Cmd;
    };
    let trimmed = raw.trim().trim_matches('"').trim();
    if trimmed.is_empty() {
        return WindowsSshShell::Cmd;
    }
    let base = trimmed
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let stem = base.strip_suffix(".exe").unwrap_or(&base);
    match stem {
        "cmd" => WindowsSshShell::Cmd,
        "powershell" | "pwsh" => WindowsSshShell::PowerShell,
        "sh" | "bash" | "dash" | "ash" | "zsh" | "ksh" | "mksh" => WindowsSshShell::Posix,
        _ => WindowsSshShell::Unknown(trimmed.to_owned()),
    }
}

/// Reads `HKLM\SOFTWARE\OpenSSH\DefaultShell` on Windows and returns which shell Windows OpenSSH
/// uses for forced commands (`Cmd` when the registry value or key is absent).
///
/// Always returns `Ok(WindowsSshShell::Cmd)` on non-Windows platforms.
pub fn windows_sshd_default_shell() -> Result<WindowsSshShell> {
    #[cfg(windows)]
    {
        let value = windows_impl::read_openssh_default_shell()?;
        Ok(classify_windows_default_shell(value.as_deref()))
    }
    #[cfg(not(windows))]
    {
        Ok(WindowsSshShell::Cmd)
    }
}

/// Applies the ACL and ownership that Windows OpenSSH `sshd` demands for
/// `administrators_authorized_keys`: disables inherited permissions, grants Full Control solely to
/// `BUILTIN\Administrators` (`S-1-5-32-544`) and `NT AUTHORITY\SYSTEM` (`S-1-5-18`), and sets the
/// file owner to `BUILTIN\Administrators` (`S-1-5-32-544`).
///
/// Uses locale-independent SID strings so non-English Windows installations are handled identically.
/// No-op on non-Windows platforms.
pub fn apply_windows_admin_ssh_acl(path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        windows_impl::apply_admin_ssh_acl(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(windows)]
mod windows_impl {
    use std::ffi::{OsString, c_void};
    use std::os::windows::ffi::OsStringExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use windows::Win32::Foundation::{
        CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA,
        ERROR_PATH_NOT_FOUND, ERROR_SUCCESS, HANDLE,
    };
    use windows::Win32::Security::{
        CreateWellKnownSid, EqualSid, GetTokenInformation, LookupAccountSidW, PSID,
        SECURITY_MAX_SID_SIZE, SID_NAME_USE, TOKEN_GROUPS, TOKEN_QUERY, TOKEN_USER, TokenGroups,
        TokenUser, WinBuiltinAdministratorsSid,
    };
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::Win32::UI::Shell::{FOLDERID_ProgramData, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
    use windows::core::{PCWSTR, PWSTR};

    use crate::{OsError, Result};

    struct TokenGuard(HANDLE);
    #[allow(unsafe_code)]
    impl Drop for TokenGuard {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                // SAFETY: `self.0` was obtained from `OpenProcessToken` and is closed once.
                let _ = unsafe { CloseHandle(self.0) };
            }
        }
    }

    #[allow(unsafe_code)]
    pub(super) fn token_username() -> Option<String> {
        let mut token = HANDLE::default();
        // SAFETY: `GetCurrentProcess()` returns the calling process pseudo-handle and `token` is
        // immediately wrapped in `TokenGuard`.
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.ok()?;
        let _guard = TokenGuard(token);

        let mut needed: u32 = 0;
        // SAFETY: Querying required `TokenUser` buffer size with a null output pointer.
        let first = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut needed) };
        if let Err(err) = first
            && err.code() != ERROR_INSUFFICIENT_BUFFER.to_hresult()
        {
            return None;
        }
        if needed == 0 {
            return None;
        }
        let words = (needed as usize).div_ceil(std::mem::size_of::<usize>());
        let mut buf = vec![0_usize; words];
        // SAFETY: `buf` is `usize`-aligned with at least `needed` bytes of writable storage.
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                Some(buf.as_mut_ptr().cast::<c_void>()),
                needed,
                &mut needed,
            )
        }
        .ok()?;

        // SAFETY: `GetTokenInformation(TokenUser)` populated `TOKEN_USER` at the start of `buf`.
        let user_sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        if user_sid.0.is_null() {
            return None;
        }

        let mut name_len: u32 = 0;
        let mut domain_len: u32 = 0;
        let mut sid_use = SID_NAME_USE::default();
        // SAFETY: Querying required buffer lengths for `LookupAccountSidW` with null output pointers.
        let query = unsafe {
            LookupAccountSidW(
                PCWSTR::null(),
                user_sid,
                None,
                &mut name_len,
                None,
                &mut domain_len,
                &mut sid_use,
            )
        };
        if let Err(err) = query
            && err.code() != ERROR_INSUFFICIENT_BUFFER.to_hresult()
        {
            return None;
        }
        if name_len == 0 {
            return None;
        }

        let mut name_buf = vec![0_u16; name_len as usize];
        let mut domain_buf = vec![0_u16; (domain_len as usize).max(1)];
        let mut domain_cap = u32::try_from(domain_buf.len()).unwrap_or(u32::MAX);
        // SAFETY: `name_buf` and `domain_buf` hold `name_len` and `domain_cap` UTF-16 code units.
        unsafe {
            LookupAccountSidW(
                PCWSTR::null(),
                user_sid,
                Some(PWSTR::from_raw(name_buf.as_mut_ptr())),
                &mut name_len,
                Some(PWSTR::from_raw(domain_buf.as_mut_ptr())),
                &mut domain_cap,
                &mut sid_use,
            )
        }
        .ok()?;

        let slice = name_buf.get(..(name_len as usize))?;
        let trimmed_len = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
        let name = String::from_utf16_lossy(&slice[..trimmed_len]);
        let trimmed = name.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    }

    #[allow(unsafe_code)]
    pub(super) fn known_folder_program_data() -> Option<PathBuf> {
        // SAFETY: `SHGetKnownFolderPath` allocates a NUL-terminated UTF-16 string with the COM
        // task allocator; the guard below frees it unconditionally via `CoTaskMemFree`.
        let raw =
            unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramData, KF_FLAG_DEFAULT, None) }.ok()?;
        struct CoTaskStr(PWSTR);
        impl Drop for CoTaskStr {
            fn drop(&mut self) {
                if !self.0.is_null() {
                    // SAFETY: `self.0` was allocated by `SHGetKnownFolderPath` and is freed once.
                    unsafe { CoTaskMemFree(Some(self.0.0.cast::<c_void>())) };
                }
            }
        }
        let guard = CoTaskStr(raw);
        if guard.0.is_null() {
            return None;
        }
        // SAFETY: `SHGetKnownFolderPath` returns a valid NUL-terminated UTF-16 string.
        let wide = unsafe { guard.0.as_wide() };
        if wide.is_empty() {
            return None;
        }
        Some(PathBuf::from(OsString::from_wide(wide)))
    }

    #[allow(unsafe_code)]
    pub(super) fn is_in_administrators_group() -> bool {
        let mut token = HANDLE::default();
        // SAFETY: `GetCurrentProcess()` returns the pseudo-handle for the calling process, and
        // `token` is a valid out-pointer closed unconditionally below.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.is_err() {
            return false;
        }
        let _guard = TokenGuard(token);

        let mut admin_sid = [0_u8; SECURITY_MAX_SID_SIZE as usize];
        let mut admin_sid_len = u32::try_from(admin_sid.len()).unwrap_or(SECURITY_MAX_SID_SIZE);
        // SAFETY: `admin_sid` is sized to `SECURITY_MAX_SID_SIZE` and `admin_sid_len` holds its
        // exact byte capacity.
        if unsafe {
            CreateWellKnownSid(
                WinBuiltinAdministratorsSid,
                None,
                Some(PSID(admin_sid.as_mut_ptr().cast::<c_void>())),
                &mut admin_sid_len,
            )
        }
        .is_err()
        {
            return false;
        }
        let admin_psid = PSID(admin_sid.as_mut_ptr().cast::<c_void>());

        let mut needed: u32 = 0;
        // SAFETY: Querying buffer size with a null output pointer is the documented Win32 pattern.
        let first = unsafe { GetTokenInformation(token, TokenGroups, None, 0, &mut needed) };
        if let Err(err) = first
            && err.code() != ERROR_INSUFFICIENT_BUFFER.to_hresult()
        {
            return false;
        }
        if needed == 0 {
            return false;
        }
        // Allocate as `usize`-aligned words so the buffer meets `TOKEN_GROUPS` alignment.
        let word_count = (needed as usize).div_ceil(std::mem::size_of::<usize>());
        let mut buf = vec![0_usize; word_count];
        // SAFETY: `buf` is at least `needed` bytes and properly aligned for `TOKEN_GROUPS`.
        if unsafe {
            GetTokenInformation(
                token,
                TokenGroups,
                Some(buf.as_mut_ptr().cast::<c_void>()),
                needed,
                &mut needed,
            )
        }
        .is_err()
        {
            return false;
        }

        // SAFETY: `GetTokenInformation(TokenGroups)` succeeded and initialized `TOKEN_GROUPS` with
        // `GroupCount` elements in the flexible `Groups` array inside `buf`.
        let groups_ptr = buf.as_ptr().cast::<TOKEN_GROUPS>();
        let count = unsafe { (*groups_ptr).GroupCount } as usize;
        let first_group = unsafe {
            std::ptr::addr_of!((*groups_ptr).Groups)
                .cast::<windows::Win32::Security::SID_AND_ATTRIBUTES>()
        };
        // Check group membership regardless of whether UAC has temporarily marked the group
        // deny-only: OpenSSH `Match Group administrators` matches account membership.
        for i in 0..count {
            let entry = unsafe { *first_group.add(i) };
            if !entry.Sid.0.is_null() && unsafe { EqualSid(entry.Sid, admin_psid) }.is_ok() {
                return true;
            }
        }
        false
    }

    #[allow(unsafe_code)]
    pub(super) fn read_openssh_default_shell() -> Result<Option<String>> {
        let subkey: Vec<u16> = "SOFTWARE\\OpenSSH\0".encode_utf16().collect();
        let value_name: Vec<u16> = "DefaultShell\0".encode_utf16().collect();
        // `RRF_RT_REG_SZ` alone accepts both `REG_SZ` and `REG_EXPAND_SZ` (automatically expanding
        // environment variables in the latter). Combining `RRF_RT_REG_EXPAND_SZ` without
        // `RRF_NOEXPAND` makes Windows `RegGetValueW` return `ERROR_INVALID_PARAMETER`.
        let flags = RRF_RT_REG_SZ;
        let mut cb_data: u32 = 0;
        // SAFETY: `subkey` and `value_name` are NUL-terminated UTF-16 buffers, and passing `None`
        // for `pvdata` queries the required byte length.
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                PCWSTR::from_raw(subkey.as_ptr()),
                PCWSTR::from_raw(value_name.as_ptr()),
                flags,
                None,
                None,
                Some(&mut cb_data),
            )
        };
        if status == ERROR_FILE_NOT_FOUND || status == ERROR_PATH_NOT_FOUND {
            return Ok(None);
        }
        if status != ERROR_SUCCESS {
            return Err(OsError::Platform {
                detail: format!(
                    "RegGetValueW(HKLM\\SOFTWARE\\OpenSSH\\DefaultShell) failed with Win32 error {}",
                    status.0
                ),
            });
        }
        if cb_data == 0 {
            return Ok(Some(String::new()));
        }
        // Loop on `ERROR_MORE_DATA` in case the registry value grows between the size probe and
        // the read.
        for _ in 0..4 {
            let mut buf = vec![0_u16; (cb_data as usize).div_ceil(2)];
            let mut out_bytes = u32::try_from(buf.len() * 2).unwrap_or(u32::MAX);
            // SAFETY: `buf` holds at least `out_bytes` bytes of writable UTF-16 storage.
            let status = unsafe {
                RegGetValueW(
                    HKEY_LOCAL_MACHINE,
                    PCWSTR::from_raw(subkey.as_ptr()),
                    PCWSTR::from_raw(value_name.as_ptr()),
                    flags,
                    None,
                    Some(buf.as_mut_ptr().cast::<c_void>()),
                    Some(&mut out_bytes),
                )
            };
            if status == ERROR_FILE_NOT_FOUND || status == ERROR_PATH_NOT_FOUND {
                return Ok(None);
            }
            if status == ERROR_MORE_DATA {
                cb_data = out_bytes.max(cb_data.saturating_mul(2));
                continue;
            }
            if status != ERROR_SUCCESS {
                return Err(OsError::Platform {
                    detail: format!(
                        "RegGetValueW(HKLM\\SOFTWARE\\OpenSSH\\DefaultShell) failed with Win32 error {}",
                        status.0
                    ),
                });
            }
            let u16_len = (out_bytes as usize) / 2;
            let slice = &buf[..u16_len.min(buf.len())];
            let trimmed_len = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
            return Ok(Some(String::from_utf16_lossy(&slice[..trimmed_len])));
        }
        Err(OsError::Platform {
            detail:
                "RegGetValueW(HKLM\\SOFTWARE\\OpenSSH\\DefaultShell) kept returning ERROR_MORE_DATA"
                    .to_owned(),
        })
    }

    pub(super) fn apply_admin_ssh_acl(path: &Path) -> Result<()> {
        let icacls = std::env::var_os("SystemRoot")
            .map(|root| PathBuf::from(root).join("System32").join("icacls.exe"))
            .filter(|candidate| candidate.is_file())
            .unwrap_or_else(|| PathBuf::from("icacls.exe"));
        run_icacls(
            &icacls,
            path,
            &[
                "/inheritance:r",
                "/grant:r",
                "*S-1-5-32-544:F",
                "/grant:r",
                "*S-1-5-18:F",
            ],
            "setting Administrators/SYSTEM ACL",
        )?;
        run_icacls(
            &icacls,
            path,
            &["/setowner", "*S-1-5-32-544"],
            "setting BUILTIN\\Administrators owner",
        )?;
        Ok(())
    }

    fn run_icacls(icacls: &Path, path: &Path, args: &[&str], action: &str) -> Result<()> {
        let output = Command::new(icacls)
            .arg(path)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|err| {
                OsError::from_io(
                    format!(
                        "cannot run {} while {action} on {}",
                        icacls.display(),
                        path.display()
                    ),
                    err,
                )
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let detail = stderr.trim();
            let detail = if detail.is_empty() {
                stdout.trim()
            } else {
                detail
            };
            return Err(OsError::Platform {
                detail: format!(
                    "icacls failed {action} on {} (exit {:?}): {detail}",
                    path.display(),
                    output.status.code()
                ),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_username_returns_non_empty_name() {
        let name = current_username().unwrap();
        assert!(!name.is_empty());
        assert!(!name.chars().any(char::is_control));
    }

    #[test]
    fn classifies_windows_default_shell_values() {
        assert_eq!(classify_windows_default_shell(None), WindowsSshShell::Cmd);
        assert_eq!(
            classify_windows_default_shell(Some("")),
            WindowsSshShell::Cmd
        );
        assert_eq!(
            classify_windows_default_shell(Some(r"C:\Windows\System32\cmd.exe")),
            WindowsSshShell::Cmd
        );
        assert_eq!(
            classify_windows_default_shell(Some(
                r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"
            )),
            WindowsSshShell::PowerShell
        );
        assert_eq!(
            classify_windows_default_shell(Some(r#""C:\Program Files\PowerShell\7\pwsh.exe""#)),
            WindowsSshShell::PowerShell
        );
        assert_eq!(
            classify_windows_default_shell(Some("C:/Program Files/PowerShell/7/PWSH")),
            WindowsSshShell::PowerShell
        );
        assert_eq!(
            classify_windows_default_shell(Some(r"C:\Program Files\Git\bin\bash.exe")),
            WindowsSshShell::Posix
        );
        assert_eq!(
            classify_windows_default_shell(Some(r"C:\msys64\usr\bin\sh.exe")),
            WindowsSshShell::Posix
        );
        assert_eq!(
            classify_windows_default_shell(Some(r"C:\msys64\usr\bin\zsh.exe")),
            WindowsSshShell::Posix
        );
        assert_eq!(
            classify_windows_default_shell(Some(r"C:\Program Files\nu\bin\nu.exe")),
            WindowsSshShell::Unknown(r"C:\Program Files\nu\bin\nu.exe".to_owned())
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_windows_is_not_windows_administrator_and_acl_is_noop() {
        assert!(!is_windows_administrator());
        assert_eq!(windows_sshd_default_shell().unwrap(), WindowsSshShell::Cmd);
        apply_windows_admin_ssh_acl(Path::new("/nonexistent")).unwrap();
    }
}
