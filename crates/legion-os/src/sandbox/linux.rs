//! Linux worker and `ffmpeg` sandboxing using `PR_SET_NO_NEW_PRIVS`, seccomp-BPF, and Landlock.
//!
//! - **Decode worker (`__thumb-worker`):** irreversibly sets `PR_SET_NO_NEW_PRIVS`, installs an
//!   empty Landlock ruleset (filesystem + network on ABI >= 4) when the running kernel supports
//!   Landlock, and installs a strict seccomp-BPF allowlist that permits only I/O on already-open
//!   descriptors, non-executable memory allocation, futex/clock/signals/getrandom, and exit. Any
//!   other syscall (including opening files, creating sockets, exec/fork/clone, or executable
//!   `mmap`/`mprotect`) kills the process immediately (`SECCOMP_RET_KILL_PROCESS`). Works cleanly
//!   on both dynamic glibc and static musl binaries (`x86_64` and `aarch64`).
//! - **`ffmpeg` video poster extraction:** prepares a Landlock ruleset in the parent before `fork`
//!   granting read+exec to `ffmpeg` and host dynamic library directories, read-only to the single
//!   input video file and `/dev/null`/`/dev/urandom`/`/proc/cpuinfo`/`/sys/devices/system/cpu`,
//!   and read-write only to the throwaway temp directory; plus a seccomp-BPF filter denying all
//!   socket/network syscalls (`EPERM`). Installed async-signal-safely inside `pre_exec`.

#![allow(unsafe_code)]

use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::sandbox::{SandboxReport, SandboxUnavailable};
use crate::{OsError, Result};

// ── BPF / seccomp constants (Linux UAPI, stable across glibc and musl) ─────────────────────────

const BPF_LD: u16 = 0x00;
const BPF_ALU: u16 = 0x04;
const BPF_JMP: u16 = 0x05;
const BPF_RET: u16 = 0x06;
const BPF_W: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_AND: u16 = 0x50;
const BPF_JEQ: u16 = 0x10;
const BPF_K: u16 = 0x00;

const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const SECCOMP_RET_KILL_THREAD: u32 = 0x0000_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;

const SECCOMP_DATA_NR_OFFSET: u32 = 0;
const SECCOMP_DATA_ARCH_OFFSET: u32 = 4;
// `args[1]` (`cmd` in `fcntl(fd, cmd, ...)`) starts at byte offset 16 + 1 * 8 = 24.
const SECCOMP_DATA_ARG1_LO_OFFSET: u32 = 24;
const SECCOMP_DATA_ARG1_HI_OFFSET: u32 = 28;
// `args[2]` (`prot` in both `mmap` and `mprotect`) starts at byte offset 16 + 2 * 8 = 32.
// Both x86_64 and aarch64 Linux targets are little-endian.
const SECCOMP_DATA_ARG2_LO_OFFSET: u32 = 32;
const SECCOMP_DATA_ARG2_HI_OFFSET: u32 = 36;

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH_CURRENT: u32 = 0xc000_003e; // AUDIT_ARCH_X86_64
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH_CURRENT: u32 = 0xc000_00b7; // AUDIT_ARCH_AARCH64

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct SockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

#[repr(C)]
struct SockFprog {
    len: u16,
    filter: *const SockFilter,
}

#[inline]
const fn bpf_stmt(code: u16, k: u32) -> SockFilter {
    SockFilter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

#[inline]
const fn bpf_jump(code: u16, k: u32, jt: u8, jf: u8) -> SockFilter {
    SockFilter { code, jt, jf, k }
}

// ── Landlock Linux UAPI constants (unified across x86_64 and aarch64) ──────────────────────────

const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
const SYS_LANDLOCK_ADD_RULE: libc::c_long = 445;
const SYS_LANDLOCK_RESTRICT_SELF: libc::c_long = 446;

const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1 << 0;
const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;

const LANDLOCK_ACCESS_FS_EXECUTE: u64 = 1 << 0;
const LANDLOCK_ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
const LANDLOCK_ACCESS_FS_READ_FILE: u64 = 1 << 2;
const LANDLOCK_ACCESS_FS_READ_DIR: u64 = 1 << 3;
const LANDLOCK_ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
const LANDLOCK_ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
const LANDLOCK_ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
const LANDLOCK_ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
const LANDLOCK_ACCESS_FS_MAKE_REG: u64 = 1 << 8;
const LANDLOCK_ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
const LANDLOCK_ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
const LANDLOCK_ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
const LANDLOCK_ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
const LANDLOCK_ACCESS_FS_REFER: u64 = 1 << 13;
const LANDLOCK_ACCESS_FS_TRUNCATE: u64 = 1 << 14;
const LANDLOCK_ACCESS_FS_IOCTL_DEV: u64 = 1 << 15;

const LANDLOCK_ACCESS_NET_BIND_TCP: u64 = 1 << 0;
const LANDLOCK_ACCESS_NET_CONNECT_TCP: u64 = 1 << 1;

/// Filesystem rights valid on non-directory (regular/device) inodes.
const LANDLOCK_FILE_ONLY_MASK: u64 = LANDLOCK_ACCESS_FS_EXECUTE
    | LANDLOCK_ACCESS_FS_WRITE_FILE
    | LANDLOCK_ACCESS_FS_READ_FILE
    | LANDLOCK_ACCESS_FS_TRUNCATE
    | LANDLOCK_ACCESS_FS_IOCTL_DEV;

#[repr(C)]
struct LandlockRulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
}

