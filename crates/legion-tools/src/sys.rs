//! `sys`: what the machine is and how busy it is, the processes it runs, and stopping one.
//!
//! System and process figures come from `sysinfo`, whose reads block on the file system or the
//! kernel, so every read runs on the blocking pool and the async runtime keeps serving other
//! calls. CPU usage is a rate, which takes two samples a short interval apart; the interval is
//! awaited on the runtime between two blocking reads rather than slept through on a thread.
//!
//! One rule for every result of the `sys` and `exec` families: a field that may have no value is
//! left out when it has none, never sent as `null`, so a caller tests for presence and never for
//! both.

mod disks;
mod gpu;
mod probe;

use std::collections::HashMap;
use std::time::Duration;

use legion_core::proto::{Effect, ErrorBody, ErrorCode};
use legion_core::{Ctx, NoArgs, Registry, ToolError, audit};
use legion_os::OsError;
use legion_os::procid::{self, ProcessId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sysinfo::{
    CpuRefreshKind, Disks, MemoryRefreshKind, Pid, ProcessRefreshKind, ProcessesToUpdate,
    RefreshKind, System, ThreadKind, UpdateKind, Users,
};

pub use disks::Disk;
pub use gpu::Gpu;

/// Adds the `sys` tools.
pub fn register(registry: &mut Registry) {
    registry.add_fn(
        "sys.info",
        Effect::Read,
        "Describe this machine: host name, operating system, uptime and boot id, CPU model, core count and current usage, memory and swap, disks, and NVIDIA GPUs. Sizes are bytes.",
        info,
    );
    registry.add_fn(
        "sys.procs",
        Effect::Read,
        "List the processes running on this machine, busiest first, with their id, parent, name, command line, user, CPU usage (percent of one core), memory in bytes and start time. `filter` keeps those whose name or command line contains it. Command lines are shown with secret-looking values (passwords, tokens, credentials in URLs) replaced by [redacted].",
        procs,
    );
    registry.add_fn(
        "sys.kill",
        Effect::Destroy,
        "Stop a process: ask it to exit, and force it after a short grace if it has not. Pass the `start_time` sys.procs reported so a process that has since exited and whose id was reused is never hit. `tree` stops its descendants too; `force` skips asking.",
        kill,
    );
}

/// How long apart the two CPU samples are taken. `sysinfo` needs at least 200 ms on some
/// platforms before a difference means anything.
const CPU_SAMPLE_INTERVAL: Duration = Duration::from_millis(250);

/// How long a process asked politely to exit is given before it is forced.
const KILL_GRACE: Duration = Duration::from_secs(3);

/// What `sys.info` returns.
#[derive(Debug, Serialize)]
pub struct Info {
    /// The node's name.
    pub node: String,
    /// The host name the operating system reports.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// The operating system.
    pub os: Os,
    /// Seconds since the machine booted.
    pub uptime_s: u64,
    /// An identifier that changes on every boot, so a reboot can be told apart from a dropped link.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
    /// The processor.
    pub cpu: Cpu,
    /// Memory and swap, in bytes.
    pub memory: Memory,
    /// Mounted file systems that hold data, each once.
    pub disks: Vec<Disk>,
    /// NVIDIA GPUs; empty where `nvidia-smi` is absent or does not answer.
    pub gpus: Vec<Gpu>,
}

/// The operating system part of [`Info`].
#[derive(Debug, Serialize)]
pub struct Os {
    /// `linux`, `macos` or `windows`.
    pub family: &'static str,
    /// The system's own name for itself, such as a Linux distribution's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Its version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The kernel's version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kernel: Option<String>,
    /// The CPU architecture, such as `x86_64` or `aarch64`.
    pub arch: &'static str,
}

/// The processor part of [`Info`].
#[derive(Debug, Serialize)]
pub struct Cpu {
    /// The model name.
    pub model: String,
    /// Logical cores.
    pub cores: usize,
    /// Usage across all cores over a quarter of a second, in percent.
    pub usage_pct: f64,
    /// The load average over 1, 5 and 15 minutes; Windows has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load: Option<[f64; 3]>,
}

/// The memory part of [`Info`].
#[derive(Debug, Serialize)]
pub struct Memory {
    /// Installed memory.
    pub total: u64,
    /// Memory in use.
    pub used: u64,
    /// Memory a program could have without anything being swapped out.
    pub available: u64,
    /// Swap space.
    pub swap_total: u64,
    /// Swap in use.
    pub swap_used: u64,
}

async fn info(ctx: &Ctx, _args: NoArgs) -> Result<Info, ToolError> {
    // nvidia-smi can take a second to wake a sleeping GPU; it runs while the CPU is sampled.
    let gpus = tokio::spawn(gpu::query());
    let mut system = blocking(|| {
        let mut system = System::new_with_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::nothing().with_usage())
                .with_memory(MemoryRefreshKind::everything()),
        )
        .map_err(|error| failed("read the processor and memory figures", &error))?;
        system.refresh_cpu_usage();
        Ok(system)
    })
    .await?;
    tokio::time::sleep(CPU_SAMPLE_INTERVAL).await;
    let windows = ctx.os() == "windows";
    let (cpu, memory, disks, os, hostname, uptime_s, boot_id) = blocking(move || {
        system.refresh_cpu_usage();
        let model = system
            .cpus()
            .first()
            .map(|cpu| cpu.brand().trim().to_owned())
            .unwrap_or_default();
        let load = if windows {
            None
        } else {
            System::load_average()
                .ok()
                .map(|load| [round2(load.one), round2(load.five), round2(load.fifteen)])
        };
        let cpu = Cpu {
            model,
            cores: system.cpus().len(),
            usage_pct: round1(f64::from(system.global_cpu_usage())),
            load,
        };
        let memory = Memory {
            total: system.total_memory(),
            used: system.used_memory(),
            available: system.available_memory(),
            swap_total: system.total_swap(),
            swap_used: system.used_swap(),
        };
        let disks = Disks::new_with_refreshed_list()
            .map(|list| disks::summarise(list.iter().map(disks::Mounted::from)))
            .unwrap_or_default();
        let os = (
            System::name().ok(),
            System::os_version().ok(),
            System::kernel_version().ok(),
        );
        let hostname = System::host_name().ok();
        let uptime_s = System::uptime().unwrap_or_default();
        let boot_id = procid::boot_id().ok();
        Ok((cpu, memory, disks, os, hostname, uptime_s, boot_id))
    })
    .await?;
    let gpus = gpus.await.unwrap_or_default();
    let (name, version, kernel) = os;
    Ok(Info {
        node: ctx.node_name().to_owned(),
        hostname,
        os: Os {
            family: ctx.os(),
            name,
            version,
            kernel,
            arch: ctx.arch(),
        },
        uptime_s,
        boot_id,
        cpu,
        memory,
        disks,
        gpus,
    })
}

