//! Filesystem locality detection for safe move probing.
//!
//! On a network share mounted by two machines (NFS, SMB/CIFS, sshfs, 9p, etc.), or on an
//! unverified FUSE/overlay/unknown filesystem, cached directory listings or mount semantics can
//! hide a probe marker written on one host from a read on the other. Classifying a path into
//! [`FsLocality::Local`], [`FsLocality::Network`], or [`FsLocality::Unknown`] lets move probing
//! conclude distinctness or sameness **only** when both endpoints are provably [`FsLocality::Local`]
//! and treat every network, unverified FUSE, or error case as inconclusive (copy completes, source
//! kept, reason stated).

use std::path::Path;

/// Linux `statfs::f_type` magic for FUSE mounts, refined via `/proc/self/mountinfo`.
pub const LINUX_FUSE_SUPER_MAGIC: i64 = 0x6573_5546;

/// Windows `GetDriveTypeW` return codes.
pub const WINDOWS_DRIVE_UNKNOWN: u32 = 0;
/// Windows `GetDriveTypeW`: root path is invalid (`DRIVE_NO_ROOT_DIR`).
pub const WINDOWS_DRIVE_NO_ROOT_DIR: u32 = 1;
/// Windows `GetDriveTypeW`: removable local drive (`DRIVE_REMOVABLE`).
pub const WINDOWS_DRIVE_REMOVABLE: u32 = 2;
/// Windows `GetDriveTypeW`: fixed local drive (`DRIVE_FIXED`).
pub const WINDOWS_DRIVE_FIXED: u32 = 3;
/// Windows `GetDriveTypeW`: remote network drive (`DRIVE_REMOTE`).
pub const WINDOWS_DRIVE_REMOTE: u32 = 4;
/// Windows `GetDriveTypeW`: local CD-ROM drive (`DRIVE_CDROM`).
pub const WINDOWS_DRIVE_CDROM: u32 = 5;
/// Windows `GetDriveTypeW`: local RAM disk (`DRIVE_RAMDISK`).
pub const WINDOWS_DRIVE_RAMDISK: u32 = 6;

/// Three-valued locality classification of the filesystem holding a directory.
///
/// Only [`FsLocality::Local`] allows move probing to conclude whether two paths are the same or
/// distinct directories. Both [`FsLocality::Network`] and [`FsLocality::Unknown`] fail safe to
/// inconclusive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FsLocality {
    /// Provably a local kernel-backed filesystem (or local block-device FUSE `fuseblk`).
    Local,
    /// Known network or host-shared filesystem (such as NFS, SMB/CIFS, sshfs, 9p, or a UNC share).
    Network(String),
    /// Could not be proven local (unverified FUSE type, unrecognized magic/name, or OS query
    /// failure). Treated as inconclusive.
    Unknown(String),
}

impl FsLocality {
    /// Returns `true` only when the filesystem is provably local.
    pub fn is_local(&self) -> bool {
        matches!(self, FsLocality::Local)
    }
}

/// Classifies a Linux `statfs::f_type` filesystem magic number into [`FsLocality`].
///
/// `LINUX_FUSE_SUPER_MAGIC` returns [`FsLocality::Unknown`] here and must be refined via
/// [`classify_fuse_fstype`] using `/proc/self/mountinfo`. Any unrecognized magic number returns
/// [`FsLocality::Unknown`] rather than assuming local.
pub fn classify_linux_magic(f_type: i64) -> FsLocality {
    match f_type {
        // Known network / host-shared kernel filesystems.
        0x6969 => FsLocality::Network("nfs".to_owned()),
        0xFF53_4D42 => FsLocality::Network("cifs".to_owned()),
        0xFE53_4D42 => FsLocality::Network("smb2".to_owned()),
        0x517B => FsLocality::Network("smbfs".to_owned()),
        0x0102_1997 => FsLocality::Network("9p".to_owned()),
        0x5346_414F | 0x6B41_4653 => FsLocality::Network("afs".to_owned()),
        0x00C3_6400 => FsLocality::Network("ceph".to_owned()),
        0x7375_7245 => FsLocality::Network("coda".to_owned()),
        0x564C => FsLocality::Network("ncpfs".to_owned()),
        0x7461_636F => FsLocality::Network("ocfs2".to_owned()),
        0x0116_1970 => FsLocality::Network("gfs2".to_owned()),
        0x0BD0_0BD0 => FsLocality::Network("lustre".to_owned()),
        0x786F_4256 => FsLocality::Network("vboxsf".to_owned()),
        // Known local Linux kernel filesystems.
        0xEF53 // ext2/ext3/ext4
        | 0x5846_5342 // xfs
        | 0x9123_683E // btrfs
        | 0x0102_1994 // tmpfs
        | 0x8584_58F6 // ramfs
        | 0x4D44 // msdos / vfat
        | 0x2011_BAB0 // exfat
        | 0x5346_544E // ntfs-sb
        | 0x7366_746E // ntfs3
        | 0xF2F5_2010 // f2fs
        | 0x794C_7630 // overlayfs
        | 0xCA45_1A4E // bcachefs
        | 0x2FC1_2FC1 // zfs
        | 0x3153_464A // jfs
        | 0x5265_4973 // reiserfs
        | 0x7371_7368 // squashfs
        | 0x1501_3346 // udf
        | 0x9660 // iso9660
        | 0x4244 // hfs
        | 0x482B // hfsplus
        | 0x4858 // hfsx
        | 0x2003_1985 // jffs2
        => FsLocality::Local,
        LINUX_FUSE_SUPER_MAGIC => FsLocality::Unknown("fuse".to_owned()),
        other => FsLocality::Unknown(format!("unrecognized Linux filesystem magic 0x{other:x}")),
    }
}