#[repr(C, packed)]
struct LandlockPathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

fn landlock_abi_version() -> Option<i32> {
    // SAFETY: `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)` is a read-only
    // kernel ABI probe taking no pointers.
    let rc = unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if rc > 0 { i32::try_from(rc).ok() } else { None }
}

fn landlock_handled_for_abi(abi: i32) -> (LandlockRulesetAttr, usize) {
    let mut fs = LANDLOCK_ACCESS_FS_EXECUTE
        | LANDLOCK_ACCESS_FS_WRITE_FILE
        | LANDLOCK_ACCESS_FS_READ_FILE
        | LANDLOCK_ACCESS_FS_READ_DIR
        | LANDLOCK_ACCESS_FS_REMOVE_DIR
        | LANDLOCK_ACCESS_FS_REMOVE_FILE
        | LANDLOCK_ACCESS_FS_MAKE_CHAR
        | LANDLOCK_ACCESS_FS_MAKE_DIR
        | LANDLOCK_ACCESS_FS_MAKE_REG
        | LANDLOCK_ACCESS_FS_MAKE_SOCK
        | LANDLOCK_ACCESS_FS_MAKE_FIFO
        | LANDLOCK_ACCESS_FS_MAKE_BLOCK
        | LANDLOCK_ACCESS_FS_MAKE_SYM;
    if abi >= 2 {
        fs |= LANDLOCK_ACCESS_FS_REFER;
    }
    if abi >= 3 {
        fs |= LANDLOCK_ACCESS_FS_TRUNCATE;
    }
    if abi >= 5 {
        fs |= LANDLOCK_ACCESS_FS_IOCTL_DEV;
    }
    if abi >= 4 {
        (
            LandlockRulesetAttr {
                handled_access_fs: fs,
                handled_access_net: LANDLOCK_ACCESS_NET_BIND_TCP | LANDLOCK_ACCESS_NET_CONNECT_TCP,
            },
            std::mem::size_of::<LandlockRulesetAttr>(),
        )
    } else {
        (
            LandlockRulesetAttr {
                handled_access_fs: fs,
                handled_access_net: 0,
            },
            std::mem::size_of::<u64>(),
        )
    }
}

fn create_landlock_ruleset(abi: i32) -> io::Result<(OwnedFd, u64)> {
    let (attr, size) = landlock_handled_for_abi(abi);
    // SAFETY: `attr` is initialized and `size` is <= `size_of::<LandlockRulesetAttr>()`.
    let fd = unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            (&raw const attr).cast::<libc::c_void>(),
            size,
            0u32,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: kernel returned a fresh owned file descriptor (`O_CLOEXEC` set by Landlock).
    let owned = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    Ok((owned, attr.handled_access_fs))
}

