//! `sys`: what the machine is and how busy it is, the processes it runs, and stopping them.
//!
//! System and process figures come from `sysinfo` and `legion_os::hardware`, whose reads block on
//! the file system or the kernel, so every read runs on the blocking pool or in a bounded probe
//! task and the async runtime keeps serving other calls. CPU usage is a rate, which takes two
//! samples a short interval apart; hardware probes run concurrently during that interval so
//! `sys.info` and `sys.procs` stay as fast as their slowest single probe.
//!
//! One rule for every result of the `sys` and `exec` families: a field that may have no value is
//! left out when it has none, never sent as `null`, so a caller tests for presence and never for
//! both.

mod disks;
mod probe;

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use legion_core::proto::{Effect, ErrorBody, ErrorCode};
use legion_core::{Ctx, NoArgs, Registry, ToolError, audit};
use legion_os::OsError;
use legion_os::hardware::{self, Battery, Gpu};
use legion_os::procid::{self, ProcessId, SignalOutcome};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sysinfo::{
    CpuRefreshKind, Disks, MemoryRefreshKind, Pid, ProcessRefreshKind, ProcessesToUpdate,
    RefreshKind, System, ThreadKind, UpdateKind, Users,
};

pub use disks::Disk;
pub use legion_os::hardware::{Battery as SysBattery, BatteryState, Gpu as SysGpu};

/// Adds the `sys` tools.
pub fn register(registry: &mut Registry) {
    registry.add_fn(
        "sys.info",
        Effect::Read,
        "Describe this machine: host name, operating system, uptime and boot id, CPU model, core count, usage and temperature, memory and swap, disks, graphics cards, and battery. Sizes are bytes.",
        info,
    );
    registry.add_fn(
        "sys.procs",
        Effect::Read,
        "List the processes running on this machine, busiest first, with their id, parent, name, command line, user, CPU usage (percent of one core), memory and graphics memory in bytes, start time, and whether the process is legion itself or an ancestor. `group` combines processes with the same name and user into one row. `filter` keeps those whose name or command line contains it. Command lines are shown with secret-looking values (passwords, tokens, credentials in URLs) replaced by [redacted].",
        procs,
    );
    registry.add_fn(
        "sys.kill",
        Effect::Destroy,
        "Stop one process (`pid`) or up to 64 verified `{pid, start_time}` processes (`targets`). With `targets`, without `force` the tool asks them to exit and waits up to five seconds, returning per-target outcomes (`stopped`, `gone`, `reused`, `still_running`, or `refused`). With `pid`, pass `start_time` so a reused id is never hit; `tree` stops descendants too; `force` skips asking.",
        kill,
    );
}

/// How long apart the two CPU samples are taken. `sysinfo` needs at least 200 ms on some
/// platforms before a difference means anything.
const CPU_SAMPLE_INTERVAL: Duration = Duration::from_millis(250);

/// How long a single `pid` call waits before escalating to force.
const KILL_GRACE: Duration = Duration::from_secs(3);

/// How long a `targets` call without `force` waits for processes to exit before reporting
/// survivors as `still_running`.
const TARGETS_WAIT: Duration = Duration::from_secs(5);

/// How long a `targets` call with `force` waits for signaled processes to leave the process table.
const TARGETS_FORCE_SETTLE: Duration = Duration::from_secs(2);

/// Poll interval while waiting for `targets` to exit.
const TARGETS_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Maximum number of targets accepted by `sys.kill` `targets` and members listed per grouped row.
const MAX_TARGETS: usize = 64;

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
    /// Graphics cards reported by `nvidia-smi` or Linux `/sys/class/drm`.
    pub gpus: Vec<Gpu>,
    /// System battery state, absent on machines without a battery.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery: Option<Battery>,
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
    /// Processor package temperature in degrees Celsius, when reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature_c: Option<f64>,
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
    // Hardware probes (CPU temp, NVIDIA GPUs, DRM GPUs, battery) each have a 1s limit and run
    // concurrently side by side while the CPU usage interval elapses.
    let hw = tokio::spawn(hardware::probe_all(hardware::ProbeConfig::default()));
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
    let (mut cpu, memory, disks, os, hostname, uptime_s, boot_id) = blocking(move || {
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
            temperature_c: None,
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
    let hw = hw.await.unwrap_or_default();
    cpu.temperature_c = hw.cpu_temperature_c;
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
        gpus: hw.gpus,
        battery: hw.battery,
    })
}