/// Arguments of `sys.procs`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ProcsArgs {
    /// Keep only processes whose name or command line contains this, ignoring case.
    #[serde(default)]
    filter: Option<String>,
    /// What the busiest are measured by: `cpu` (the default) or `memory`.
    #[serde(default)]
    sort: SortBy,
    /// How many processes to return, 1 to 500; 30 when absent.
    #[serde(default)]
    limit: Option<u32>,
}

/// What `sys.procs` sorts by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum SortBy {
    /// CPU usage, highest first.
    #[default]
    Cpu,
    /// Memory, largest first.
    Memory,
}

/// What `sys.procs` returns.
#[derive(Debug, Serialize)]
pub struct Procs {
    /// The processes, busiest first.
    pub procs: Vec<Proc>,
    /// How many processes matched, of which `procs` holds the first `limit`.
    pub total: usize,
    /// Whether more matched than `procs` holds.
    pub truncated: bool,
}

/// One process in [`Procs`].
#[derive(Clone, Debug, Serialize)]
pub struct Proc {
    /// Its process id.
    pub pid: u32,
    /// Its parent's, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ppid: Option<u32>,
    /// The program's name.
    pub name: String,
    /// The command line with secret-looking values replaced by `[redacted]`, cut at 300
    /// characters; empty where the system does not say.
    pub cmd: String,
    /// The account it runs as, when that can be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// CPU usage over a quarter of a second, in percent of one core, so a busy process on several
    /// cores can pass 100.
    pub cpu_pct: f64,
    /// Resident memory, in bytes.
    pub memory: u64,
    /// When it started, to the second, when the system says. `sys.kill` takes it back to be sure
    /// of the process.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_time: Option<String>,
}