/// Classifies a Linux FUSE filesystem type string from `/proc/self/mountinfo`.
///
/// Only `fuseblk` (a kernel-attached local block device) is classified as [`FsLocality::Local`].
/// Known network FUSE types are [`FsLocality::Network`]; every other FUSE type (such as
/// `fuse.mergerfs`, `fuse.bindfs`, `fuse.encfs`, or plain `fuse`) is [`FsLocality::Unknown`].
pub fn classify_fuse_fstype(fs_type: &str) -> FsLocality {
    let lower = fs_type.trim().to_ascii_lowercase();
    if lower == "fuseblk" {
        return FsLocality::Local;
    }
    if is_known_network_fs_name(&lower) {
        return FsLocality::Network(lower);
    }
    if lower.is_empty() {
        FsLocality::Unknown("empty FUSE filesystem type".to_owned())
    } else {
        FsLocality::Unknown(format!("unverified FUSE filesystem ({lower})"))
    }
}

/// Classifies a Unix/macOS/BSD `statfs::f_fstypename` string into [`FsLocality`].
///
/// Only explicitly known local filesystems return [`FsLocality::Local`]. Known network filesystems
/// return [`FsLocality::Network`]. Everything else — including `macfuse`, `osxfuse`, `fusefs`,
/// `nullfs`, and unrecognized names — returns [`FsLocality::Unknown`].
pub fn classify_unix_fs_name(fs_type: &str) -> FsLocality {
    let lower = fs_type.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return FsLocality::Unknown("empty filesystem type name".to_owned());
    }
    if matches!(
        lower.as_str(),
        "apfs"
            | "hfs"
            | "ufs"
            | "ffs"
            | "zfs"
            | "msdos"
            | "vfat"
            | "exfat"
            | "ntfs"
            | "ntfs3"
            | "tmpfs"
            | "ext2"
            | "ext3"
            | "ext4"
            | "xfs"
            | "btrfs"
            | "f2fs"
            | "cd9660"
            | "udf"
    ) {
        return FsLocality::Local;
    }
    if is_known_network_fs_name(&lower) {
        return FsLocality::Network(lower);
    }
    FsLocality::Unknown(format!("unverified filesystem ({lower})"))
}

fn is_known_network_fs_name(lower: &str) -> bool {
    matches!(
        lower,
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
            | "lifs"
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
            | "fuse.juicefs"
            | "fuse.mountpoint-s3"
            | "fuse.goofys"
            | "fuse.seaweedfs"
            | "fuse.smbnetfs"
            | "fuse.curlftpfs"
            | "fuse.ossfs"
            | "fuse.blobfuse"
            | "fuse.blobfuse2"
    )
}