/// Arguments of `sys.procs`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ProcsArgs {
    /// Keep only processes whose name or command line contains this, ignoring case.
    #[serde(default)]
    filter: Option<String>,
    /// What the busiest are measured by: `cpu` (the default), `memory`, or `gpu_memory`.
    #[serde(default)]
    sort: SortBy,
    /// How many rows to return, 1 to 500; 30 when absent.
    #[serde(default)]
    limit: Option<u32>,
    /// Combine processes with the same name and user into one row per program.
    #[serde(default)]
    group: bool,
}

/// What `sys.procs` sorts by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SortBy {
    /// CPU usage, highest first.
    #[default]
    Cpu,
    /// Memory, largest first.
    Memory,
    /// Graphics memory, largest first.
    GpuMemory,
}

/// What `sys.procs` returns.
#[derive(Debug, Serialize)]
pub struct Procs {
    /// The processes (or grouped programs when `group` is true), busiest first.
    pub procs: Vec<ProcRow>,
    /// How many rows matched, of which `procs` holds the first `limit`.
    pub total: usize,
    /// Whether more matched than `procs` holds.
    pub truncated: bool,
}

/// One row in [`Procs`]: either an individual process or a grouped program row.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ProcRow {
    /// One individual process (`group: false`).
    Proc(Proc),
    /// One grouped program (`group: true`).
    Group(ProcGroup),
}

/// One process in [`Procs`].
#[derive(Clone, Debug, PartialEq, Serialize)]
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
    /// Graphics memory in bytes, when reported by the graphics driver.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_memory: Option<u64>,
    /// When it started, to the second, when the system says. `sys.kill` takes it back to be sure
    /// of the process.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_time: Option<String>,
    /// Present and `true` on the node's own process and its ancestors.
    #[serde(rename = "self", skip_serializing_if = "is_false")]
    pub is_self: bool,
}

/// One `{pid, start_time}` member inside a [`ProcGroup`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProcMember {
    /// Process id.
    pub pid: u32,
    /// When the process started, to the second, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_time: Option<String>,
}

/// One grouped program row returned by `sys.procs` when `group: true`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProcGroup {
    /// The program's name.
    pub name: String,
    /// The account it runs as, when that can be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Number of processes in this group.
    pub count: usize,
    /// Sum of CPU usage across the group, in percent of one core.
    pub cpu_pct: f64,
    /// Sum of resident memory across the group, in bytes.
    pub memory: u64,
    /// Sum of graphics memory across the group, in bytes, when any member reports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_memory: Option<u64>,
    /// Redacted command line of the busiest member, when non-empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cmd: Option<String>,
    /// Present and `true` when any member is the node's own process or an ancestor.
    #[serde(rename = "self", skip_serializing_if = "is_false")]
    pub is_self: bool,
    /// Up to 64 `{pid, start_time}` pairs, busiest first.
    pub members: Vec<ProcMember>,
    /// How many members were omitted past the 64-member cap.
    pub more: usize,
}