fn add_landlock_path_rule(
    ruleset_fd: i32,
    path: &Path,
    requested_access: u64,
    handled_fs: u64,
) -> io::Result<()> {
    let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
        return Ok(());
    };
    // SAFETY: `c_path` is NUL-terminated; `O_PATH | O_CLOEXEC` opens a path reference without
    // reading or executing its contents and closes on exec.
    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh owned descriptor returned by `open`.
    let path_fd = unsafe { OwnedFd::from_raw_fd(fd) };

    let mut stat_buf: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `path_fd` is a valid open descriptor and `stat_buf` is valid writable storage.
    let is_dir = unsafe { libc::fstat(path_fd.as_raw_fd(), &mut stat_buf) == 0 }
        && (stat_buf.st_mode & libc::S_IFMT) == libc::S_IFDIR;

    let mut allowed = requested_access & handled_fs;
    if !is_dir {
        allowed &= LANDLOCK_FILE_ONLY_MASK;
    }
    if allowed == 0 {
        return Ok(());
    }

    let attr = LandlockPathBeneathAttr {
        allowed_access: allowed,
        parent_fd: path_fd.as_raw_fd(),
    };
    // SAFETY: `ruleset_fd` and `attr.parent_fd` are valid open descriptors; `attr` is properly
    // packed matching Linux `struct landlock_path_beneath_attr`.
    let rc = unsafe {
        libc::syscall(
            SYS_LANDLOCK_ADD_RULE,
            ruleset_fd,
            LANDLOCK_RULE_PATH_BENEATH,
            (&raw const attr).cast::<libc::c_void>(),
            0u32,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ── Worker strict seccomp-BPF allowlist ────────────────────────────────────────────────────────

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn worker_unconditional_syscalls() -> &'static [u32] {
    #[cfg(target_arch = "x86_64")]
    const LIST: &[u32] = &[
        libc::SYS_read as u32,
        libc::SYS_pread64 as u32,
        libc::SYS_readv as u32,
        libc::SYS_write as u32,
        libc::SYS_writev as u32,
        libc::SYS_lseek as u32,
        libc::SYS_fstat as u32,
        libc::SYS_close as u32,
        libc::SYS_brk as u32,
        libc::SYS_munmap as u32,
        libc::SYS_mremap as u32,
        libc::SYS_madvise as u32,
        libc::SYS_futex as u32,
        libc::SYS_sched_yield as u32,
        libc::SYS_clock_gettime as u32,
        libc::SYS_gettimeofday as u32,
        libc::SYS_getrandom as u32,
        libc::SYS_rt_sigreturn as u32,
        libc::SYS_rt_sigprocmask as u32,
        libc::SYS_sigaltstack as u32,
        libc::SYS_getpid as u32,
        libc::SYS_gettid as u32,
        libc::SYS_restart_syscall as u32,
        libc::SYS_exit as u32,
        libc::SYS_exit_group as u32,
    ];

    #[cfg(target_arch = "aarch64")]
    const LIST: &[u32] = &[
        libc::SYS_read as u32,
        libc::SYS_pread64 as u32,
        libc::SYS_readv as u32,
        libc::SYS_write as u32,
        libc::SYS_writev as u32,
        libc::SYS_lseek as u32,
        libc::SYS_fstat as u32,
        libc::SYS_close as u32,
        libc::SYS_brk as u32,
        libc::SYS_munmap as u32,
        libc::SYS_mremap as u32,
        libc::SYS_madvise as u32,
        libc::SYS_futex as u32,
        libc::SYS_sched_yield as u32,
        libc::SYS_clock_gettime as u32,
        libc::SYS_gettimeofday as u32,
        libc::SYS_getrandom as u32,
        libc::SYS_rt_sigreturn as u32,
        libc::SYS_rt_sigprocmask as u32,
        libc::SYS_sigaltstack as u32,
        libc::SYS_getpid as u32,
        libc::SYS_gettid as u32,
        libc::SYS_restart_syscall as u32,
        libc::SYS_exit as u32,
        libc::SYS_exit_group as u32,
    ];

    LIST
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn build_worker_seccomp_filter(kill_action: u32) -> Vec<SockFilter> {
    let allow_list = worker_unconditional_syscalls();
    // Layout:
    //   [0]      LD arch
    //   [1]      JEQ AUDIT_ARCH_CURRENT -> next, else -> kill
    //   [2]      LD nr
    //   [3 .. 3+N)               JEQ allowed_nr -> allow
    //   [3+N]                    JEQ SYS_mmap -> prot_check
    //   [3+N+1]                  JEQ SYS_mprotect -> prot_check
    //   [3+N+2]                  JEQ SYS_fcntl -> fcntl_check
    //   [3+N+3]                  RET kill_action            (idx = kill_idx)
    //   [3+N+4]  prot_check:     LD args[2] high 32 bits
    //   [3+N+5]                  JEQ 0 -> next, else -> kill_2
    //   [3+N+6]                  LD args[2] low 32 bits
    //   [3+N+7]                  AND PROT_EXEC
    //   [3+N+8]                  JEQ 0 -> allow, else -> kill_2
    //   [3+N+9]  fcntl_check:    LD args[1] high 32 bits
    //   [3+N+10]                 JEQ 0 -> next, else -> kill_2
    //   [3+N+11]                 LD args[1] low 32 bits
    //   [3+N+12]                 JEQ F_GETFD -> allow, else -> kill_2
    //   [3+N+13]                 RET kill_action            (idx = kill_idx_2)
    //   [3+N+14]                 RET SECCOMP_RET_ALLOW      (idx = allow_idx)
    let n = allow_list.len();
    let kill_idx = 3 + n + 3;
    let prot_check_idx = 3 + n + 4;
    let fcntl_check_idx = 3 + n + 9;
    let kill_idx_2 = 3 + n + 13;
    let allow_idx = 3 + n + 14;

    let mut prog = Vec::with_capacity(allow_idx + 1);
    // [0..2]: verify architecture.
    prog.push(bpf_stmt(BPF_LD | BPF_W | BPF_ABS, SECCOMP_DATA_ARCH_OFFSET));
    prog.push(bpf_jump(
        BPF_JMP | BPF_JEQ | BPF_K,
        AUDIT_ARCH_CURRENT,
        0,
        (kill_idx - 2) as u8,
    ));
    // [2]: load syscall number.
    prog.push(bpf_stmt(BPF_LD | BPF_W | BPF_ABS, SECCOMP_DATA_NR_OFFSET));

    // [3..3+N]: unconditional allowlist.
    for (i, &nr) in allow_list.iter().enumerate() {
        let cur = 3 + i;
        let jt = (allow_idx - cur - 1) as u8;
        prog.push(bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, nr, jt, 0));
    }

    // [3+N]: mmap -> prot_check.
    {
        let cur = 3 + n;
        let jt = (prot_check_idx - cur - 1) as u8;
        prog.push(bpf_jump(
            BPF_JMP | BPF_JEQ | BPF_K,
            libc::SYS_mmap as u32,
            jt,
            0,
        ));
    }
    // [3+N+1]: mprotect -> prot_check.
    {
        let cur = 3 + n + 1;
        let jt = (prot_check_idx - cur - 1) as u8;
        prog.push(bpf_jump(
            BPF_JMP | BPF_JEQ | BPF_K,
            libc::SYS_mprotect as u32,
            jt,
            0,
        ));
    }
    // [3+N+2]: fcntl -> fcntl_check (strictly read-only F_GETFD for Rust std debug `OwnedFd::drop`).
    {
        let cur = 3 + n + 2;
        let jt = (fcntl_check_idx - cur - 1) as u8;
        prog.push(bpf_jump(
            BPF_JMP | BPF_JEQ | BPF_K,
            libc::SYS_fcntl as u32,
            jt,
            0,
        ));
    }
    // [kill_idx]: default deny.
    prog.push(bpf_stmt(BPF_RET | BPF_K, kill_action));

    // [prot_check_idx]: check `prot` (arg 2) has high 32 bits == 0 and `(prot & PROT_EXEC) == 0`.
    prog.push(bpf_stmt(
        BPF_LD | BPF_W | BPF_ABS,
        SECCOMP_DATA_ARG2_HI_OFFSET,
    ));
    prog.push(bpf_jump(
        BPF_JMP | BPF_JEQ | BPF_K,
        0,
        0,
        (kill_idx_2 - (prot_check_idx + 1) - 1) as u8,
    ));
    prog.push(bpf_stmt(
        BPF_LD | BPF_W | BPF_ABS,
        SECCOMP_DATA_ARG2_LO_OFFSET,
    ));
    prog.push(bpf_stmt(BPF_ALU | BPF_AND | BPF_K, libc::PROT_EXEC as u32));
    prog.push(bpf_jump(
        BPF_JMP | BPF_JEQ | BPF_K,
        0,
        (allow_idx - (prot_check_idx + 4) - 1) as u8,
        (kill_idx_2 - (prot_check_idx + 4) - 1) as u8,
    ));

    // [fcntl_check_idx]: permit ONLY read-only `fcntl(fd, F_GETFD)` (needed by Rust std debug
    // `OwnedFd::drop` assertion); kill on every other fcntl command (F_DUPFD, F_SETFL, locks, ...).
    prog.push(bpf_stmt(
        BPF_LD | BPF_W | BPF_ABS,
        SECCOMP_DATA_ARG1_HI_OFFSET,
    ));
    prog.push(bpf_jump(
        BPF_JMP | BPF_JEQ | BPF_K,
        0,
        0,
        (kill_idx_2 - (fcntl_check_idx + 1) - 1) as u8,
    ));
    prog.push(bpf_stmt(
        BPF_LD | BPF_W | BPF_ABS,
        SECCOMP_DATA_ARG1_LO_OFFSET,
    ));
    prog.push(bpf_jump(
        BPF_JMP | BPF_JEQ | BPF_K,
        libc::F_GETFD as u32,
        (allow_idx - (fcntl_check_idx + 3) - 1) as u8,
        0,
    ));

    // [kill_idx_2]: kill on disallowed arguments.
    prog.push(bpf_stmt(BPF_RET | BPF_K, kill_action));
    // [allow_idx]: allow.
    prog.push(bpf_stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW));

    debug_assert_eq!(prog.len(), allow_idx + 1);
    prog
}

