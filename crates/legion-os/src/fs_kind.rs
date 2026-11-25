//! Network file system detection for safe move probing.
//!
//! On a network share mounted by two machines (NFS, SMB/CIFS, sshfs, 9p, etc.), cached directory
//! listings can hide a probe marker written on one host from a read on the other. Detecting a
//! network file system lets callers treat the directory pair as inconclusive so a move copies
//! cleanly and keeps the source rather than deleting the only copy.

use std::path::Path;

/// Linux `statfs::f_type` magic for FUSE mounts, refined via `/proc/self/mountinfo` so local FUSE
/// mounts (e.g. `fuseblk` / `fuse.ntfs`) are distinguished from network ones (`fuse.sshfs`,
/// `fuse.rclone`, `fuse.gvfsd-fuse`, etc.).
pub const LINUX_FUSE_SUPER_MAGIC: i64 = 0x6573_5546;

/// Windows `GetDriveTypeW` return code for a remote (network) drive (`DRIVE_REMOTE`).
pub const WINDOWS_DRIVE_REMOTE: u32 = 4;

/// Classifies a Linux `statfs::f_type` filesystem magic number into a network file system label,
/// returning `None` for local file systems (and for `LINUX_FUSE_SUPER_MAGIC`, which callers refine
/// via [`is_network_fs_name`] on the mount's filesystem type string).
pub fn network_fs_from_linux_magic(f_type: i64) -> Option<&'static str> {
    match f_type {
        0x6969 => Some("nfs"),
        0xFF53_4D42 => Some("cifs"),
        0xFE53_4D42 => Some("smb2"),
        0x517B => Some("smbfs"),
        0x0102_1997 => Some("9p"),
        0x5346_414F | 0x6B41_4653 => Some("afs"),
        0x00C3_6400 => Some("ceph"),
        0x7375_7245 => Some("coda"),
        0x564C => Some("ncpfs"),
        0x7461_636F => Some("ocfs2"),
        0x0116_1970 => Some("gfs2"),
        0x0BD0_0BD0 => Some("lustre"),
        0x786F_4256 => Some("vboxsf"),
        _ => None,
    }
}

/// Returns `true` when `fs_type` (a macOS/BSD `statfs::f_fstypename` or Linux `/proc/self/mountinfo`
/// filesystem type string) names a network or host-shared file system.
pub fn is_network_fs_name(fs_type: &str) -> bool {
    let lower = fs_type.trim().to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "nfs"
            | "nfs4"
            | "smbfs"
            | "cifs"
            | "smb"
            | "smb2"
            | "smb3"
            | "afpfs"
            | "webdav"
            | "davfs"
            | "davfs2"
            | "9p"
            | "ceph"
            | "cephfs"
            | "glusterfs"
            | "lustre"
            | "afs"
            | "coda"
            | "ncpfs"
            | "gfs2"
            | "ocfs2"
            | "sshfs"
            | "virtiofs"
            | "vboxsf"
            | "vmhgfs"
            | "prl_fs"
            | "fuse.sshfs"
            | "fuse.rclone"
            | "fuse.gvfsd-fuse"
            | "fuse.s3fs"
            | "fuse.glusterfs"
            | "fuse.ceph"
            | "fuse.gcsfuse"
            | "fuse.davfs"
            | "fuse.davfs2"
            | "fuse.cifs"
            | "fuse.smb"
            | "fuse.nfs"
            | "fuse.afpfs"
            | "fuse.webdav"
    )
}

/// Classifies a Windows `GetDriveTypeW` result and whether the path is a UNC share path.
pub fn is_network_windows_drive_type(drive_type: u32, path_is_unc: bool) -> bool {
    path_is_unc || drive_type == WINDOWS_DRIVE_REMOTE
}