fn is_false(value: &bool) -> bool {
    !*value
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
    let gpu_mem_task = tokio::spawn(hardware::probe_gpu_process_memory(
        hardware::default_nvidia_compute_procs_command(),
        hardware::default_nvidia_pmon_procs_command(),
        hardware::DEFAULT_PROBE_TIMEOUT,
    ));
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
    let gpu_mem = gpu_mem_task.await.unwrap_or_default();
    let filter = args.filter.map(|filter| filter.to_lowercase());
    let sort = args.sort;
    let mut list = blocking(move || {
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        let self_pids = procid::self_and_ancestors();
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
                let pid = process.pid().as_u32();
                Proc {
                    pid,
                    ppid: process.parent().map(Pid::as_u32),
                    name: process.name().to_string_lossy().into_owned(),
                    cmd,
                    user: process
                        .user_id()
                        .and_then(|uid| names.get(&uid.to_string()).cloned()),
                    cpu_pct: round1(f64::from(process.cpu_usage())),
                    memory: process.memory(),
                    gpu_memory: gpu_mem.get(&pid).copied(),
                    start_time: rfc3339(process.start_time()),
                    is_self: self_pids.contains(&pid),
                }
            })
            .filter(|proc| filter.as_deref().is_none_or(|filter| matches(proc, filter)))
            .collect();
        Ok(list)
    })
    .await?;

    let keep = usize::try_from(limit).unwrap_or(usize::MAX);
    if args.group {
        let mut groups = group_procs(list, sort);
        let total = groups.len();
        groups.truncate(keep);
        return Ok(Procs {
            truncated: total > groups.len(),
            procs: groups.into_iter().map(ProcRow::Group).collect(),
            total,
        });
    }

    sort_procs(&mut list, sort);
    let total = list.len();
    list.truncate(keep);
    for proc in &mut list {
        proc.cmd = cut(&proc.cmd, CMD_CHARS);
    }
    Ok(Procs {
        truncated: total > list.len(),
        procs: list.into_iter().map(ProcRow::Proc).collect(),
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
                .then(b.memory.cmp(&a.memory))
                .then(b.gpu_memory.unwrap_or(0).cmp(&a.gpu_memory.unwrap_or(0))),
            SortBy::Memory => b
                .memory
                .cmp(&a.memory)
                .then(b.cpu_pct.total_cmp(&a.cpu_pct))
                .then(b.gpu_memory.unwrap_or(0).cmp(&a.gpu_memory.unwrap_or(0))),
            SortBy::GpuMemory => b
                .gpu_memory
                .unwrap_or(0)
                .cmp(&a.gpu_memory.unwrap_or(0))
                .then(b.cpu_pct.total_cmp(&a.cpu_pct))
                .then(b.memory.cmp(&a.memory)),
        };
        busier.then(a.pid.cmp(&b.pid))
    });
}

/// Combine processes with the same `(name, user)` into one [`ProcGroup`] row per program.
fn group_procs(procs: Vec<Proc>, sort: SortBy) -> Vec<ProcGroup> {
    let mut buckets: BTreeMap<(String, Option<String>), Vec<Proc>> = BTreeMap::new();
    for proc in procs {
        buckets
            .entry((proc.name.clone(), proc.user.clone()))
            .or_default()
            .push(proc);
    }

    let mut groups: Vec<ProcGroup> = buckets
        .into_iter()
        .map(|((name, user), mut items)| {
            sort_procs(&mut items, sort);
            let count = items.len();
            let cpu_pct = round1(items.iter().map(|p| p.cpu_pct).sum());
            let memory = items
                .iter()
                .fold(0u64, |acc, p| acc.saturating_add(p.memory));
            let mut gpu_sum: Option<u64> = None;
            for p in &items {
                if let Some(g) = p.gpu_memory {
                    gpu_sum = Some(gpu_sum.unwrap_or(0).saturating_add(g));
                }
            }
            let is_self = items.iter().any(|p| p.is_self);
            let cmd = items
                .first()
                .map(|p| p.cmd.as_str())
                .filter(|c| !c.is_empty())
                .map(|c| cut(c, CMD_CHARS));
            let more = count.saturating_sub(MAX_TARGETS);
            let members = items
                .into_iter()
                .take(MAX_TARGETS)
                .map(|p| ProcMember {
                    pid: p.pid,
                    start_time: p.start_time,
                })
                .collect();
            ProcGroup {
                name,
                user,
                count,
                cpu_pct,
                memory,
                gpu_memory: gpu_sum,
                cmd,
                is_self,
                members,
                more,
            }
        })
        .collect();

    groups.sort_by(|a, b| {
        let busier = match sort {
            SortBy::Cpu => b
                .cpu_pct
                .total_cmp(&a.cpu_pct)
                .then(b.memory.cmp(&a.memory))
                .then(b.gpu_memory.unwrap_or(0).cmp(&a.gpu_memory.unwrap_or(0))),
            SortBy::Memory => b
                .memory
                .cmp(&a.memory)
                .then(b.cpu_pct.total_cmp(&a.cpu_pct))
                .then(b.gpu_memory.unwrap_or(0).cmp(&a.gpu_memory.unwrap_or(0))),
            SortBy::GpuMemory => b
                .gpu_memory
                .unwrap_or(0)
                .cmp(&a.gpu_memory.unwrap_or(0))
                .then(b.cpu_pct.total_cmp(&a.cpu_pct))
                .then(b.memory.cmp(&a.memory)),
        };
        busier.then(a.name.cmp(&b.name)).then(a.user.cmp(&b.user))
    });
    groups
}