fn install_seccomp_filter(filter: &[SockFilter]) -> io::Result<()> {
    let len = u16::try_from(filter.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "seccomp filter too long"))?;
    let prog = SockFprog {
        len,
        filter: filter.as_ptr(),
    };
    // SAFETY: `prog` points to a valid `SockFilter` array for the duration of `prctl`, and
    // `PR_SET_NO_NEW_PRIVS` was already installed by the caller.
    let rc = unsafe {
        libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER,
            (&raw const prog).cast::<libc::c_void>(),
            0,
            0,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn apply_worker_sandbox() -> std::result::Result<SandboxReport, SandboxUnavailable> {
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        return Err(SandboxUnavailable {
            platform: "linux".to_owned(),
            missing: format!("seccomp-bpf architecture ({})", std::env::consts::ARCH),
            detail: "Linux worker seccomp allowlist is only implemented for x86_64 and aarch64"
                .to_owned(),
        });
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        // Step 1: PR_SET_NO_NEW_PRIVS is mandatory for unprivileged Landlock and seccomp-BPF.
        // SAFETY: `prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)` takes scalar arguments only.
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            let err = io::Error::last_os_error();
            return Err(SandboxUnavailable {
                platform: "linux".to_owned(),
                missing: "PR_SET_NO_NEW_PRIVS".to_owned(),
                detail: format!("prctl(PR_SET_NO_NEW_PRIVS) failed: {err}"),
            });
        }

        // Step 2: Best-effort empty Landlock ruleset (zero path rules, zero net rules on ABI >= 4)
        // as defense-in-depth beneath seccomp. Absence of kernel Landlock alone does not mark the
        // worker sandbox unavailable because seccomp already denies all open/socket/exec syscalls.
        let landlock_abi = apply_empty_landlock_ruleset();

        // Step 3: Mandatory strict seccomp-BPF allowlist. Prefer KILL_PROCESS (>= Linux 4.14) and
        // fall back to KILL_THREAD only if an older kernel rejects KILL_PROCESS with EINVAL.
        let primary = build_worker_seccomp_filter(SECCOMP_RET_KILL_PROCESS);
        let installed = match install_seccomp_filter(&primary) {
            Ok(()) => Ok("kill-process"),
            Err(err) if err.raw_os_error() == Some(libc::EINVAL) => {
                let fallback = build_worker_seccomp_filter(SECCOMP_RET_KILL_THREAD);
                install_seccomp_filter(&fallback).map(|()| "kill-thread")
            }
            Err(err) => Err(err),
        };

        let kill_mode = match installed {
            Ok(mode) => mode,
            Err(err) => {
                return Err(SandboxUnavailable {
                    platform: "linux".to_owned(),
                    missing: "seccomp-bpf".to_owned(),
                    detail: format!("prctl(PR_SET_SECCOMP) failed: {err}"),
                });
            }
        };

        let (mechanism, detail) = match landlock_abi {
            Some(abi) => (
                "seccomp-bpf+landlock".to_owned(),
                format!(
                    "Linux decode worker sandbox active (seccomp-BPF allowlist [{kill_mode}] + empty Landlock ABI v{abi})."
                ),
            ),
            None => (
                "seccomp-bpf".to_owned(),
                format!(
                    "Linux decode worker sandbox active (seccomp-BPF allowlist [{kill_mode}]; Landlock unavailable on this kernel)."
                ),
            ),
        };

        Ok(SandboxReport {
            available: true,
            complete: true,
            platform: "linux".to_owned(),
            mechanism,
            detail,
            missing: None,
        })
    }
}

