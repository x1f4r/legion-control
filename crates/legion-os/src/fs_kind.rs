//! Filesystem-kind inspection used to refuse network filesystems for sync folders.
//!
//! Sync folders require reliable local POSIX/NTFS atomic rename, `fsync`, and native directory
//! watching semantics. Network filesystems (NFS, SMB/CIFS, AFP, WebDAV, SSHFS, rclone/FUSE remote
//! mounts, Windows UNC shares and mapped network drives) are detected and refused before a sync
//! folder is created or joined.

use std::path::{Path, PathBuf};

use crate::{OsError, Result};

/// Returns `Ok(Some(kind))` when `path` (or its nearest existing ancestor, if `path` does not yet
/// exist) resides on a network/remote filesystem, or `Ok(None)` when it is on a local filesystem.
pub fn network_filesystem_kind(path: &Path) -> Result<Option<String>> {
    if is_unc_path(path) {
        return Ok(Some("network share (UNC)".to_owned()));
    }
    let probe = nearest_existing_ancestor(path).ok_or_else(|| {
        OsError::from_io(
            format!("inspect filesystem for {}", path.display()),
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no existing ancestor directory found",
            ),
        )
    })?;
    platform_network_fs_kind(&probe)
}

/// Returns `true` when `path` is a Windows UNC share path (`\\server\share...`, `//server/share...`,
/// or `\\?\UNC\...`).
pub fn is_unc_path(path: &Path) -> bool {
    let raw = path.to_string_lossy();
    if raw.starts_with(r"\\?\UNC\") || raw.starts_with(r"\\.\UNC\") {
        return true;
    }
    if let Some(rest) = raw.strip_prefix(r"\\") {
        return !rest.starts_with("?\\") && !rest.starts_with(".\\") && !rest.is_empty();
    }
    // On Windows, `//server/share` is also a UNC share; on Unix `//foo` is equivalent to `/foo`,
    // except explicit `//server/share` isn't a normal Unix mount anyway — only treat `//` as UNC
    // on Windows to avoid false positives on Unix double-slash paths.
    #[cfg(windows)]
    {
        if let Some(rest) = raw.strip_prefix("//") {
            return !rest.starts_with('/') && !rest.is_empty();
        }
    }
    false
}

fn nearest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut cur = path;
    loop {
        if cur.exists() {
            return Some(cur.to_path_buf());
        }
        cur = cur.parent()?;
    }
}

/// Classifies a Linux `statfs.f_type` magic number when it unconditionally identifies a network
/// filesystem. Returns `None` for local filesystems or generic FUSE (which is checked via
/// `/proc/self/mountinfo`).
pub fn classify_linux_fs_magic(f_type: u64) -> Option<&'static str> {
    match f_type {
        0x6969 => Some("nfs"),
        0x517B => Some("smb"),
        0xFF53_4D42 => Some("cifs"),
        0xFE53_4D42 => Some("smb2"),
        0x564C => Some("ncpfs"),
        0x5346_414F => Some("afs"),
        0x7375_7245 => Some("coda"),
        0x00C3_6400 => Some("ceph"),
        0x0102_1997 => Some("9p"),
        0x6573_5543 => Some("fusectl"),
        0x6B41_4653 => Some("kafs"),
        0x0BD0_0BD0 => Some("lustre"),
        0x4750_4653 => Some("gpfs"),
        0x6E66_7364 => Some("nfsd"),
        0x7461_636F => Some("ocfs2"),
        _ => None,
    }
}

/// Classifies a filesystem type string (from Linux `/proc/self/mountinfo` or macOS
/// `statfs.f_fstypename`) as a network filesystem when known to be remote.
pub fn classify_fstype_name(fstype: &str) -> Option<&'static str> {
    let lower = fstype.to_ascii_lowercase();
    match lower.as_str() {
        "nfs" | "nfs4" => Some("nfs"),
        "cifs" | "smb" | "smb2" | "smb3" | "smbfs" => Some("smb/cifs"),
        "afpfs" => Some("afp"),
        "webdav" | "davfs" | "fuse.davfs" | "fuse.webdav" => Some("webdav"),
        "fuse.sshfs" | "sshfs" => Some("sshfs"),
        "fuse.rclone" => Some("rclone"),
        "fuse.gvfsd-fuse" | "gvfsd-fuse" => Some("gvfs"),
        "fuse.s3fs" | "fuse.gcsfuse" | "fuse.goofys" => Some("object-storage fuse"),
        "fuse.curlftpfs" | "ftp" => Some("ftp"),
        "fuse.glusterfs" | "glusterfs" => Some("glusterfs"),
        "fuse.ceph" | "ceph" => Some("ceph"),
        "9p" => Some("9p"),
        "afs" | "kafs" => Some("afs"),
        "ncpfs" => Some("ncpfs"),
        "coda" => Some("coda"),
        "lustre" => Some("lustre"),
        "vboxsf" | "vmhgfs" | "fuse.vmhgfs-fuse" => Some("vm shared folder"),
        other if other.starts_with("fuse.") && (other.contains("smb") || other.contains("nfs") || other.contains("cifs") || other.contains("ssh") || other.contains("ftp") || other.contains("dav") || other.contains("s3") || other.contains("rclone")) => {
            Some("remote fuse")
        }
        _ => None,
    }
}