/// Classifies a Windows `GetDriveTypeW` result and UNC flag into [`FsLocality`].
pub fn classify_windows_drive_type(drive_type: u32, path_is_unc: bool) -> FsLocality {
    if path_is_unc {
        return FsLocality::Network("unc".to_owned());
    }
    match drive_type {
        WINDOWS_DRIVE_REMOTE => FsLocality::Network("remote drive".to_owned()),
        WINDOWS_DRIVE_REMOVABLE
        | WINDOWS_DRIVE_FIXED
        | WINDOWS_DRIVE_CDROM
        | WINDOWS_DRIVE_RAMDISK => FsLocality::Local,
        WINDOWS_DRIVE_UNKNOWN => FsLocality::Unknown("Windows drive type is unknown".to_owned()),
        WINDOWS_DRIVE_NO_ROOT_DIR => {
            FsLocality::Unknown("Windows path has no valid root directory".to_owned())
        }
        other => FsLocality::Unknown(format!("unrecognized Windows drive type {other}")),
    }
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

/// Detects the filesystem locality of `path` on Linux via `statfs` and `/proc/self/mountinfo`.
///
/// Any error (missing path, failed `statfs`, unreadable mountinfo, or unknown FUSE/magic type)
/// returns [`FsLocality::Unknown`] rather than assuming local.
#[cfg(target_os = "linux")]
pub fn detect_fs_locality(path: &Path) -> FsLocality {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
        return FsLocality::Unknown("path contains NUL byte".to_owned());
    };
    #[allow(unsafe_code)]
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    #[allow(unsafe_code)]
    let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return FsLocality::Unknown(format!("statfs failed: {err}"));
    }
    #[allow(clippy::unnecessary_cast)]
    let magic = stat.f_type as i64;
    if magic == LINUX_FUSE_SUPER_MAGIC {
        let Ok(canonical) = std::fs::canonicalize(path) else {
            return FsLocality::Unknown("could not canonicalize FUSE path".to_owned());
        };
        let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") else {
            return FsLocality::Unknown("could not read /proc/self/mountinfo for FUSE".to_owned());
        };
        let Some(fs_type) = linux_mountinfo_fstype(&mountinfo, &canonical.to_string_lossy()) else {
            return FsLocality::Unknown("FUSE mount entry not found in mountinfo".to_owned());
        };
        return classify_fuse_fstype(&fs_type);
    }
    classify_linux_magic(magic)
}

/// Detects the filesystem locality of `path` on macOS / BSD via `statfs::f_fstypename`.
///
/// Any error or unverified filesystem (including `macfuse` / `osxfuse`) returns
/// [`FsLocality::Unknown`].
#[cfg(all(unix, not(target_os = "linux")))]
pub fn detect_fs_locality(path: &Path) -> FsLocality {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;

    let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
        return FsLocality::Unknown("path contains NUL byte".to_owned());
    };
    #[allow(unsafe_code)]
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    #[allow(unsafe_code)]
    let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return FsLocality::Unknown(format!("statfs failed: {err}"));
    }
    #[allow(unsafe_code)]
    let fs_name = unsafe { CStr::from_ptr(stat.f_fstypename.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    classify_unix_fs_name(&fs_name)
}

/// Detects the filesystem locality of `path` on Windows via UNC prefix inspection and
/// `GetVolumePathNameW` + `GetDriveTypeW`. Any error returns [`FsLocality::Unknown`].
#[cfg(windows)]
pub fn detect_fs_locality(path: &Path) -> FsLocality {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumePathNameW};
    use windows::core::PCWSTR;

    let raw = path.to_string_lossy();
    if is_unc_path_str(&raw) {
        return FsLocality::Network("unc".to_owned());
    }
    if std::fs::symlink_metadata(path).is_err() {
        return FsLocality::Unknown("path does not exist or cannot be inspected".to_owned());
    }
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut vol = vec![0u16; wide.len().max(260) + 16];
    #[allow(unsafe_code)]
    let ok = unsafe { GetVolumePathNameW(PCWSTR(wide.as_ptr()), &mut vol) }.is_ok();
    if !ok {
        return FsLocality::Unknown("GetVolumePathNameW failed".to_owned());
    }
    #[allow(unsafe_code)]
    let drive_type = unsafe { GetDriveTypeW(PCWSTR(vol.as_ptr())) };
    classify_windows_drive_type(drive_type, false)
}

