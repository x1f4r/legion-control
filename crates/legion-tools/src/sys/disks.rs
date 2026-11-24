//! Which mounted file systems `sys.info` reports.
//!
//! A machine mounts far more than its disks: kernel interfaces, memory-backed scratch space,
//! container layers, and the same storage under several names (a btrfs subvolume per mount, an
//! APFS container's half-dozen system volumes). Listing them all would bury the two lines a person
//! or an agent wants, so the pseudo ones are dropped and each piece of storage is listed once,
//! under its shortest mount point.
//!
//! What counts as the same storage is deliberately narrow, because dropping a real disk is worse
//! than listing one twice: a volume label (`Data`, `Untitled`) is not an identity, and two empty
//! drives of one model report the same size to the byte.

use std::collections::HashSet;

use serde::Serialize;

/// One file system in `sys.info`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Disk {
    /// Where it is mounted.
    pub mount: String,
    /// Its file system type, such as `ext4`, `apfs` or `NTFS`.
    pub fs: String,
    /// Its size in bytes.
    pub total: u64,
    /// The bytes a program could still write.
    pub available: u64,
}

/// A mount as the system lists it, before anything is dropped.
#[derive(Clone, Debug)]
pub(super) struct Mounted {
    /// The device or volume name.
    pub(super) device: String,
    /// Its file system type.
    pub(super) fs: String,
    /// Where it is mounted.
    pub(super) mount: String,
    /// Its size in bytes.
    pub(super) total: u64,
    /// The bytes a program could still write.
    pub(super) available: u64,
}

impl From<&sysinfo::Disk> for Mounted {
    fn from(disk: &sysinfo::Disk) -> Mounted {
        Mounted {
            device: disk.name().to_string_lossy().into_owned(),
            fs: disk.file_system().to_string_lossy().into_owned(),
            mount: disk.mount_point().to_string_lossy().into_owned(),
            total: disk.total_space(),
            available: disk.available_space(),
        }
    }
}

/// File systems that hold no data of their own: kernel interfaces, memory, container layers and
/// read-only images. Compared ignoring case.
const PSEUDO: &[&str] = &[
    "autofs",
    "binfmt_misc",
    "bpf",
    "cgroup",
    "cgroup2",
    "configfs",
    "debugfs",
    "devfs",
    "devpts",
    "devtmpfs",
    "efivarfs",
    "fusectl",
    "hugetlbfs",
    "mqueue",
    "nsfs",
    "nullfs",
    "overlay",
    "proc",
    "pstore",
    "ramfs",
    "rpc_pipefs",
    "securityfs",
    "squashfs",
    "sysfs",
    "tmpfs",
    "tracefs",
];

/// FUSE file systems that are views of something else rather than storage: desktop portals, GNOME's
/// virtual file system, container and snap helpers. Real ones (`fuse.sshfs`, `fuse.mergerfs`,
/// `fuse.rclone`) are kept.
const PSEUDO_FUSE: &[&str] = &[
    "fuse.doc",
    "fuse.gvfs-fuse-daemon",
    "fuse.gvfsd-fuse",
    "fuse.kio-fuse",
    "fuse.lxcfs",
    "fuse.portal",
    "fuse.snapfuse",
    "fuse.xdg-document-portal",
];

/// File systems that pool one store across several volumes, each mount reporting the whole pool.
const POOLED: &[&str] = &["apfs", "btrfs", "zfs"];

/// The mounts worth reporting, each piece of storage once, under its shortest mount point.
///
/// Two mounts are the same storage when they are at the same mount point, when they name the same
/// device by its path (`/dev/sda1`, `server:/export`), or when they are volumes of a pooled file
/// system reporting exactly the same size and free space, which is how one APFS container or btrfs
/// pool looks from outside. A device name that is only a label says nothing.
pub(super) fn summarise(mounts: impl IntoIterator<Item = Mounted>) -> Vec<Disk> {
    let mut mounts: Vec<Mounted> = mounts
        .into_iter()
        .filter(|mount| mount.total > 0)
        .filter(|mount| {
            let fs = mount.fs.to_ascii_lowercase();
            !PSEUDO.contains(&fs.as_str()) && !PSEUDO_FUSE.contains(&fs.as_str())
        })
        .collect();
    mounts.sort_by(|a, b| {
        a.mount
            .len()
            .cmp(&b.mount.len())
            .then(a.mount.cmp(&b.mount))
    });
    let mut places = HashSet::new();
    let mut devices = HashSet::new();
    let mut pools = HashSet::new();
    let mut disks = Vec::new();
    for mount in mounts {
        let fresh_place = places.insert(mount.mount.clone());
        let fresh_device = !names_a_device(&mount.device) || devices.insert(mount.device.clone());
        let fs = mount.fs.to_ascii_lowercase();
        let fresh_pool =
            !POOLED.contains(&fs.as_str()) || pools.insert((fs, mount.total, mount.available));
        if fresh_place && fresh_device && fresh_pool {
            disks.push(Disk {
                mount: mount.mount,
                fs: mount.fs,
                total: mount.total,
                available: mount.available,
            });
        }
    }
    disks
}