/// Returns `true` when a Windows-style path string refers to a UNC network share (`\\server\share`
/// or `\\?\UNC\server\share`).
pub fn is_unc_path_str(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    if bytes.len() >= 8
        && (bytes[0] == b'\\' || bytes[0] == b'/')
        && (bytes[1] == b'\\' || bytes[1] == b'/')
        && (bytes[2] == b'?' || bytes[2] == b'.')
        && (bytes[3] == b'\\' || bytes[3] == b'/')
        && bytes[4..7].eq_ignore_ascii_case(b"UNC")
        && (bytes[7] == b'\\' || bytes[7] == b'/')
    {
        return true;
    }
    raw.starts_with(r"\\") && !raw.starts_with(r"\\?\") && !raw.starts_with(r"\\.\")
}

/// Extracts the filesystem type string for `target_path` from Linux `/proc/self/mountinfo` text
/// by matching the longest mount-point prefix.
pub fn linux_mountinfo_fstype(mountinfo: &str, target_path: &str) -> Option<String> {
    let mut best_len = 0usize;
    let mut best_fs: Option<String> = None;
    for line in mountinfo.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let left_fields: Vec<&str> = left.split_whitespace().collect();
        if left_fields.len() < 5 {
            continue;
        }
        let mount_point = unescape_mountinfo(left_fields[4]);
        let matches = target_path == mount_point
            || mount_point == "/"
            || target_path
                .strip_prefix(&mount_point)
                .is_some_and(|rest| rest.starts_with('/'));
        if matches && mount_point.len() >= best_len {
            let mut right_fields = right.split_whitespace();
            if let Some(fs_type) = right_fields.next() {
                best_len = mount_point.len();
                best_fs = Some(fs_type.to_owned());
            }
        }
    }
    best_fs
}

fn unescape_mountinfo(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && (b'0'..=b'3').contains(&bytes[i + 1])
            && (b'0'..=b'7').contains(&bytes[i + 2])
            && (b'0'..=b'7').contains(&bytes[i + 3])
        {
            let val =
                ((bytes[i + 1] - b'0') << 6) | ((bytes[i + 2] - b'0') << 3) | (bytes[i + 3] - b'0');
            out.push(val);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Detects whether `path` resides on a network file system on the running host, returning a short
/// human-readable filesystem kind (such as `"nfs"`, `"cifs"`, `"smbfs"`, or `"remote drive"`) when
/// it does and `None` when it is local or cannot be determined.
#[cfg(target_os = "linux")]
pub fn detect_network_fs(path: &Path) -> Option<String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    #[allow(unsafe_code)]
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    #[allow(unsafe_code)]
    let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast)]
    let magic = stat.f_type as i64;
    if let Some(name) = network_fs_from_linux_magic(magic) {
        return Some(name.to_owned());
    }
    if magic == LINUX_FUSE_SUPER_MAGIC {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
        let fs_type = linux_mountinfo_fstype(&mountinfo, &canonical.to_string_lossy())?;
        if is_network_fs_name(&fs_type) {
            return Some(fs_type);
        }
    }
    None
}