#[cfg(not(any(unix, windows)))]
/// Fallback filesystem locality detector on unsupported platforms: always [`FsLocality::Unknown`].
pub fn detect_fs_locality(_path: &Path) -> FsLocality {
    FsLocality::Unknown("filesystem locality detection is unsupported on this platform".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_magic_classifies_local_network_and_unknown() {
        assert_eq!(
            classify_linux_magic(0x6969),
            FsLocality::Network("nfs".to_owned())
        );
        assert_eq!(
            classify_linux_magic(0xFF53_4D42),
            FsLocality::Network("cifs".to_owned())
        );
        assert_eq!(
            classify_linux_magic(0xFE53_4D42),
            FsLocality::Network("smb2".to_owned())
        );
        assert_eq!(
            classify_linux_magic(0x0102_1997),
            FsLocality::Network("9p".to_owned())
        );
        assert_eq!(
            classify_linux_magic(0x00C3_6400),
            FsLocality::Network("ceph".to_owned())
        );
        assert_eq!(
            classify_linux_magic(0x786F_4256),
            FsLocality::Network("vboxsf".to_owned())
        );
        for local in [0xEF53, 0x0102_1994, 0x9123_683E, 0x5846_5342, 0x4D44] {
            assert_eq!(
                classify_linux_magic(local),
                FsLocality::Local,
                "magic {local:#x}"
            );
        }
        assert!(matches!(
            classify_linux_magic(LINUX_FUSE_SUPER_MAGIC),
            FsLocality::Unknown(_)
        ));
        assert!(matches!(
            classify_linux_magic(0x1234_5678),
            FsLocality::Unknown(_)
        ));
    }

    #[test]
    fn fuse_and_unix_names_err_toward_unknown_for_unverified_types() {
        // Only fuseblk is Local for FUSE.
        assert_eq!(classify_fuse_fstype("fuseblk"), FsLocality::Local);

        // Known network FUSE types are Network.
        for net in [
            "fuse.sshfs",
            "fuse.rclone",
            "fuse.gvfsd-fuse",
            "fuse.s3fs",
            "fuse.juicefs",
            "fuse.mountpoint-s3",
            "fuse.goofys",
            "fuse.seaweedfs",
        ] {
            assert!(
                matches!(classify_fuse_fstype(net), FsLocality::Network(_)),
                "expected network fuse: {net}"
            );
        }

        // Unverified FUSE types MUST be Unknown, never Local.
        for unk in [
            "fuse.mergerfs",
            "fuse.bindfs",
            "fuse.encfs",
            "fuse.ntfs",
            "fuse",
            "",
        ] {
            assert!(
                matches!(classify_fuse_fstype(unk), FsLocality::Unknown(_)),
                "expected unknown fuse: {unk}"
            );
        }

        // macOS / BSD names: macfuse, osxfuse, nullfs, and unknown names MUST be Unknown.
        for unk in ["macfuse", "osxfuse", "fusefs", "nullfs", "mysteryfs", ""] {
            assert!(
                matches!(classify_unix_fs_name(unk), FsLocality::Unknown(_)),
                "expected unknown unix fs: {unk}"
            );
        }
        for net in ["nfs", "smbfs", "afpfs", "webdav", "cifs", "SMB3", "9p"] {
            assert!(
                matches!(classify_unix_fs_name(net), FsLocality::Network(_)),
                "expected network unix fs: {net}"
            );
        }
        for local in [
            "apfs", "hfs", "ext4", "xfs", "btrfs", "tmpfs", "zfs", "vfat",
        ] {
            assert_eq!(
                classify_unix_fs_name(local),
                FsLocality::Local,
                "expected local unix fs: {local}"
            );
        }
    }

    #[test]
    fn windows_drive_type_and_unc_err_toward_unknown_on_unknown_or_invalid_drive() {
        assert_eq!(
            classify_windows_drive_type(WINDOWS_DRIVE_REMOTE, false),
            FsLocality::Network("remote drive".to_owned())
        );
        assert_eq!(
            classify_windows_drive_type(WINDOWS_DRIVE_FIXED, true),
            FsLocality::Network("unc".to_owned())
        );
        assert_eq!(
            classify_windows_drive_type(WINDOWS_DRIVE_FIXED, false),
            FsLocality::Local
        );
        assert_eq!(
            classify_windows_drive_type(WINDOWS_DRIVE_REMOVABLE, false),
            FsLocality::Local
        );
        assert!(matches!(
            classify_windows_drive_type(WINDOWS_DRIVE_UNKNOWN, false),
            FsLocality::Unknown(_)
        ));
        assert!(matches!(
            classify_windows_drive_type(WINDOWS_DRIVE_NO_ROOT_DIR, false),
            FsLocality::Unknown(_)
        ));
        assert!(matches!(
            classify_windows_drive_type(99, false),
            FsLocality::Unknown(_)
        ));

        assert!(is_unc_path_str(r"\\nas\share\docs"));
        assert!(is_unc_path_str(r"\\?\UNC\nas\share\docs"));
        assert!(!is_unc_path_str(r"C:\Work\docs"));
        assert!(!is_unc_path_str(r"\\?\C:\Work\docs"));
    }

    #[test]
    fn missing_path_reports_unknown_never_local() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(detect_fs_locality(tmp.path()), FsLocality::Local);
        let missing = tmp.path().join("definitely-does-not-exist");
        assert!(matches!(
            detect_fs_locality(&missing),
            FsLocality::Unknown(_)
        ));
    }

    #[test]
    fn linux_mountinfo_picks_longest_matching_mount_and_unescapes_spaces() {
        let mountinfo = "\
22 1 8:1 / / rw,relatime - ext4 /dev/sda1 rw
45 22 0:42 / /mnt/media\\040share rw,nosuid - fuse.sshfs me@nas:/media rw
46 22 0:43 / /mnt/local rw - fuseblk /dev/sdb1 rw
47 22 0:44 / /mnt/pool rw - fuse.mergerfs /disk1:/disk2 rw
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
            linux_mountinfo_fstype(mountinfo, "/mnt/pool/video.mkv").as_deref(),
            Some("fuse.mergerfs")
        );
        assert_eq!(
            linux_mountinfo_fstype(mountinfo, "/home/me/file.txt").as_deref(),
            Some("ext4")
        );
    }
}