/// Parses Linux `/proc/self/mountinfo` content to find the filesystem type of the deepest mount
/// point containing `target`.
pub fn mountinfo_fstype_for_path(mountinfo: &str, target: &Path) -> Option<String> {
    let target_str = target.to_string_lossy();
    let mut best_len = 0usize;
    let mut best_fstype: Option<String> = None;

    for line in mountinfo.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let left_fields: Vec<&str> = left.split(' ').collect();
        if left_fields.len() < 5 {
            continue;
        }
        let mount_point = unescape_mountinfo_field(left_fields[4]);
        let right_fields: Vec<&str> = right.split(' ').collect();
        let Some(fstype) = right_fields.first() else {
            continue;
        };

        let matches = if mount_point == "/" {
            target_str.starts_with('/')
        } else {
            target_str == mount_point
                || (target_str.starts_with(&mount_point)
                    && target_str.as_bytes().get(mount_point.len()) == Some(&b'/'))
        };
        if matches && mount_point.len() >= best_len {
            best_len = mount_point.len();
            best_fstype = Some((*fstype).to_owned());
        }
    }
    best_fstype
}

fn unescape_mountinfo_field(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let d0 = bytes[i + 1];
            let d1 = bytes[i + 2];
            let d2 = bytes[i + 3];
            if (b'0'..=b'7').contains(&d0)
                && (b'0'..=b'7').contains(&d1)
                && (b'0'..=b'7').contains(&d2)
            {
                let val = ((d0 - b'0') << 6) | ((d1 - b'0') << 3) | (d2 - b'0');
                out.push(val as char);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(target_os = "linux")]
fn platform_network_fs_kind(path: &Path) -> Result<Option<String>> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    const FUSE_SUPER_MAGIC: u64 = 0x6573_5546;

    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        OsError::from_io(
            format!("inspect filesystem for {}", path.display()),
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL byte"),
        )
    })?;

    let mut stat: libc::statfs = {
        #[allow(unsafe_code)]
        // SAFETY: `libc::statfs` is a plain C struct of integer fields safe to zero-initialize.
        unsafe {
            std::mem::zeroed()
        }
    };

    #[allow(unsafe_code)]
    // SAFETY: `c_path` is a valid NUL-terminated path and `stat` is valid writable memory.
    let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return Err(OsError::from_io(
            format!("statfs {}", path.display()),
            std::io::Error::last_os_error(),
        ));
    }

    let f_type = stat.f_type as u64;
    if let Some(kind) = classify_linux_fs_magic(f_type) {
        return Ok(Some(kind.to_owned()));
    }

    if f_type == FUSE_SUPER_MAGIC
        && let Ok(canonical) = std::fs::canonicalize(path)
        && let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo")
        && let Some(fstype) = mountinfo_fstype_for_path(&mountinfo, &canonical)
        && let Some(kind) = classify_fstype_name(&fstype)
    {
        return Ok(Some(kind.to_owned()));
    }

    Ok(None)
}