/// The first `max` characters of `text`, with an ellipsis when something was cut.
fn cut(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

/// One verified `{pid, start_time}` target for `sys.kill` `targets`.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct KillTarget {
    /// The process id.
    pid: u32,
    /// When the process started, as `sys.procs` reported it. Required so a reused id is never
    /// stopped by mistake.
    start_time: String,
}

/// Arguments of `sys.kill`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct KillArgs {
    /// The process id (alternative to `targets`).
    #[serde(default)]
    pid: Option<u32>,
    /// When the process started, as sys.procs reported it (used with `pid`). When given and the
    /// process with this id started at another time, the id has been reused and nothing is stopped.
    #[serde(default)]
    start_time: Option<String>,
    /// 1 to 64 `{pid, start_time}` pairs to stop together with per-target results (alternative to
    /// `pid`).
    #[serde(default)]
    targets: Option<Vec<KillTarget>>,
    /// Stop the process's descendants as well (only valid with `pid`).
    #[serde(default)]
    tree: bool,
    /// Stop at once instead of asking to exit first.
    #[serde(default)]
    force: bool,
}

/// What `sys.kill` returns when called with `pid`.
#[derive(Debug, Serialize)]
pub struct Killed {
    /// The process that was asked to stop.
    pub pid: u32,
    /// Whether it, and with `tree` every descendant, is gone.
    pub stopped: bool,
    /// The processes still running afterwards, which the system refused to stop.
    pub survivors: Vec<u32>,
}

/// Outcome for one entry in `sys.kill` `targets`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetStatus {
    /// The process exited after being asked (or forced).
    Stopped,
    /// The process had already ended before anything was done.
    Gone,
    /// Another process now has that id; nothing was done.
    Reused,
    /// The process was still running after the wait window.
    StillRunning,
    /// Stopping this process was refused.
    Refused,
}

/// Per-target result returned by `sys.kill` when called with `targets`.
#[derive(Debug, Serialize)]
pub struct TargetOutcome {
    /// The process id that was targeted.
    pub pid: u32,
    /// Outcome for this target.
    pub status: TargetStatus,
    /// Error code when `status` is `refused`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<ErrorCode>,
    /// Human-readable sentence explaining a refusal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// What `sys.kill` returns when called with `targets`.
#[derive(Debug, Serialize)]
pub struct KillTargetsResult {
    /// Per-target outcomes, in the same order as `targets`.
    pub results: Vec<TargetOutcome>,
}

/// Return value of `sys.kill`, preserving the single-`pid` shape and adding the `targets` shape.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum KillResult {
    /// Result of calling `sys.kill` with `pid`.
    Single(Killed),
    /// Result of calling `sys.kill` with `targets`.
    Batch(KillTargetsResult),
}

async fn kill(_ctx: &Ctx, args: KillArgs) -> Result<KillResult, ToolError> {
    match (args.pid, args.targets) {
        (Some(_), Some(_)) => Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "sys.kill: pass either `pid` or `targets`, not both.",
        )
        .with_hint("Use `pid` for one process or `targets` for a verified list.")),
        (None, None) => Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "sys.kill: pass either `pid` or `targets`.",
        )
        .with_hint("Pass a single `pid` or a non-empty `targets` array of `{pid, start_time}`.")),
        (Some(pid), None) => kill_single(pid, args.start_time, args.tree, args.force)
            .await
            .map(KillResult::Single),
        (None, Some(targets)) => {
            if args.start_time.is_some() {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    "sys.kill: `start_time` belongs inside each item of `targets`, not at the top level.",
                ));
            }
            if args.tree {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    "sys.kill: `tree` cannot be combined with `targets`.",
                )
                .with_hint(
                    "List each process to stop in `targets`, or use `pid` with `tree: true`.",
                ));
            }
            kill_targets(targets, args.force)
                .await
                .map(KillResult::Batch)
        }
    }
}