/// The default and the largest `limit` of `sys.procs`.
const PROCS_LIMIT: (u32, u32) = (30, 500);
/// The longest command line `sys.procs` returns, in characters.
const CMD_CHARS: usize = 300;

async fn procs(_ctx: &Ctx, args: ProcsArgs) -> Result<Procs, ToolError> {
    let limit = args.limit.unwrap_or(PROCS_LIMIT.0);
    if limit == 0 || limit > PROCS_LIMIT.1 {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "sys.procs: `limit` must be between 1 and {}, not {limit}.",
                PROCS_LIMIT.1
            ),
        )
        .with_hint("Narrow the list with `filter` instead of asking for more."));
    }
    let refresh = ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_cmd(UpdateKind::OnlyIfNotSet)
        .with_user(UpdateKind::OnlyIfNotSet)
        .without_tasks();
    let mut system = blocking(move || {
        let mut system = System::new().map_err(|error| failed("list the processes", &error))?;
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        Ok(system)
    })
    .await?;
    tokio::time::sleep(CPU_SAMPLE_INTERVAL).await;
    let filter = args.filter.map(|filter| filter.to_lowercase());
    let sort = args.sort;
    let mut list = blocking(move || {
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        let users = Users::new_with_refreshed_list().ok();
        let names: HashMap<String, String> = users
            .iter()
            .flat_map(|users| users.iter())
            .map(|user| (user.id().to_string(), user.name().to_owned()))
            .collect();
        let list: Vec<Proc> = system
            .processes()
            .values()
            .filter(|process| process.thread_kind() != Some(ThreadKind::Userland))
            .map(|process| {
                // Redacted before the filter sees it, so a filter cannot probe for a secret.
                let words: Vec<String> = process
                    .cmd()
                    .iter()
                    .map(|part| part.to_string_lossy().into_owned())
                    .collect();
                let cmd = audit::redact_command(&words).join(" ");
                Proc {
                    pid: process.pid().as_u32(),
                    ppid: process.parent().map(Pid::as_u32),
                    name: process.name().to_string_lossy().into_owned(),
                    cmd,
                    user: process
                        .user_id()
                        .and_then(|uid| names.get(&uid.to_string()).cloned()),
                    cpu_pct: round1(f64::from(process.cpu_usage())),
                    memory: process.memory(),
                    start_time: rfc3339(process.start_time()),
                }
            })
            .filter(|proc| filter.as_deref().is_none_or(|filter| matches(proc, filter)))
            .collect();
        Ok(list)
    })
    .await?;
    sort_procs(&mut list, sort);
    let total = list.len();
    let keep = usize::try_from(limit).unwrap_or(usize::MAX);
    list.truncate(keep);
    for proc in &mut list {
        proc.cmd = cut(&proc.cmd, CMD_CHARS);
    }
    Ok(Procs {
        truncated: total > list.len(),
        procs: list,
        total,
    })
}

/// Whether a process's name or full command line contains `filter`, which is already lower case.
fn matches(proc: &Proc, filter: &str) -> bool {
    proc.name.to_lowercase().contains(filter) || proc.cmd.to_lowercase().contains(filter)
}