fn apply_empty_landlock_ruleset() -> Option<i32> {
    let abi = landlock_abi_version()?;
    let (ruleset_fd, _) = create_landlock_ruleset(abi).ok()?;
    // SAFETY: `ruleset_fd` is a valid empty Landlock ruleset and `PR_SET_NO_NEW_PRIVS` is active.
    let rc = unsafe { libc::syscall(SYS_LANDLOCK_RESTRICT_SELF, ruleset_fd.as_raw_fd(), 0u32) };
    if rc == 0 { Some(abi) } else { None }
}

// ── Linux `ffmpeg` sandbox (Landlock filesystem box + seccomp network deny) ────────────────────

/// Pre-built Linux sandbox artifacts passed into `Command::pre_exec` for `ffmpeg`.
///
/// Built in the parent process BEFORE `fork` so the `pre_exec` closure allocates nothing and makes
/// only async-signal-safe syscalls (`prctl`, `landlock_restrict_self`).
pub(crate) struct PreparedFfmpegSandbox {
    /// Optional Landlock ruleset fd (`O_CLOEXEC`); dropped in parent after `spawn`.
    landlock_fd: Option<OwnedFd>,
    /// Pre-built seccomp-BPF filter denying all network syscalls with `EPERM`.
    seccomp_filter: Vec<SockFilter>,
}