#[cfg(target_os = "macos")]
fn platform_network_fs_kind(path: &Path) -> Result<Option<String>> {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        OsError::from_io(
            format!("inspect filesystem for {}", path.display()),
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL byte"),
        )
    })?;

    let mut stat: libc::statfs = {
        #[allow(unsafe_code)]
        // SAFETY: `libc::statfs` is a plain C struct safe to zero-initialize.
        unsafe {
            std::mem::zeroed()
        }
    };

    #[allow(unsafe_code)]
    // SAFETY: `c_path` is a valid NUL-terminated path and `stat` is valid writable memory.
    let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return Err(OsError::from_io(
            format!("statfs {}", path.display()),
            std::io::Error::last_os_error(),
        ));
    }

    #[allow(unsafe_code)]
    // SAFETY: `f_fstypename` is a NUL-terminated C char array populated by `statfs`.
    let fstype = unsafe { CStr::from_ptr(stat.f_fstypename.as_ptr()) }
        .to_string_lossy()
        .into_owned();

    if let Some(kind) = classify_fstype_name(&fstype) {
        return Ok(Some(kind.to_owned()));
    }
    if (stat.f_flags & (libc::MNT_LOCAL as u32)) == 0 {
        return Ok(Some(if fstype.is_empty() {
            "remote filesystem".to_owned()
        } else {
            fstype
        }));
    }
    Ok(None)
}

#[cfg(windows)]
fn platform_network_fs_kind(path: &Path) -> Result<Option<String>> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    use windows::core::PCWSTR;

    // DRIVE_REMOTE = 4 in Win32.
    const DRIVE_REMOTE: u32 = 4;

    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if is_unc_path(&canonical) {
        return Ok(Some("network share (UNC)".to_owned()));
    }
    let raw = canonical.to_string_lossy();
    let stripped = raw.strip_prefix(r"\\?\").unwrap_or(&raw);
    let Some(drive) = stripped.get(..3) else {
        return Ok(None);
    };
    let bytes = drive.as_bytes();
    if !(bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/')) {
        return Ok(None);
    }
    let root = format!("{}:\\", bytes[0] as char);
    let wide: Vec<u16> = std::ffi::OsStr::new(&root)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    #[allow(unsafe_code)]
    // SAFETY: `wide` is a valid NUL-terminated UTF-16 string.
    let drive_type = unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) };
    if drive_type == DRIVE_REMOTE {
        return Ok(Some("mapped network drive".to_owned()));
    }
    Ok(None)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn platform_network_fs_kind(_path: &Path) -> Result<Option<String>> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_linux_network_magics() {
        assert_eq!(classify_linux_fs_magic(0x6969), Some("nfs"));
        assert_eq!(classify_linux_fs_magic(0xFF53_4D42), Some("cifs"));
        assert_eq!(classify_linux_fs_magic(0xFE53_4D42), Some("smb2"));
        assert_eq!(classify_linux_fs_magic(0x0102_1997), Some("9p"));
        // ext4 (0xEF53) and btrfs (0x9123683E) are local.
        assert_eq!(classify_linux_fs_magic(0xEF53), None);
        assert_eq!(classify_linux_fs_magic(0x9123_683E), None);
    }

    #[test]
    fn classifies_fstype_names_and_mountinfo() {
        assert_eq!(classify_fstype_name("fuse.sshfs"), Some("sshfs"));
        assert_eq!(classify_fstype_name("fuse.rclone"), Some("rclone"));
        assert_eq!(classify_fstype_name("smbfs"), Some("smb/cifs"));
        assert_eq!(classify_fstype_name("ext4"), None);
        assert_eq!(classify_fstype_name("btrfs"), None);
        assert_eq!(classify_fstype_name("fuseblk"), None);

        let sample = "\
22 1 0:20 / / rw,relatime - btrfs /dev/nvme0n1p2 rw
89 22 0:45 / /mnt/nas\\040share rw,nosuid - fuse.sshfs user@nas:/data rw
90 22 0:46 / /mnt/nas\\040share/local rw - ext4 /dev/sdb1 rw
";
        assert_eq!(
            mountinfo_fstype_for_path(sample, Path::new("/mnt/nas share/docs/a.txt")).as_deref(),
            Some("fuse.sshfs")
        );
        assert_eq!(
            mountinfo_fstype_for_path(sample, Path::new("/mnt/nas share/local/file")).as_deref(),
            Some("ext4")
        );
        assert_eq!(
            mountinfo_fstype_for_path(sample, Path::new("/home/me/docs")).as_deref(),
            Some("btrfs")
        );
    }

    #[test]
    fn detects_unc_paths() {
        assert!(is_unc_path(Path::new(r"\\nas\share\docs")));
        assert!(is_unc_path(Path::new(r"\\?\UNC\nas\share\docs")));
        assert!(!is_unc_path(Path::new(r"\\?\C:\Work\docs")));
        assert!(!is_unc_path(Path::new("/home/me/docs")));
    }

    #[test]
    fn local_temp_dir_is_not_network_fs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kind = network_filesystem_kind(dir.path()).expect("network_filesystem_kind");
        assert_eq!(kind, None);
    }
}