/// Busiest first; ties go to the lower process id so the order is stable between calls.
fn sort_procs(list: &mut [Proc], sort: SortBy) {
    list.sort_by(|a, b| {
        let busier = match sort {
            SortBy::Cpu => b
                .cpu_pct
                .total_cmp(&a.cpu_pct)
                .then(b.memory.cmp(&a.memory)),
            SortBy::Memory => b
                .memory
                .cmp(&a.memory)
                .then(b.cpu_pct.total_cmp(&a.cpu_pct)),
        };
        busier.then(a.pid.cmp(&b.pid))
    });
}

/// The first `max` characters of `text`, with an ellipsis when something was cut.
fn cut(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

/// Arguments of `sys.kill`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct KillArgs {
    /// The process id.
    pid: u32,
    /// When the process started, as sys.procs reported it. When given and the process with this
    /// id started at another time, the id has been reused and nothing is stopped.
    #[serde(default)]
    start_time: Option<String>,
    /// Stop the process's descendants as well.
    #[serde(default)]
    tree: bool,
    /// Stop it at once instead of asking it to exit first.
    #[serde(default)]
    force: bool,
}

/// What `sys.kill` returns.
#[derive(Debug, Serialize)]
pub struct Killed {
    /// The process that was asked to stop.
    pub pid: u32,
    /// Whether it, and with `tree` every descendant, is gone.
    pub stopped: bool,
    /// The processes still running afterwards, which the system refused to stop.
    pub survivors: Vec<u32>,
}

async fn kill(_ctx: &Ctx, args: KillArgs) -> Result<Killed, ToolError> {
    let pid = args.pid;
    // What can be refused from the pid alone is, before anything is looked up: a process another
    // account owns may not even let its identity be read.
    if let Some(reason) = protected_by_pid(pid, std::process::id(), std::env::consts::OS) {
        return Err(protected(pid, Some(reason)));
    }
    let expected = args
        .start_time
        .as_deref()
        .map(|text| {
            parse_rfc3339(text).ok_or_else(|| {
                ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("sys.kill: `start_time` {text:?} is not an RFC 3339 time."),
                )
                .with_hint("Pass the start_time sys.procs reported for the process, unchanged.")
            })
        })
        .transpose()?;

    let outcome = blocking(move || {
        let lookup = identify(pid)?;
        let probe = match lookup {
            Lookup::Missing => probe::probe(pid),
            Lookup::Found(..) | Lookup::NotPermitted => probe::Probe::Unknown,
        };
        let outcome = decide(pid, std::process::id(), std::env::consts::OS, lookup, probe);
        // The rules that need the identity: the processes legion runs under, and those the system
        // marks as its own.
        Ok(match outcome {
            Outcome::Found(identity, started) => match procid::is_protected(&identity) {
                Some(reason) => Outcome::Protected(reason),
                None => Outcome::Found(identity, started),
            },
            other => other,
        })
    })
    .await?;
    let (identity, started) = match outcome {
        Outcome::Found(identity, started) => (identity, started),
        Outcome::Protected(reason) => return Err(protected(pid, Some(reason))),
        Outcome::NotPermitted => {
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                format!("Process {pid} belongs to another user or to the system, which does not let legion look at it, so it was not stopped."),
            )
            .with_hint("Stop it as the account it runs as, or as an administrator.")
            .with_details(json!({"pid": pid})));
        }
        Outcome::NotFound => {
            return Err(ErrorBody::new(
                ErrorCode::NotFound,
                format!("There is no process {pid} on this machine."),
            )
            .with_hint("It may have exited already; sys.procs lists the processes running now."));
        }
    };
    if let Some(expected) = expected {
        let Some(started) = started else {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("The system does not say when process {pid} started, so it cannot be checked against the start_time given, and nothing was stopped."),
            )
            .with_hint("Call sys.kill without start_time if you are sure of the process.")
            .with_details(json!({"pid": pid})));
        };
        if expected != started {
            let mut details = json!({"pid": pid});
            if let Some(time) = rfc3339(started) {
                details["start_time"] = json!(time);
            }
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                format!(
                    "Process {pid} started at {}, not at {}: the process that had this id has exited and the id was reused, so nothing was stopped.",
                    rfc3339(started).unwrap_or_default(),
                    rfc3339(expected).unwrap_or_default(),
                ),
            )
            .with_hint("Look the process up again with sys.procs before stopping it.")
            .with_details(details));
        }
    }

    let grace = if args.force {
        Duration::ZERO
    } else {
        KILL_GRACE
    };
    // The identity read above goes all the way down: legion-os checks it again at the moment it
    // signals, so a process that exits meanwhile and whose id is reused is never hit.
    let survivors = if args.tree {
        let report = procid::kill_tree(&identity, grace).await.map_err(|error| {
            os_error(
                pid,
                &format!("stop process {pid} and its descendants"),
                error,
            )
        })?;
        report
            .survivors
            .iter()
            .map(|survivor| survivor.pid)
            .collect()
    } else {
        let gone = procid::kill_process(&identity, args.force, grace)
            .await
            .map_err(|error| os_error(pid, &format!("stop process {pid}"), error))?;
        if gone { Vec::new() } else { vec![pid] }
    };
    Ok(Killed {
        pid,
        stopped: survivors.is_empty(),
        survivors,
    })
}