impl PreparedFfmpegSandbox {
    /// Installs this prepared sandbox inside `Command::pre_exec` (between `fork` and `exec`).
    ///
    /// Returns an `io::Error` if `PR_SET_NO_NEW_PRIVS`, Landlock (when built), or seccomp fails
    /// so `ffmpeg` never runs unsandboxed.
    pub(crate) fn apply_in_pre_exec(
        landlock_raw_fd: Option<i32>,
        filter: &[SockFilter],
    ) -> io::Result<()> {
        // SAFETY: `prctl(PR_SET_NO_NEW_PRIVS, ...)` is async-signal-safe with scalar arguments.
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if let Some(fd) = landlock_raw_fd {
            // SAFETY: `fd` is a valid Landlock ruleset fd inherited across `fork` before `exec`
            // closes `O_CLOEXEC` descriptors; `landlock_restrict_self` is a raw syscall.
            let rc = unsafe { libc::syscall(SYS_LANDLOCK_RESTRICT_SELF, fd, 0u32) };
            if rc != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        install_seccomp_filter(filter)?;
        Ok(())
    }

    pub(crate) fn landlock_raw_fd(&self) -> Option<i32> {
        self.landlock_fd.as_ref().map(AsRawFd::as_raw_fd)
    }

    pub(crate) fn seccomp_filter_clone(&self) -> Vec<SockFilter> {
        self.seccomp_filter.clone()
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn build_ffmpeg_network_deny_filter() -> Vec<SockFilter> {
    let denied: &[u32] = &[
        libc::SYS_socket as u32,
        libc::SYS_socketpair as u32,
        libc::SYS_connect as u32,
        libc::SYS_bind as u32,
        libc::SYS_listen as u32,
        libc::SYS_accept as u32,
        libc::SYS_accept4 as u32,
        libc::SYS_sendto as u32,
        libc::SYS_sendmsg as u32,
        libc::SYS_sendmmsg as u32,
    ];
    let deny_action = SECCOMP_RET_ERRNO | ((libc::EPERM as u32) & 0xffff);
    let n = denied.len();
    let deny_idx = 3 + n + 1;

    let mut prog = Vec::with_capacity(deny_idx + 1);
    prog.push(bpf_stmt(BPF_LD | BPF_W | BPF_ABS, SECCOMP_DATA_ARCH_OFFSET));
    // Wrong architecture -> deny immediately.
    prog.push(bpf_jump(
        BPF_JMP | BPF_JEQ | BPF_K,
        AUDIT_ARCH_CURRENT,
        0,
        (deny_idx - 2) as u8,
    ));
    prog.push(bpf_stmt(BPF_LD | BPF_W | BPF_ABS, SECCOMP_DATA_NR_OFFSET));
    for (i, &nr) in denied.iter().enumerate() {
        let cur = 3 + i;
        let jt = (deny_idx - cur - 1) as u8;
        prog.push(bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, nr, jt, 0));
    }
    prog.push(bpf_stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW));
    prog.push(bpf_stmt(BPF_RET | BPF_K, deny_action));
    debug_assert_eq!(prog.len(), deny_idx + 1);
    prog
}

/// Parses `/etc/ld.so.conf` (and `include` globs) to discover extra host dynamic library
/// directories (for example `/opt/cuda/lib64` or `/usr/lib/x86_64-linux-gnu`) that `ffmpeg` needs
/// read+exec access to under Landlock.
fn collect_ld_so_conf_dirs(root_conf: &Path) -> Vec<PathBuf> {
    let mut dirs = BTreeSet::new();
    let mut visited = BTreeSet::new();
    parse_ld_so_conf_recursive(root_conf, &mut dirs, &mut visited, 0);
    dirs.into_iter().collect()
}

fn parse_ld_so_conf_recursive(
    conf_path: &Path,
    dirs: &mut BTreeSet<PathBuf>,
    visited: &mut BTreeSet<PathBuf>,
    depth: usize,
) {
    if depth > 4 || !visited.insert(conf_path.to_path_buf()) {
        return;
    }
    let Ok(text) = fs::read_to_string(conf_path) else {
        return;
    };
    let parent = conf_path.parent().unwrap_or_else(|| Path::new("/etc"));
    for raw_line in text.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("include")
            && rest.starts_with(char::is_whitespace)
        {
            for pattern in rest.split_whitespace() {
                let resolved = if Path::new(pattern).is_absolute() {
                    PathBuf::from(pattern)
                } else {
                    parent.join(pattern)
                };
                for matched in expand_simple_conf_glob(&resolved) {
                    parse_ld_so_conf_recursive(&matched, dirs, visited, depth + 1);
                }
            }
            continue;
        }
        if line.starts_with("hwcap") {
            continue;
        }
        let path = PathBuf::from(line);
        if path.is_absolute() {
            dirs.insert(path);
        }
    }
}