async fn kill_single(
    pid: u32,
    start_time: Option<String>,
    tree: bool,
    force: bool,
) -> Result<Killed, ToolError> {
    // What can be refused from the pid alone is, before anything is looked up: a process another
    // account owns may not even let its identity be read.
    if let Some(reason) = protected_by_pid(pid, std::process::id(), std::env::consts::OS) {
        return Err(protected(pid, Some(reason)));
    }
    let expected = start_time
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

    let grace = if force { Duration::ZERO } else { KILL_GRACE };
    // The identity read above goes all the way down: legion-os checks it again at the moment it
    // signals, so a process that exits meanwhile and whose id is reused is never hit.
    let survivors = if tree {
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
        let gone = procid::kill_process(&identity, force, grace)
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

async fn kill_targets(
    targets: Vec<KillTarget>,
    force: bool,
) -> Result<KillTargetsResult, ToolError> {
    if targets.is_empty() || targets.len() > MAX_TARGETS {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "sys.kill: `targets` must have between 1 and {MAX_TARGETS} entries, not {}.",
                targets.len()
            ),
        )
        .with_hint("Pass 1 to 64 `{pid, start_time}` targets from sys.procs."));
    }

    let mut parsed: Vec<(u32, u64)> = Vec::with_capacity(targets.len());
    for target in &targets {
        let Some(ts) = parse_rfc3339(&target.start_time) else {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "sys.kill: `start_time` {:?} for pid {} is not an RFC 3339 time.",
                    target.start_time, target.pid
                ),
            )
            .with_hint("Pass the start_time sys.procs reported for each target, unchanged."));
        };
        parsed.push((target.pid, ts));
    }

    let own = std::process::id();
    let inspected: Vec<TargetPlan> = blocking(move || {
        let mut plans = Vec::with_capacity(parsed.len());
        for (pid, expected_start) in parsed {
            if let Some(reason) = protected_target_pid(pid, own, std::env::consts::OS) {
                plans.push(TargetPlan::Ready(refused_target(
                    pid,
                    ErrorCode::Denied,
                    plain_protected_sentence(reason),
                )));
                continue;
            }
            let lookup = match identify(pid) {
                Ok(lookup) => lookup,
                Err(err) => {
                    plans.push(TargetPlan::Ready(refused_target(pid, err.code, err.message)));
                    continue;
                }
            };
            let probe = match lookup {
                Lookup::Missing => probe::probe(pid),
                Lookup::Found(..) | Lookup::NotPermitted => probe::Probe::Unknown,
            };
            let outcome = decide(pid, own, std::env::consts::OS, lookup, probe);
            match outcome {
                Outcome::Protected(reason) => {
                    plans.push(TargetPlan::Ready(refused_target(
                        pid,
                        ErrorCode::Denied,
                        plain_protected_sentence(reason),
                    )));
                }
                Outcome::NotPermitted => {
                    plans.push(TargetPlan::Ready(refused_target(
                        pid,
                        ErrorCode::Denied,
                        "This program belongs to another user or to the system, so Legion is not allowed to stop it."
                            .to_owned(),
                    )));
                }
                Outcome::NotFound => {
                    plans.push(TargetPlan::Ready(TargetOutcome {
                        pid,
                        status: TargetStatus::Gone,
                        code: None,
                        message: None,
                    }));
                }
                Outcome::Found(identity, started) => {
                    if let Some(reason) = procid::is_protected(&identity) {
                        plans.push(TargetPlan::Ready(refused_target(
                            pid,
                            ErrorCode::Denied,
                            plain_protected_sentence(reason),
                        )));
                        continue;
                    }
                    match started {
                        None => {
                            plans.push(TargetPlan::Ready(refused_target(
                                pid,
                                ErrorCode::Failed,
                                "The system did not report when this program started, so nothing was stopped."
                                    .to_owned(),
                            )));
                        }
                        Some(actual) if actual != expected_start => {
                            plans.push(TargetPlan::Ready(TargetOutcome {
                                pid,
                                status: TargetStatus::Reused,
                                code: None,
                                message: None,
                            }));
                        }
                        Some(_) => {
                            plans.push(TargetPlan::Signal(identity));
                        }
                    }
                }
            }
        }
        Ok(plans)
    })
    .await?;

    let mut results: Vec<Option<TargetOutcome>> = Vec::with_capacity(inspected.len());
    let mut pending: Vec<(usize, ProcessId)> = Vec::new();

    for (idx, plan) in inspected.into_iter().enumerate() {
        match plan {
            TargetPlan::Ready(outcome) => results.push(Some(outcome)),
            TargetPlan::Signal(identity) => match procid::signal_process(&identity, force).await {
                Ok(SignalOutcome::Signaled) => {
                    results.push(None);
                    pending.push((idx, identity));
                }
                Ok(SignalOutcome::AlreadyGone) => {
                    results.push(Some(TargetOutcome {
                        pid: identity.pid,
                        status: TargetStatus::Gone,
                        code: None,
                        message: None,
                    }));
                }
                Ok(SignalOutcome::IdentityMismatch) => {
                    results.push(Some(TargetOutcome {
                        pid: identity.pid,
                        status: TargetStatus::Reused,
                        code: None,
                        message: None,
                    }));
                }
                Err(OsError::PermissionDenied { .. }) => {
                    results.push(Some(refused_target(
                        identity.pid,
                        ErrorCode::Denied,
                        "This program belongs to another user or to the system, so Legion is not allowed to stop it."
                            .to_owned(),
                    )));
                }
                Err(OsError::Unsupported { detail }) => {
                    results.push(Some(refused_target(
                        identity.pid,
                        ErrorCode::Unsupported,
                        format!("This system cannot stop the program: {detail}."),
                    )));
                }
                Err(other) => {
                    results.push(Some(refused_target(
                        identity.pid,
                        ErrorCode::Failed,
                        format!("Could not stop the program: {other}."),
                    )));
                }
            },
        }
    }

    if !pending.is_empty() {
        let wait = if force {
            TARGETS_FORCE_SETTLE
        } else {
            TARGETS_WAIT
        };
        let deadline = Instant::now() + wait;
        loop {
            let ids: Vec<ProcessId> = pending.iter().map(|(_, id)| *id).collect();
            let alive_flags: Vec<bool> =
                blocking(move || Ok(ids.iter().map(procid::is_alive).collect())).await?;

            let mut still = Vec::new();
            for ((idx, id), alive) in pending.into_iter().zip(alive_flags) {
                if alive {
                    still.push((idx, id));
                } else {
                    results[idx] = Some(TargetOutcome {
                        pid: id.pid,
                        status: TargetStatus::Stopped,
                        code: None,
                        message: None,
                    });
                }
            }
            pending = still;
            if pending.is_empty() {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline.saturating_duration_since(now);
            tokio::time::sleep(TARGETS_POLL_INTERVAL.min(remaining)).await;
        }

        for (idx, id) in pending {
            results[idx] = Some(TargetOutcome {
                pid: id.pid,
                status: TargetStatus::StillRunning,
                code: None,
                message: None,
            });
        }
    }

    let final_results = results.into_iter().flatten().collect();
    Ok(KillTargetsResult {
        results: final_results,
    })
}