/// Why sys.kill refuses a process legion-os names as protected: legion itself, the processes it
/// runs under, and those the operating system cannot do without.
fn protected(pid: u32, reason: Option<&str>) -> ErrorBody {
    let reason = reason.unwrap_or("protected");
    ErrorBody::new(
        ErrorCode::Denied,
        format!("sys.kill does not stop process {pid}, which is protected: {reason}."),
    )
    .with_hint("Legion never stops itself, the processes it runs under, or the system's own; a process that must go anyway has to be stopped outside legion.")
    .with_details(json!({"pid": pid, "reason": reason}))
}

/// What reading a process's identity found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lookup {
    /// The process, and when it started in seconds since the epoch where the system says.
    Found(ProcessId, Option<u64>),
    /// Nothing could be read, which is either no process or one that hides itself.
    Missing,
    /// The system refused to say anything about it.
    NotPermitted,
}

/// What sys.kill does about a pid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    /// Stops this process, once the rules that need its identity have passed.
    Found(ProcessId, Option<u64>),
    /// Refuses: the process is protected, for this reason.
    Protected(&'static str),
    /// Refuses: the process exists but belongs to another user or the system.
    NotPermitted,
    /// There is no such process.
    NotFound,
}

/// The processes refused from the pid alone: the kernel's (0), the first process on Unix (1),
/// the System process on Windows (4), and legion itself.
fn protected_by_pid(pid: u32, own: u32, os: &str) -> Option<&'static str> {
    match pid {
        0 => Some("kernel / idle process (pid 0)"),
        1 if os != "windows" => Some("init process (pid 1)"),
        4 if os == "windows" => Some("Windows System process (pid 4)"),
        _ if pid == own => Some("current process"),
        _ => None,
    }
}

/// What sys.kill does, given the pid, what reading its identity found, and what asking the system
/// about the pid said. A process whose identity cannot be read is someone else's when the system
/// says a process is there, and missing only when it says there is none.
fn decide(pid: u32, own: u32, os: &str, lookup: Lookup, probe: probe::Probe) -> Outcome {
    if let Some(reason) = protected_by_pid(pid, own, os) {
        return Outcome::Protected(reason);
    }
    match (lookup, probe) {
        (Lookup::Found(identity, started), _) => Outcome::Found(identity, started),
        (Lookup::NotPermitted, _)
        | (Lookup::Missing, probe::Probe::Exists | probe::Probe::NotPermitted) => {
            Outcome::NotPermitted
        }
        (Lookup::Missing, probe::Probe::Missing | probe::Probe::Unknown) => Outcome::NotFound,
    }
}