/// Expands a single-segment filename glob such as `/etc/ld.so.conf.d/*.conf` without pulling in an
/// external glob crate.
fn expand_simple_conf_glob(pattern: &Path) -> Vec<PathBuf> {
    let Some(name) = pattern.file_name().and_then(|s| s.to_str()) else {
        return Vec::new();
    };
    if !name.contains('*') {
        return if pattern.is_file() {
            vec![pattern.to_path_buf()]
        } else {
            Vec::new()
        };
    }
    let Some(dir) = pattern.parent() else {
        return Vec::new();
    };
    let Some((prefix, suffix)) = name.split_once('*') else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut matches = Vec::new();
    for entry in entries.flatten() {
        let entry_name = entry.file_name();
        let Some(s) = entry_name.to_str() else {
            continue;
        };
        if s.starts_with(prefix) && s.ends_with(suffix) && s.len() >= prefix.len() + suffix.len() {
            let path = entry.path();
            if path.is_file() {
                matches.push(path);
            }
        }
    }
    matches.sort();
    matches
}

pub(crate) fn prepare_ffmpeg_sandbox(
    ffmpeg_bin: &Path,
    input_file: Option<&Path>,
    temp_dir: &Path,
) -> Result<PreparedFfmpegSandbox> {
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _ = (ffmpeg_bin, input_file, temp_dir);
        return Err(OsError::Unsupported {
            detail: format!(
                "Linux ffmpeg seccomp sandbox is not implemented for architecture {}",
                std::env::consts::ARCH
            ),
        });
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        let seccomp_filter = build_ffmpeg_network_deny_filter();
        let landlock_fd = if let Some(abi) = landlock_abi_version() {
            let (ruleset_fd, handled_fs) =
                create_landlock_ruleset(abi).map_err(|err| OsError::Platform {
                    detail: format!("failed to create Landlock ruleset for ffmpeg: {err}"),
                })?;

            let ro_exec = LANDLOCK_ACCESS_FS_READ_FILE
                | LANDLOCK_ACCESS_FS_EXECUTE
                | LANDLOCK_ACCESS_FS_READ_DIR;
            let ro_only = LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_READ_DIR;
            let rw_temp = LANDLOCK_ACCESS_FS_READ_FILE
                | LANDLOCK_ACCESS_FS_WRITE_FILE
                | LANDLOCK_ACCESS_FS_READ_DIR
                | LANDLOCK_ACCESS_FS_REMOVE_DIR
                | LANDLOCK_ACCESS_FS_REMOVE_FILE
                | LANDLOCK_ACCESS_FS_MAKE_DIR
                | LANDLOCK_ACCESS_FS_MAKE_REG
                | LANDLOCK_ACCESS_FS_TRUNCATE;

            // Canonical + given paths for the ffmpeg binary itself.
            let mut exec_paths: BTreeSet<PathBuf> = BTreeSet::new();
            exec_paths.insert(ffmpeg_bin.to_path_buf());
            if let Ok(canon) = fs::canonicalize(ffmpeg_bin) {
                if let Some(prefix) = canon.parent().and_then(Path::parent) {
                    for sub in ["lib", "lib64"] {
                        let cand = prefix.join(sub);
                        if cand.exists() {
                            exec_paths.insert(cand);
                        }
                    }
                }
                if canon.starts_with("/nix/store") {
                    exec_paths.insert(PathBuf::from("/nix/store"));
                }
                exec_paths.insert(canon);
            }

            // Standard system dynamic linker and library directories plus `/etc/ld.so.conf*`.
            for dir in [
                "/lib",
                "/lib64",
                "/lib32",
                "/usr/lib",
                "/usr/lib64",
                "/usr/lib32",
                "/usr/local/lib",
                "/usr/local/lib64",
            ] {
                exec_paths.insert(PathBuf::from(dir));
            }
            for dir in collect_ld_so_conf_dirs(Path::new("/etc/ld.so.conf")) {
                exec_paths.insert(dir);
            }
            for path in exec_paths {
                if path.exists() {
                    let _ =
                        add_landlock_path_rule(ruleset_fd.as_raw_fd(), &path, ro_exec, handled_fs);
                }
            }

            // Minimal read-only system files needed by ld.so / libav / CPU detection.
            for ro_path in [
                "/etc/ld.so.cache",
                "/etc/ld.so.preload",
                "/dev/null",
                "/dev/urandom",
                "/dev/random",
                "/proc/cpuinfo",
                "/proc/self/auxv",
                "/sys/devices/system/cpu",
            ] {
                let p = Path::new(ro_path);
                if p.exists() {
                    let _ = add_landlock_path_rule(ruleset_fd.as_raw_fd(), p, ro_only, handled_fs);
                }
            }
            if Path::new("/dev/null").exists() {
                let _ = add_landlock_path_rule(
                    ruleset_fd.as_raw_fd(),
                    Path::new("/dev/null"),
                    LANDLOCK_ACCESS_FS_WRITE_FILE,
                    handled_fs,
                );
            }

            // Grant read-only access to the ONE input video file (when opened by path).
            if let Some(input) = input_file {
                let mut input_paths = BTreeSet::new();
                input_paths.insert(input.to_path_buf());
                if let Ok(canon) = fs::canonicalize(input) {
                    input_paths.insert(canon);
                }
                for p in input_paths {
                    add_landlock_path_rule(
                        ruleset_fd.as_raw_fd(),
                        &p,
                        LANDLOCK_ACCESS_FS_READ_FILE,
                        handled_fs,
                    )
                    .map_err(|err| OsError::Platform {
                        detail: format!(
                            "failed to add Landlock read rule for ffmpeg input {}: {err}",
                            p.display()
                        ),
                    })?;
                }
            }

            // Grant read+write access ONLY to the dedicated throwaway temp directory.
            let mut temp_paths = BTreeSet::new();
            temp_paths.insert(temp_dir.to_path_buf());
            if let Ok(canon) = fs::canonicalize(temp_dir) {
                temp_paths.insert(canon);
            }
            for p in temp_paths {
                add_landlock_path_rule(ruleset_fd.as_raw_fd(), &p, rw_temp, handled_fs).map_err(
                    |err| OsError::Platform {
                        detail: format!(
                            "failed to add Landlock write rule for ffmpeg temp dir {}: {err}",
                            p.display()
                        ),
                    },
                )?;
            }

            Some(ruleset_fd)
        } else {
            None
        };

        Ok(PreparedFfmpegSandbox {
            landlock_fd,
            seccomp_filter,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_seccomp_filter_has_valid_jump_offsets_and_blocks_open_socket_exec() {
        let prog = build_worker_seccomp_filter(SECCOMP_RET_KILL_PROCESS);
        assert!(prog.len() <= u16::MAX as usize);
        let allow_list = worker_unconditional_syscalls();
        // Verify forbidden syscalls are NOT in the unconditional allowlist.
        #[cfg(target_arch = "x86_64")]
        let forbidden: &[u32] = &[
            libc::SYS_open as u32,
            libc::SYS_openat as u32,
            libc::SYS_socket as u32,
            libc::SYS_connect as u32,
            libc::SYS_bind as u32,
            libc::SYS_fork as u32,
            libc::SYS_clone as u32,
            libc::SYS_execve as u32,
            libc::SYS_mmap as u32,
            libc::SYS_mprotect as u32,
            libc::SYS_fcntl as u32,
        ];
        #[cfg(target_arch = "aarch64")]
        let forbidden: &[u32] = &[
            libc::SYS_openat as u32,
            libc::SYS_socket as u32,
            libc::SYS_connect as u32,
            libc::SYS_bind as u32,
            libc::SYS_clone as u32,
            libc::SYS_execve as u32,
            libc::SYS_mmap as u32,
            libc::SYS_mprotect as u32,
            libc::SYS_fcntl as u32,
        ];
        for &nr in forbidden {
            assert!(
                !allow_list.contains(&nr),
                "syscall {nr} must not be unconditionally allowed"
            );
        }
        // Every forward jump must land strictly inside `prog`.
        for (idx, insn) in prog.iter().enumerate() {
            if (insn.code & 0x07) == BPF_JMP {
                let t = idx + 1 + usize::from(insn.jt);
                let f = idx + 1 + usize::from(insn.jf);
                assert!(
                    t < prog.len(),
                    "jt out of bounds at {idx}: {t} >= {}",
                    prog.len()
                );
                assert!(
                    f < prog.len(),
                    "jf out of bounds at {idx}: {f} >= {}",
                    prog.len()
                );
            }
        }
    }

    #[test]
    fn ld_so_conf_parser_expands_nested_globs_and_skips_comments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sub = dir.path().join("ld.so.conf.d");
        fs::create_dir_all(&sub).expect("mkdir");
        fs::write(
            sub.join("cuda.conf"),
            "# comment\n/opt/cuda/lib64\n  /usr/local/lib/custom  # trailing comment\n",
        )
        .expect("write cuda.conf");
        let main = dir.path().join("ld.so.conf");
        fs::write(&main, "include ld.so.conf.d/*.conf\n/opt/extra/lib\n").expect("write main");

        let parsed = collect_ld_so_conf_dirs(&main);
        assert!(parsed.contains(&PathBuf::from("/opt/cuda/lib64")));
        assert!(parsed.contains(&PathBuf::from("/usr/local/lib/custom")));
        assert!(parsed.contains(&PathBuf::from("/opt/extra/lib")));
    }

    #[test]
    fn ffmpeg_network_deny_filter_has_valid_jump_offsets() {
        let prog = build_ffmpeg_network_deny_filter();
        for (idx, insn) in prog.iter().enumerate() {
            if (insn.code & 0x07) == BPF_JMP {
                let t = idx + 1 + usize::from(insn.jt);
                let f = idx + 1 + usize::from(insn.jf);
                assert!(t < prog.len(), "jt out of bounds at {idx}");
                assert!(f < prog.len(), "jf out of bounds at {idx}");
            }
        }
    }
}