enum TargetPlan {
    Ready(TargetOutcome),
    Signal(ProcessId),
}

fn refused_target(pid: u32, code: ErrorCode, message: String) -> TargetOutcome {
    TargetOutcome {
        pid,
        status: TargetStatus::Refused,
        code: Some(code),
        message: Some(message),
    }
}

/// Protected PIDs for `targets`: PID 0 and PID 1 are refused always, alongside the current
/// process and Windows System (PID 4).
fn protected_target_pid(pid: u32, own: u32, os: &str) -> Option<&'static str> {
    if pid == 1 {
        return Some("init process (pid 1)");
    }
    protected_by_pid(pid, own, os)
}

fn plain_protected_sentence(reason: &str) -> String {
    match reason {
        "current process" => "Legion cannot stop itself.".to_owned(),
        "ancestor of current process" => {
            "Legion cannot stop a program it is running inside.".to_owned()
        }
        _ => "The system requires this program and will not stop it.".to_owned(),
    }
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
            gpu_memory: None,
            start_time: rfc3339(0),
            is_self: false,
        }
    }

    #[test]
    fn processes_are_sorted_busiest_first_and_ties_by_pid() {
        let mut p1 = proc(1, 5.0, 5);
        p1.gpu_memory = Some(100);
        let mut p2 = proc(2, 1.0, 10);
        p2.gpu_memory = Some(500);
        let p3 = proc(3, 1.0, 10);
        let mut list = vec![p3, p1, p2];

        sort_procs(&mut list, SortBy::Cpu);
        let pids: Vec<u32> = list.iter().map(|p| p.pid).collect();
        assert_eq!(pids, [1, 2, 3]);

        sort_procs(&mut list, SortBy::Memory);
        let pids: Vec<u32> = list.iter().map(|p| p.pid).collect();
        assert_eq!(pids, [2, 3, 1]);

        sort_procs(&mut list, SortBy::GpuMemory);
        let pids: Vec<u32> = list.iter().map(|p| p.pid).collect();
        assert_eq!(pids, [2, 1, 3]);
    }

    #[test]
    fn grouping_sums_orders_caps_at_64_and_counts_more() {
        let mut list = Vec::new();
        // 70 rustc processes for user "me", pids 1..=70, busiest is pid 70.
        for i in 1..=70u32 {
            list.push(Proc {
                pid: i,
                ppid: Some(1),
                name: "rustc".to_owned(),
                cmd: format!("rustc worker-{i}"),
                user: Some("me".to_owned()),
                cpu_pct: f64::from(i),
                memory: u64::from(i) * 1024,
                gpu_memory: None,
                start_time: rfc3339(1_700_000_000 + u64::from(i)),
                is_self: false,
            });
        }
        // Separate user "other" with same name stays a distinct row.
        list.push(Proc {
            pid: 500,
            ppid: Some(1),
            name: "rustc".to_owned(),
            cmd: "rustc other".to_owned(),
            user: Some("other".to_owned()),
            cpu_pct: 5.0,
            memory: 2048,
            gpu_memory: None,
            start_time: rfc3339(1_700_000_500),
            is_self: false,
        });
        // GPU-heavy python process.
        list.push(Proc {
            pid: 600,
            ppid: Some(1),
            name: "python3".to_owned(),
            cmd: "python3 train.py".to_owned(),
            user: Some("me".to_owned()),
            cpu_pct: 12.0,
            memory: 4096,
            gpu_memory: Some(9_000_000_000),
            start_time: rfc3339(1_700_000_600),
            is_self: false,
        });

        let by_cpu = group_procs(list.clone(), SortBy::Cpu);
        assert_eq!(by_cpu.len(), 3);
        let rustc = &by_cpu[0];
        assert_eq!(rustc.name, "rustc");
        assert_eq!(rustc.user.as_deref(), Some("me"));
        assert_eq!(rustc.count, 70);
        assert_eq!(rustc.cpu_pct, 2485.0);
        assert_eq!(rustc.memory, 2485 * 1024);
        assert_eq!(rustc.gpu_memory, None);
        assert_eq!(rustc.members.len(), 64);
        assert_eq!(rustc.more, 6);
        // Members are ordered busiest first (pid 70 down to pid 7).
        assert_eq!(rustc.members[0].pid, 70);
        assert_eq!(rustc.members[63].pid, 7);
        assert_eq!(rustc.cmd.as_deref(), Some("rustc worker-70"));

        let by_gpu = group_procs(list, SortBy::GpuMemory);
        assert_eq!(by_gpu[0].name, "python3");
        assert_eq!(by_gpu[0].gpu_memory, Some(9_000_000_000));
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
        // Pid 1 and 4 are ordinary on the other family for single-pid mode.
        assert_eq!(
            decide(1, own, "windows", Lookup::Missing, Probe::Missing),
            Outcome::NotFound
        );
        assert_eq!(
            decide(4, own, "linux", Lookup::Missing, Probe::Missing),
            Outcome::NotFound
        );
        // Targets mode refuses pid 0 and 1 on every OS.
        assert!(protected_target_pid(0, own, "windows").is_some());
        assert!(protected_target_pid(1, own, "windows").is_some());
        assert!(protected_target_pid(1, own, "linux").is_some());
    }

    #[test]
    fn times_round_trip_through_rfc3339_and_zero_is_absent() {
        assert_eq!(rfc3339(0), None);
        let text = rfc3339(1_700_000_000).unwrap_or_default();
        assert_eq!(text, "2023-11-14T22:13:20Z");
        assert_eq!(parse_rfc3339(&text), Some(1_700_000_000));
        assert_eq!(parse_rfc3339("not a time"), None);
    }
}
