//! Whether a process exists at all, when its identity cannot be read.
//!
//! legion-os reads a process's identity, but a process another user owns can hide it (macOS gives
//! nothing about root's processes to anyone else), and legion-os then reports no process at all.
//! Telling "there is none" from "it is not ours to look at" takes asking the system about the pid
//! itself, which legion-os does not offer yet; until it does, the one question lives here.

/// What the system says about a pid, asked without reading anything about the process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Probe {
    /// A process has this pid, and this account may signal it.
    Exists,
    /// A process has this pid, and it belongs to another account or the system.
    #[cfg_attr(
        not(unix),
        allow(
            dead_code,
            reason = "Windows lists every process, so it never answers this"
        )
    )]
    NotPermitted,
    /// No process has this pid.
    Missing,
    /// The system gave an answer that says neither.
    Unknown,
}

/// Asks the system about `pid` with signal 0, which checks that a signal could be delivered and
/// delivers nothing: `EPERM` means a process is there but is not ours, `ESRCH` that there is none.
#[cfg(unix)]
pub(super) fn probe(pid: u32) -> Probe {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    let Ok(raw) = i32::try_from(pid) else {
        return Probe::Missing;
    };
    if raw <= 0 {
        return Probe::Unknown;
    }
    match kill(Pid::from_raw(raw), None) {
        Ok(()) => Probe::Exists,
        Err(Errno::EPERM) => Probe::NotPermitted,
        Err(Errno::ESRCH) => Probe::Missing,
        Err(_) => Probe::Unknown,
    }
}

/// Windows lists every process to every account, whoever owns it, so the process list answers.
/// Whether this account may stop it is learned when it tries.
#[cfg(not(unix))]
pub(super) fn probe(pid: u32) -> Probe {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

    let Ok(mut system) = System::new() else {
        return Probe::Unknown;
    };
    let target = Pid::from_u32(pid);
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[target]),
        true,
        ProcessRefreshKind::nothing(),
    );
    if system.process(target).is_some() {
        Probe::Exists
    } else {
        Probe::Missing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_exists_and_an_unused_pid_does_not() {
        assert_eq!(probe(std::process::id()), Probe::Exists);
        assert_eq!(probe(u32::MAX - 7), Probe::Missing);
    }
}