/// The identity of a live process and when it started.
///
/// The seconds are what `sys.procs` reports and a caller passes back; the identity is what
/// legion-os stops by. Both are read for the same process: the identity is read before and after
/// the start time, and a process that changed in between counts as gone.
fn identify(pid: u32) -> Result<Lookup, ErrorBody> {
    let read = || match procid::of_pid(pid) {
        Err(OsError::PermissionDenied { .. }) => Ok(Err(Lookup::NotPermitted)),
        Err(error) => Err(os_error(pid, &format!("read process {pid}"), error)),
        Ok(None) => Ok(Err(Lookup::Missing)),
        Ok(Some(identity)) => Ok(Ok(identity)),
    };
    let identity = match read()? {
        Ok(identity) => identity,
        Err(lookup) => return Ok(lookup),
    };
    let mut system = System::new().map_err(|error| failed("look the process up", &error))?;
    let target = Pid::from_u32(pid);
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[target]),
        true,
        ProcessRefreshKind::nothing(),
    );
    let started = system
        .process(target)
        .map(sysinfo::Process::start_time)
        .filter(|&seconds| seconds > 0);
    match read()? {
        Ok(again) if again.matches(&identity) => Ok(Lookup::Found(identity, started)),
        _ => Ok(Lookup::Missing),
    }
}

/// What a failure to read or stop a process means for the caller. The system refusing for lack
/// of permission is `denied`, whichever way the process was being stopped, and names the process.
fn os_error(pid: u32, what: &str, error: OsError) -> ErrorBody {
    match error {
        OsError::PermissionDenied { .. } => ErrorBody::new(
            ErrorCode::Denied,
            format!("The system does not let legion {what}: it belongs to another account, or the system protects it."),
        )
        .with_hint("Stop it as the account it runs as, or as an administrator.")
        .with_details(json!({"pid": pid})),
        OsError::Unsupported { detail } => ErrorBody::new(
            ErrorCode::Unsupported,
            format!("This system cannot {what}: {detail}."),
        )
        .with_details(json!({"pid": pid})),
        other => failed(what, &other),
    }
}

/// Seconds since the epoch as RFC 3339 UTC. Zero is what the system says when it cannot tell, and
/// is no time at all.
fn rfc3339(seconds: u64) -> Option<String> {
    if seconds == 0 {
        return None;
    }
    let seconds = i64::try_from(seconds).ok()?;
    jiff::Timestamp::from_second(seconds)
        .ok()
        .map(|time| time.to_string())
}

/// An RFC 3339 time as whole seconds since the epoch, the resolution `sys.procs` reports.
fn parse_rfc3339(text: &str) -> Option<u64> {
    let time: jiff::Timestamp = text.trim().parse().ok()?;
    u64::try_from(time.as_second()).ok()
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// Runs a blocking read on the blocking pool.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ErrorBody> + Send + 'static,
) -> Result<T, ErrorBody> {
    tokio::task::spawn_blocking(work).await.map_err(|error| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Reading the system's figures stopped unexpectedly: {error}."),
        )
    })?
}