/// Whether a mount's source names a device or an export by its path, which identifies it, rather
/// than by a label anyone can give two volumes.
fn names_a_device(device: &str) -> bool {
    device.starts_with('/') || device.starts_with(r"\\") || device.contains(":/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mounted(device: &str, fs: &str, mount: &str, total: u64, available: u64) -> Mounted {
        Mounted {
            device: device.to_owned(),
            fs: fs.to_owned(),
            mount: mount.to_owned(),
            total,
            available,
        }
    }

    #[test]
    fn pseudo_file_systems_and_empty_mounts_are_left_out() {
        let disks = summarise([
            mounted("tmpfs", "tmpfs", "/run", 100, 50),
            mounted("overlay", "overlay", "/var/lib/docker/x", 100, 50),
            mounted("portal", "fuse.portal", "/run/user/1000/doc", 100, 50),
            mounted(
                "gvfsd-fuse",
                "fuse.gvfsd-fuse",
                "/run/user/1000/gvfs",
                100,
                50,
            ),
            mounted("/dev/loop0", "squashfs", "/snap/core/1", 100, 0),
            mounted("/dev/sda1", "ext4", "/", 1000, 400),
            mounted("/dev/sdb1", "vfat", "/media/usb", 0, 0),
        ]);
        let mounts: Vec<&str> = disks.iter().map(|disk| disk.mount.as_str()).collect();
        assert_eq!(mounts, ["/"]);
    }

    #[test]
    fn real_fuse_file_systems_are_kept() {
        let disks = summarise([
            mounted("me@pi:/srv", "fuse.sshfs", "/home/me/pi", 500, 100),
            mounted("pool", "fuse.mergerfs", "/mnt/pool", 9000, 4000),
        ]);
        assert_eq!(disks.len(), 2);
    }

    #[test]
    fn the_same_storage_is_listed_once_under_its_shortest_mount() {
        let disks = summarise([
            mounted("/dev/nvme0n1p2", "btrfs", "/home", 2000, 900),
            mounted("/dev/nvme0n1p2", "btrfs", "/", 2000, 900),
            mounted("disk3s5", "apfs", "/System/Volumes/Data", 5000, 3000),
            mounted("disk3s1s1", "apfs", "/System/Volumes/Preboot", 5000, 3000),
            mounted("/dev/nvme0n1p1", "vfat", "/boot", 1, 1),
            mounted("/dev/sdb1", "btrfs", "/srv", 2000, 900),
        ]);
        let mounts: Vec<&str> = disks.iter().map(|disk| disk.mount.as_str()).collect();
        assert_eq!(mounts, ["/", "/boot", "/System/Volumes/Data"]);
    }

    #[test]
    fn labels_and_equal_sizes_alone_do_not_make_disks_the_same() {
        let disks = summarise([
            // Two external drives that are both called Data, on Windows and on macOS.
            mounted("Data", "NTFS", "D:\\", 4000, 4000),
            mounted("Data", "NTFS", "E:\\", 4000, 4000),
            mounted("Untitled", "exfat", "/Volumes/Untitled", 64, 64),
            mounted("Untitled", "exfat", "/Volumes/Untitled 1", 64, 64),
            // Two new, empty drives of one model.
            mounted("/dev/sdc1", "ext4", "/mnt/a", 1000, 990),
            mounted("/dev/sdd1", "ext4", "/mnt/b", 1000, 990),
        ]);
        assert_eq!(disks.len(), 6, "{disks:?}");
    }
}