/// Detects whether `path` resides on a network file system on macOS / BSD via `statfs::f_fstypename`.
#[cfg(all(unix, not(target_os = "linux")))]
pub fn detect_network_fs(path: &Path) -> Option<String> {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    #[allow(unsafe_code)]
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    #[allow(unsafe_code)]
    let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    #[allow(unsafe_code)]
    let fs_name = unsafe { CStr::from_ptr(stat.f_fstypename.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    if is_network_fs_name(&fs_name) {
        Some(fs_name)
    } else {
        None
    }
}

/// Detects whether `path` resides on a network share or mapped network drive on Windows via UNC
/// prefix inspection and `GetVolumePathNameW` + `GetDriveTypeW`.
#[cfg(windows)]
pub fn detect_network_fs(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumePathNameW};
    use windows::core::PCWSTR;

    let raw = path.to_string_lossy();
    let unc = is_unc_path_str(&raw);
    if unc {
        return Some("unc".to_owned());
    }
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut vol = vec![0u16; wide.len().max(260) + 16];
    #[allow(unsafe_code)]
    let ok = unsafe { GetVolumePathNameW(PCWSTR(wide.as_ptr()), &mut vol) }.is_ok();
    let drive_type = if ok {
        #[allow(unsafe_code)]
        unsafe {
            GetDriveTypeW(PCWSTR(vol.as_ptr()))
        }
    } else {
        #[allow(unsafe_code)]
        unsafe {
            GetDriveTypeW(PCWSTR(wide.as_ptr()))
        }
    };
    if is_network_windows_drive_type(drive_type, unc) {
        Some("remote drive".to_owned())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_magic_classifies_network_and_local_filesystems() {
        assert_eq!(network_fs_from_linux_magic(0x6969), Some("nfs"));
        assert_eq!(network_fs_from_linux_magic(0xFF53_4D42), Some("cifs"));
        assert_eq!(network_fs_from_linux_magic(0xFE53_4D42), Some("smb2"));
        assert_eq!(network_fs_from_linux_magic(0x0102_1997), Some("9p"));
        assert_eq!(network_fs_from_linux_magic(0x00C3_6400), Some("ceph"));
        assert_eq!(network_fs_from_linux_magic(0x786F_4256), Some("vboxsf"));
        // Local filesystems must NOT be classified as network:
        // ext4 (0xEF53), tmpfs (0x01021994), btrfs (0x9123683E), xfs (0x58465342), vfat (0x4D44).
        for local in [0xEF53, 0x0102_1994, 0x9123_683E, 0x5846_5342, 0x4D44] {
            assert_eq!(network_fs_from_linux_magic(local), None, "magic {local:#x}");
        }
    }

    #[test]
    fn fs_name_classifies_unix_macos_and_fuse_network_types() {
        for net in [
            "nfs",
            "nfs4",
            "smbfs",
            "cifs",
            "SMB3",
            "afpfs",
            "webdav",
            "9p",
            "ceph",
            "glusterfs",
            "virtiofs",
            "fuse.sshfs",
            "fuse.rclone",
            "fuse.gvfsd-fuse",
            "fuse.s3fs",
        ] {
            assert!(is_network_fs_name(net), "expected network fs: {net}");
        }
        for local in [
            "ext4",
            "xfs",
            "btrfs",
            "tmpfs",
            "apfs",
            "hfs",
            "vfat",
            "ntfs",
            "zfs",
            "overlay",
            "fuseblk",
            "fuse.ntfs",
        ] {
            assert!(!is_network_fs_name(local), "expected local fs: {local}");
        }
    }

    #[test]
    fn windows_drive_type_and_unc_detection() {
        assert!(is_network_windows_drive_type(WINDOWS_DRIVE_REMOTE, false));
        assert!(is_network_windows_drive_type(3, true)); // DRIVE_FIXED + UNC
        assert!(!is_network_windows_drive_type(3, false)); // DRIVE_FIXED
        assert!(!is_network_windows_drive_type(2, false)); // DRIVE_REMOVABLE

        assert!(is_unc_path_str(r"\\nas\share\docs"));
        assert!(is_unc_path_str(r"\\?\UNC\nas\share\docs"));
        assert!(!is_unc_path_str(r"C:\Work\docs"));
        assert!(!is_unc_path_str(r"\\?\C:\Work\docs"));
    }

    #[test]
    fn linux_mountinfo_picks_longest_matching_mount_and_unescapes_spaces() {
        let mountinfo = "\
22 1 8:1 / / rw,relatime - ext4 /dev/sda1 rw
45 22 0:42 / /mnt/media\\040share rw,nosuid - fuse.sshfs me@nas:/media rw
46 22 0:43 / /mnt/local rw - fuseblk /dev/sdb1 rw
";
        assert_eq!(
            linux_mountinfo_fstype(mountinfo, "/mnt/media share/films/a.mp4").as_deref(),
            Some("fuse.sshfs")
        );
        assert_eq!(
            linux_mountinfo_fstype(mountinfo, "/mnt/local/file.txt").as_deref(),
            Some("fuseblk")
        );
        assert_eq!(
            linux_mountinfo_fstype(mountinfo, "/home/me/file.txt").as_deref(),
            Some("ext4")
        );
    }
}