fn failed(what: &str, error: &dyn std::fmt::Display) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Failed,
        format!("Legion could not {what}: {error}."),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, cpu_pct: f64, memory: u64) -> Proc {
        Proc {
            pid,
            ppid: None,
            name: format!("p{pid}"),
            cmd: String::new(),
            user: None,
            cpu_pct,
            memory,
            start_time: rfc3339(0),
        }
    }

    #[test]
    fn processes_are_sorted_busiest_first_and_ties_by_pid() {
        let mut list = vec![proc(3, 1.0, 10), proc(1, 5.0, 5), proc(2, 1.0, 10)];
        sort_procs(&mut list, SortBy::Cpu);
        let pids: Vec<u32> = list.iter().map(|p| p.pid).collect();
        assert_eq!(pids, [1, 2, 3]);
        sort_procs(&mut list, SortBy::Memory);
        let pids: Vec<u32> = list.iter().map(|p| p.pid).collect();
        assert_eq!(pids, [2, 3, 1]);
    }

    #[test]
    fn a_filter_matches_name_or_command_line_ignoring_case() {
        let mut p = proc(7, 0.0, 0);
        p.name = "Code".to_owned();
        p.cmd = "/usr/lib/code --type=GPU".to_owned();
        assert!(matches(&p, "code"));
        assert!(matches(&p, "gpu"));
        assert!(!matches(&p, "firefox"));
    }

    #[test]
    fn a_long_command_line_is_cut_on_a_character_boundary() {
        assert_eq!(cut("short", 300), "short");
        let long = "é".repeat(301);
        let cut = cut(&long, 300);
        assert_eq!(cut.chars().count(), 301);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn protection_and_ownership_are_decided_before_absence() {
        use probe::Probe;
        let own = 4242;
        let found = Lookup::Found(ProcessId::new(77, 1), Some(1));
        // macOS hides root's launchd from other users: pid 1 reads as missing, and is still
        // protected, not absent.
        for os in ["macos", "linux"] {
            assert_eq!(
                decide(1, own, os, Lookup::Missing, Probe::NotPermitted),
                Outcome::Protected("init process (pid 1)")
            );
        }
        assert_eq!(
            decide(4, own, "windows", Lookup::NotPermitted, Probe::Unknown),
            Outcome::Protected("Windows System process (pid 4)")
        );
        assert!(matches!(
            decide(0, own, "linux", Lookup::Missing, Probe::Missing),
            Outcome::Protected(_)
        ));
        assert!(matches!(
            decide(own, own, "macos", found, Probe::Exists),
            Outcome::Protected(_)
        ));
        // Another user's process whose identity cannot be read is theirs, never missing.
        assert_eq!(
            decide(500, own, "macos", Lookup::Missing, Probe::NotPermitted),
            Outcome::NotPermitted
        );
        assert_eq!(
            decide(500, own, "macos", Lookup::Missing, Probe::Exists),
            Outcome::NotPermitted
        );
        assert_eq!(
            decide(500, own, "windows", Lookup::NotPermitted, Probe::Unknown),
            Outcome::NotPermitted
        );
        assert_eq!(
            decide(500, own, "linux", Lookup::Missing, Probe::Missing),
            Outcome::NotFound
        );
        assert_eq!(
            decide(77, own, "linux", found, Probe::Unknown),
            Outcome::Found(ProcessId::new(77, 1), Some(1))
        );
        // Pid 1 and 4 are ordinary on the other family.
        assert_eq!(
            decide(1, own, "windows", Lookup::Missing, Probe::Missing),
            Outcome::NotFound
        );
        assert_eq!(
            decide(4, own, "linux", Lookup::Missing, Probe::Missing),
            Outcome::NotFound
        );
    }

    #[test]
    fn refusals_by_the_system_are_denied_and_missing_abilities_unsupported() {
        let denied = os_error(
            42,
            "stop process 42",
            OsError::PermissionDenied {
                detail: "kill".into(),
                source: None,
            },
        );
        assert_eq!(denied.code, ErrorCode::Denied);
        assert_eq!(denied.details, Some(json!({"pid": 42})));
        let unsupported = os_error(
            42,
            "stop process 42",
            OsError::Unsupported {
                detail: "no signals".into(),
            },
        );
        assert_eq!(unsupported.code, ErrorCode::Unsupported);
    }

    #[test]
    fn start_times_round_trip_through_rfc_3339() {
        assert_eq!(
            rfc3339(1_700_000_000).as_deref(),
            Some("2023-11-14T22:13:20Z")
        );
        assert_eq!(rfc3339(0), None);
        assert_eq!(parse_rfc3339("2023-11-14T22:13:20Z"), Some(1_700_000_000));
        assert_eq!(
            parse_rfc3339("2023-11-14T23:13:20+01:00"),
            Some(1_700_000_000)
        );
        assert_eq!(parse_rfc3339("yesterday"), None);
    }
}
