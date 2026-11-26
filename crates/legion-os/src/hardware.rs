//! Optional hardware probes: processor temperature, graphics cards, battery, and per-process
//! graphics memory.
//!
//! Every source is probed independently with a one-second limit and run side by side with the
//! others: a missing, malformed, or slow source leaves its fields out and never fails or delays
//! the caller.
//!
//! All probes accept their sysfs root directory or command as parameters so tests exercise every
//! parser and timeout rule against fixture trees and recorded output rather than the host machine.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(any(target_os = "linux", windows))]
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;

use crate::run::{RunOptions, run};
use crate::shell::CommandSpec;

/// Default time limit for each independent hardware probe.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

const MIB: u64 = 1024 * 1024;
const PROBE_CAPTURE_LIMIT: usize = 64 * 1024;
const PROBE_KILL_GRACE: Duration = Duration::from_millis(150);

/// Battery state reported by the operating system.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BatteryState {
    /// Connected to power and gaining charge.
    Charging,
    /// Running on battery power.
    Discharging,
    /// Fully charged.
    Full,
    /// Present, but the system does not report a more specific state.
    Unknown,
}

/// System battery readings, absent on machines without a battery.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Battery {
    /// Charge percentage, 0 to 100.
    pub percent: u8,
    /// Current battery state.
    pub state: BatteryState,
    /// Whether external AC power is connected, when reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_ac: Option<bool>,
    /// Estimated seconds remaining (to empty while discharging, or to full while charging).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_left_s: Option<u64>,
}

/// One graphics card reported by `nvidia-smi` or Linux `/sys/class/drm`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Gpu {
    /// Model or display name.
    pub name: String,
    /// Vendor identifier such as `nvidia`, `amd`, or `intel`, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// Total graphics memory, in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_total: Option<u64>,
    /// Graphics memory currently in use, in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_used: Option<u64>,
    /// Current GPU busy percentage, 0 to 100.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub utilization_pct: Option<f64>,
    /// GPU temperature, in degrees Celsius.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature_c: Option<f64>,
    /// Current GPU power draw, in watts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub power_w: Option<f64>,
}

/// Combined result of probing processor temperature, graphics cards, and battery side by side.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HardwareSnapshot {
    /// Processor package temperature in degrees Celsius, when reported.
    pub cpu_temperature_c: Option<f64>,
    /// Graphics cards reported by the system or driver.
    pub gpus: Vec<Gpu>,
    /// System battery information, when a battery is present.
    pub battery: Option<Battery>,
}

/// Parameters controlling where each hardware probe reads from and how long it may take.
#[derive(Clone, Debug)]
pub struct ProbeConfig {
    /// Root of the Linux sysfs tree (normally `/sys`).
    pub sys_root: PathBuf,
    /// Command used to query NVIDIA GPUs.
    pub nvidia_gpu_cmd: CommandSpec,
    /// Command used on macOS to query battery state.
    pub macos_battery_cmd: CommandSpec,
    /// Per-probe wall-clock timeout.
    pub timeout: Duration,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            sys_root: PathBuf::from("/sys"),
            nvidia_gpu_cmd: default_nvidia_gpu_command(),
            macos_battery_cmd: default_macos_battery_command(),
            timeout: DEFAULT_PROBE_TIMEOUT,
        }
    }
}

/// Default `nvidia-smi` invocation for GPU summary readings.
pub fn default_nvidia_gpu_command() -> CommandSpec {
    CommandSpec::argv(
        "nvidia-smi",
        [
            "--query-gpu=name,memory.total,memory.used,utilization.gpu,temperature.gpu,power.draw",
            "--format=csv,noheader,nounits",
        ],
    )
}

/// Default `nvidia-smi` invocation for compute process GPU memory.
pub fn default_nvidia_compute_procs_command() -> CommandSpec {
    CommandSpec::argv(
        "nvidia-smi",
        [
            "--query-compute-apps=pid,used_gpu_memory",
            "--format=csv,noheader,nounits",
        ],
    )
}

/// Default `nvidia-smi` invocation for combined compute and graphics process GPU memory.
pub fn default_nvidia_pmon_procs_command() -> CommandSpec {
    CommandSpec::argv("nvidia-smi", ["pmon", "-c", "1", "-s", "m"])
}

/// Default macOS battery query command.
pub fn default_macos_battery_command() -> CommandSpec {
    CommandSpec::argv("/usr/bin/pmset", ["-g", "batt"])
}

/// Run every hardware probe concurrently, each bounded by `config.timeout`.
pub async fn probe_all(config: ProbeConfig) -> HardwareSnapshot {
    let timeout = config.timeout;
    let sys_for_cpu = config.sys_root.clone();
    let sys_for_batt = config.sys_root.clone();
    let sys_for_drm = config.sys_root;
    let nvidia_cmd = config.nvidia_gpu_cmd;
    let macos_batt_cmd = config.macos_battery_cmd;

    let (cpu_temperature_c, nvidia_gpus, drm_gpus, battery) = tokio::join!(
        probe_cpu_temperature(sys_for_cpu, timeout),
        probe_nvidia_gpus(nvidia_cmd, timeout),
        probe_drm_gpus(sys_for_drm, timeout),
        probe_battery(sys_for_batt, macos_batt_cmd, timeout),
    );

    HardwareSnapshot {
        cpu_temperature_c,
        gpus: merge_gpus(nvidia_gpus, drm_gpus),
        battery,
    }
}

/// Single-flight guard for one kind of hardware probe.
///
/// At most one probe thread of a kind may run at once. If a probe fails to return within its
/// timeout, the slot stays marked stuck so later polls return `None` immediately without spawning
/// more threads; once the slow read finally returns, the mark clears automatically.
#[derive(Clone, Debug, Default)]
pub struct ProbeSlot {
    in_flight: Arc<AtomicBool>,
}

impl ProbeSlot {
    /// Create a fresh, idle probe slot.
    pub fn new() -> Self {
        Self {
            in_flight: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Return `true` when a read is currently in flight or stuck on this slot.
    pub fn is_busy(&self) -> bool {
        self.in_flight.load(Ordering::Acquire)
    }

    fn try_acquire(&self) -> Option<ProbeSlotGuard> {
        self.in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| ProbeSlotGuard {
                in_flight: Arc::clone(&self.in_flight),
            })
    }
}

struct ProbeSlotGuard {
    in_flight: Arc<AtomicBool>,
}

impl Drop for ProbeSlotGuard {
    fn drop(&mut self) {
        self.in_flight.store(false, Ordering::Release);
    }
}

#[cfg(target_os = "linux")]
static CPU_TEMP_SLOT: LazyLock<ProbeSlot> = LazyLock::new(ProbeSlot::new);
#[cfg(target_os = "linux")]
static DRM_GPUS_SLOT: LazyLock<ProbeSlot> = LazyLock::new(ProbeSlot::new);
#[cfg(any(target_os = "linux", windows))]
static BATTERY_SLOT: LazyLock<ProbeSlot> = LazyLock::new(ProbeSlot::new);

/// Probe per-process graphics memory (in bytes) via one `nvidia-smi` invocation bounded by
/// `timeout`.
pub async fn probe_gpu_process_memory(cmd: CommandSpec, timeout: Duration) -> HashMap<u32, u64> {
    run_probe_command(cmd, timeout)
        .await
        .as_deref()
        .map(parse_nvidia_pmon_procs)
        .unwrap_or_default()
}

/// Merge GPU lists from `nvidia-smi` and `/sys/class/drm`, avoiding duplicate NVIDIA cards when
/// `nvidia-smi` already reported them.
pub fn merge_gpus(nvidia: Vec<Gpu>, drm: Vec<Gpu>) -> Vec<Gpu> {
    let has_nvidia = !nvidia.is_empty();
    let mut out = nvidia;
    for gpu in drm {
        if has_nvidia && gpu.vendor.as_deref() == Some("nvidia") {
            continue;
        }
        out.push(gpu);
    }
    out
}

/// Merge per-process GPU memory maps from compute-apps and pmon, keeping the larger reported
/// figure per PID so a process listed in both is never double-counted.
pub fn merge_gpu_process_memory(
    mut compute: HashMap<u32, u64>,
    pmon: HashMap<u32, u64>,
) -> HashMap<u32, u64> {
    for (pid, bytes) in pmon {
        compute
            .entry(pid)
            .and_modify(|existing| {
                if bytes > *existing {
                    *existing = bytes;
                }
            })
            .or_insert(bytes);
    }
    compute
}

/// Run a blocking probe on a dedicated detached thread (outside the Tokio blocking pool) guarded
/// by `slot`, with a wall-clock `timeout`.
///
/// If `slot` already has a read in flight (for example a stuck driver read), this returns `None`
/// immediately without spawning another thread. Because the thread is detached, a wedged kernel
/// read never blocks Tokio's blocking pool or process exit.
pub async fn run_guarded_probe<T, F>(
    slot: &ProbeSlot,
    name: &'static str,
    timeout: Duration,
    work: F,
) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> Option<T> + Send + 'static,
{
    let guard = slot.try_acquire()?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    // Keep the thread name within Linux's 15-byte `TASK_COMM_LEN - 1` limit so
    // `pthread_setname_np` sets `/proc/self/task/<tid>/comm` reliably.
    let mut thread_name = format!("probe-{name}");
    if thread_name.len() > 15 {
        let mut end = 15;
        while !thread_name.is_char_boundary(end) {
            end -= 1;
        }
        thread_name.truncate(end);
    }
    let spawned = std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let _guard = guard;
            let value = work();
            let _ = tx.send(value);
        });
    if spawned.is_err() {
        return None;
    }
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(value)) => value,
        _ => None,
    }
}

/// Run a one-off blocking probe on a dedicated detached thread with a wall-clock timeout,
/// returning `None` if it panics or exceeds `timeout`.
pub async fn run_blocking_probe<T, F>(timeout: Duration, work: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> Option<T> + Send + 'static,
{
    let slot = ProbeSlot::new();
    run_guarded_probe(&slot, "oneshot", timeout, work).await
}

/// Probe CPU package temperature from `sys_root` on Linux; absent on macOS and Windows.
pub async fn probe_cpu_temperature(sys_root: PathBuf, timeout: Duration) -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        run_guarded_probe(&CPU_TEMP_SLOT, "cpu-temp", timeout, move || {
            read_linux_cpu_temperature(&sys_root)
        })
        .await
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (sys_root, timeout);
        None
    }
}

/// Probe NVIDIA GPUs by running `cmd` with a bounded timeout.
pub async fn probe_nvidia_gpus(cmd: CommandSpec, timeout: Duration) -> Vec<Gpu> {
    run_probe_command(cmd, timeout)
        .await
        .map(|text| parse_nvidia_gpus(&text))
        .unwrap_or_default()
}

/// Probe graphics cards exposed under Linux `<sys_root>/class/drm`; empty on macOS and Windows.
pub async fn probe_drm_gpus(sys_root: PathBuf, timeout: Duration) -> Vec<Gpu> {
    #[cfg(target_os = "linux")]
    {
        run_guarded_probe(&DRM_GPUS_SLOT, "drm-gpus", timeout, move || {
            Some(read_linux_drm_gpus(&sys_root))
        })
        .await
        .unwrap_or_default()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (sys_root, timeout);
        Vec::new()
    }
}

/// Probe the system battery using the platform's own interface, bounded by `timeout`.
pub async fn probe_battery(
    sys_root: PathBuf,
    macos_cmd: CommandSpec,
    timeout: Duration,
) -> Option<Battery> {
    #[cfg(target_os = "linux")]
    {
        let _ = macos_cmd;
        run_guarded_probe(&BATTERY_SLOT, "battery", timeout, move || {
            read_linux_battery(&sys_root)
        })
        .await
    }
    #[cfg(target_os = "macos")]
    {
        let _ = sys_root;
        probe_macos_battery(macos_cmd, timeout).await
    }
    #[cfg(windows)]
    {
        let _ = (sys_root, macos_cmd);
        run_guarded_probe(&BATTERY_SLOT, "battery", timeout, || {
            let raw = crate::win_proc::query_system_power_status()?;
            parse_windows_power_status(
                raw.ac_line_status,
                raw.battery_flag,
                raw.battery_life_percent,
                raw.battery_life_time,
            )
        })
        .await
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = (sys_root, macos_cmd, timeout);
        None
    }
}

async fn run_probe_command(command: CommandSpec, timeout: Duration) -> Option<String> {
    let options = RunOptions::new(command)
        .c_locale()
        .timeout(timeout)
        .kill_grace(PROBE_KILL_GRACE)
        .capture_limit(PROBE_CAPTURE_LIMIT);
    match run(options).await {
        Ok(output) if output.success() => Some(output.stdout.text),
        _ => None,
    }
}

// ── Linux sysfs: safe regular-file reader ────────────────────────────────────────────────────

/// Canonicalized sysfs root guaranteeing every read resolves inside the tree and only opens
/// regular files (never FIFOs, sockets, or device nodes).
struct SysfsTree {
    canon_root: PathBuf,
}

impl SysfsTree {
    fn open(sys_root: &Path) -> Option<Self> {
        let canon_root = fs::canonicalize(sys_root).ok()?;
        Some(Self { canon_root })
    }

    fn read_trimmed(&self, path: &Path) -> Option<String> {
        let resolved = fs::canonicalize(path).ok()?;
        if !resolved.starts_with(&self.canon_root) {
            return None;
        }
        let meta = fs::symlink_metadata(&resolved).ok()?;
        if !meta.file_type().is_file() {
            return None;
        }
        let mut opts = OpenOptions::new();
        opts.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_NONBLOCK);
        }
        let file = opts.open(&resolved).ok()?;
        if !file.metadata().ok()?.file_type().is_file() {
            return None;
        }
        let mut buf = Vec::new();
        file.take(PROBE_CAPTURE_LIMIT as u64)
            .read_to_end(&mut buf)
            .ok()?;
        let text = String::from_utf8(buf).ok()?;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    }

    fn read_u64(&self, path: &Path) -> Option<u64> {
        self.read_trimmed(path)?.parse::<u64>().ok()
    }
}

// ── Linux sysfs: hwmon (CPU temperature) ─────────────────────────────────────────────────────

/// Read the processor package temperature from a Linux sysfs root (`<sys_root>/class/hwmon` with
/// fallback to `<sys_root>/class/thermal`).
pub fn read_linux_cpu_temperature(sys_root: &Path) -> Option<f64> {
    let tree = SysfsTree::open(sys_root)?;
    if let Some(temp) = read_hwmon_cpu_temperature(&tree, &sys_root.join("class/hwmon")) {
        return Some(temp);
    }
    read_thermal_zone_cpu_temperature(&tree, &sys_root.join("class/thermal"))
}

fn read_hwmon_cpu_temperature(tree: &SysfsTree, hwmon_dir: &Path) -> Option<f64> {
    let mut entries = sorted_dir_entries(hwmon_dir)?;
    let mut best: Option<(u8, f64)> = None;

    for path in entries.drain(..) {
        let Some(driver) = tree.read_trimmed(&path.join("name")) else {
            continue;
        };
        let driver_lower = driver.to_ascii_lowercase();
        let Some(priority) = cpu_hwmon_priority(&driver_lower) else {
            continue;
        };

        let readings = read_hwmon_temps(tree, &path);
        let Some(temp) = select_cpu_hwmon_temp(&driver_lower, &readings) else {
            continue;
        };

        let replace = match best {
            None => true,
            Some((best_prio, best_temp)) => {
                priority < best_prio || (priority == best_prio && temp > best_temp)
            }
        };
        if replace {
            best = Some((priority, temp));
        }
    }

    best.map(|(_, temp)| round1(temp))
}

/// Lower number = stronger match for a CPU package sensor.
fn cpu_hwmon_priority(driver: &str) -> Option<u8> {
    match driver {
        "coretemp" | "k10temp" | "zenpower" | "fam15h_power" => Some(0),
        "cpu_thermal" | "soc_thermal" | "bcm2835_thermal" | "scpi_sensors" => Some(1),
        "x86_pkg_temp" => Some(2),
        "acpitz" => Some(3),
        _ => None,
    }
}

fn select_cpu_hwmon_temp(driver: &str, readings: &[(Option<String>, f64)]) -> Option<f64> {
    if readings.is_empty() {
        return None;
    }
    // Prefer an explicit package/die label when present.
    let preferred_labels: &[&str] = match driver {
        "coretemp" => &["package id", "package"],
        "k10temp" | "zenpower" => &["tctl", "tdie", "tccd1"],
        _ => &["package", "cpu", "tctl", "tdie"],
    };
    for needle in preferred_labels {
        let mut matched: Option<f64> = None;
        for (label, temp) in readings {
            if let Some(lbl) = label
                && lbl.to_ascii_lowercase().contains(needle)
            {
                matched = Some(matched.map_or(*temp, |prev| prev.max(*temp)));
            }
        }
        if matched.is_some() {
            return matched;
        }
    }
    // Fall back to the highest valid reading on that CPU hwmon device.
    readings.iter().map(|(_, t)| *t).reduce(f64::max)
}

fn read_hwmon_temps(tree: &SysfsTree, dir: &Path) -> Vec<(Option<String>, f64)> {
    let Some(entries) = sorted_dir_entries(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries {
        let Some(name) = entry.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(rest) = name.strip_prefix("temp") else {
            continue;
        };
        let Some(index) = rest.strip_suffix("_input") else {
            continue;
        };
        if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Some(raw) = tree.read_trimmed(&entry) else {
            continue;
        };
        let Some(temp_c) = parse_millidegrees(&raw) else {
            continue;
        };
        let label = tree.read_trimmed(&dir.join(format!("temp{index}_label")));
        out.push((label, temp_c));
    }
    out
}

fn read_thermal_zone_cpu_temperature(tree: &SysfsTree, thermal_dir: &Path) -> Option<f64> {
    let entries = sorted_dir_entries(thermal_dir)?;
    let mut best: Option<(u8, f64)> = None;
    for path in entries {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with("thermal_zone") {
            continue;
        }
        let Some(kind) = tree.read_trimmed(&path.join("type")) else {
            continue;
        };
        let lower = kind.to_ascii_lowercase();
        let priority = match lower.as_str() {
            "x86_pkg_temp" | "cpu-thermal" | "cpu_thermal" | "soc-thermal" | "soc_thermal" => 0,
            "acpitz" => 1,
            _ => continue,
        };
        let Some(raw) = tree.read_trimmed(&path.join("temp")) else {
            continue;
        };
        let Some(temp_c) = parse_millidegrees(&raw) else {
            continue;
        };
        let replace = match best {
            None => true,
            Some((best_prio, best_temp)) => {
                priority < best_prio || (priority == best_prio && temp_c > best_temp)
            }
        };
        if replace {
            best = Some((priority, temp_c));
        }
    }
    best.map(|(_, t)| round1(t))
}

fn parse_millidegrees(raw: &str) -> Option<f64> {
    let milli = raw.trim().parse::<i64>().ok()?;
    let celsius = (milli as f64) / 1000.0;
    if celsius.is_finite() && (-40.0..150.0).contains(&celsius) {
        Some(celsius)
    } else {
        None
    }
}

// ── Linux sysfs: power_supply (battery) ──────────────────────────────────────────────────────

/// Read system battery information from a Linux sysfs root (`<sys_root>/class/power_supply`).
///
/// Device batteries (`scope = Device`, such as a wireless mouse or keyboard) are ignored.
pub fn read_linux_battery(sys_root: &Path) -> Option<Battery> {
    let tree = SysfsTree::open(sys_root)?;
    let ps_dir = sys_root.join("class/power_supply");
    let entries = sorted_dir_entries(&ps_dir)?;

    let mut mains_online: Option<bool> = None;
    let mut batteries: Vec<LinuxBatterySample> = Vec::new();

    for path in entries {
        let Some(kind) = tree.read_trimmed(&path.join("type")) else {
            continue;
        };
        if kind.eq_ignore_ascii_case("Mains") {
            if let Some(online) = tree.read_trimmed(&path.join("online")) {
                if online == "1" {
                    mains_online = Some(true);
                } else if online == "0" && mains_online != Some(true) {
                    mains_online = Some(false);
                }
            }
            continue;
        }
        if !kind.eq_ignore_ascii_case("Battery") {
            continue;
        }
        // Ignore peripheral batteries (mouse, keyboard, headphones).
        if let Some(scope) = tree.read_trimmed(&path.join("scope"))
            && scope.eq_ignore_ascii_case("Device")
        {
            continue;
        }
        if let Some(present) = tree.read_trimmed(&path.join("present"))
            && present == "0"
        {
            continue;
        }
        if let Some(sample) = read_single_linux_battery(&tree, &path) {
            batteries.push(sample);
        }
    }

    if batteries.is_empty() {
        return None;
    }

    // Combine multiple system batteries weighted by full energy when available, else average.
    let total_weight: u64 = batteries.iter().filter_map(|b| b.weight).sum();
    let percent = if total_weight > 0 && batteries.iter().all(|b| b.weight.is_some()) {
        let weighted: u128 = batteries
            .iter()
            .map(|b| u128::from(b.percent) * u128::from(b.weight.unwrap_or(0)))
            .sum();
        let pct = (weighted + u128::from(total_weight) / 2) / u128::from(total_weight);
        u8::try_from(pct.min(100)).unwrap_or(100)
    } else {
        let sum: u64 = batteries.iter().map(|b| u64::from(b.percent)).sum();
        let count = batteries.len() as u64;
        u8::try_from(((sum + count / 2) / count).min(100)).unwrap_or(100)
    };

    let state = if batteries.iter().any(|b| b.state == BatteryState::Charging) {
        BatteryState::Charging
    } else if batteries
        .iter()
        .any(|b| b.state == BatteryState::Discharging)
    {
        BatteryState::Discharging
    } else if batteries.iter().all(|b| b.state == BatteryState::Full) {
        BatteryState::Full
    } else {
        BatteryState::Unknown
    };

    let on_ac = mains_online.or(match state {
        BatteryState::Charging | BatteryState::Full => Some(true),
        BatteryState::Discharging => Some(false),
        BatteryState::Unknown => None,
    });

    let time_left_s = combine_battery_time_left(&batteries, state);

    Some(Battery {
        percent,
        state,
        on_ac,
        time_left_s,
    })
}

struct LinuxBatterySample {
    percent: u8,
    state: BatteryState,
    weight: Option<u64>,
    time_left_s: Option<u64>,
}

fn read_single_linux_battery(tree: &SysfsTree, dir: &Path) -> Option<LinuxBatterySample> {
    let energy_now = tree.read_u64(&dir.join("energy_now"));
    let energy_full = tree.read_u64(&dir.join("energy_full")).filter(|&v| v > 0);
    let charge_now = tree.read_u64(&dir.join("charge_now"));
    let charge_full = tree.read_u64(&dir.join("charge_full")).filter(|&v| v > 0);

    let percent = if let Some(cap) = tree.read_u64(&dir.join("capacity")) {
        u8::try_from(cap.min(100)).ok()?
    } else if let (Some(now), Some(full)) = (energy_now, energy_full) {
        let pct = (u128::from(now) * 100 + u128::from(full) / 2) / u128::from(full);
        u8::try_from(pct.min(100)).ok()?
    } else if let (Some(now), Some(full)) = (charge_now, charge_full) {
        let pct = (u128::from(now) * 100 + u128::from(full) / 2) / u128::from(full);
        u8::try_from(pct.min(100)).ok()?
    } else {
        return None;
    };

    let status_raw = tree.read_trimmed(&dir.join("status")).unwrap_or_default();
    let state = match status_raw.to_ascii_lowercase().as_str() {
        "charging" => BatteryState::Charging,
        "discharging" => BatteryState::Discharging,
        "full" => BatteryState::Full,
        _ => BatteryState::Unknown,
    };

    let time_left_s = match state {
        BatteryState::Discharging => tree
            .read_u64(&dir.join("time_to_empty_now"))
            .filter(|&v| v > 0)
            .or_else(|| {
                let power = tree.read_u64(&dir.join("power_now")).filter(|&v| v > 0)?;
                let now = energy_now?;
                let secs = (u128::from(now) * 3600) / u128::from(power);
                u64::try_from(secs).ok().filter(|&v| v > 0)
            })
            .or_else(|| {
                let current = tree.read_u64(&dir.join("current_now")).filter(|&v| v > 0)?;
                let now = charge_now?;
                let secs = (u128::from(now) * 3600) / u128::from(current);
                u64::try_from(secs).ok().filter(|&v| v > 0)
            }),
        BatteryState::Charging => tree
            .read_u64(&dir.join("time_to_full_now"))
            .filter(|&v| v > 0)
            .or_else(|| {
                let power = tree.read_u64(&dir.join("power_now")).filter(|&v| v > 0)?;
                let (now, full) = (energy_now?, energy_full?);
                let remaining = full.saturating_sub(now);
                let secs = (u128::from(remaining) * 3600) / u128::from(power);
                u64::try_from(secs).ok().filter(|&v| v > 0)
            })
            .or_else(|| {
                let current = tree.read_u64(&dir.join("current_now")).filter(|&v| v > 0)?;
                let (now, full) = (charge_now?, charge_full?);
                let remaining = full.saturating_sub(now);
                let secs = (u128::from(remaining) * 3600) / u128::from(current);
                u64::try_from(secs).ok().filter(|&v| v > 0)
            }),
        BatteryState::Full | BatteryState::Unknown => None,
    };

    Some(LinuxBatterySample {
        percent,
        state,
        weight: energy_full.or(charge_full),
        time_left_s,
    })
}

fn combine_battery_time_left(batteries: &[LinuxBatterySample], state: BatteryState) -> Option<u64> {
    if !matches!(state, BatteryState::Charging | BatteryState::Discharging) {
        return None;
    }
    let mut total: u64 = 0;
    let mut any = false;
    for batt in batteries {
        if let Some(secs) = batt.time_left_s {
            total = total.saturating_add(secs);
            any = true;
        }
    }
    any.then_some(total).filter(|&v| v > 0)
}

// ── Linux sysfs: drm (non-NVIDIA GPUs) ───────────────────────────────────────────────────────

/// Read graphics cards from a Linux sysfs root (`<sys_root>/class/drm`).
///
/// Cards whose PCI runtime power state (`power/runtime_status`) is `suspended` are reported with
/// only their static facts (`name` and `vendor`) so polling never wakes a sleeping discrete GPU.
pub fn read_linux_drm_gpus(sys_root: &Path) -> Vec<Gpu> {
    let Some(tree) = SysfsTree::open(sys_root) else {
        return Vec::new();
    };
    let drm_dir = sys_root.join("class/drm");
    let Some(entries) = sorted_dir_entries(&drm_dir) else {
        return Vec::new();
    };

    let mut gpus = Vec::new();
    for card_path in entries {
        let Some(name) = card_path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(num) = name.strip_prefix("card") else {
            continue;
        };
        if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }

        let device_dir = card_path.join("device");
        let Some(vendor_raw) = tree.read_trimmed(&device_dir.join("vendor")) else {
            continue;
        };
        let vendor = pci_vendor_name(&vendor_raw);
        let gpu_name = read_drm_gpu_name(&tree, &device_dir, vendor.as_deref());

        // Skip dynamic metrics when the card is runtime-suspended so polling never wakes it.
        if let Some(status) = tree.read_trimmed(&device_dir.join("power/runtime_status"))
            && status.eq_ignore_ascii_case("suspended")
        {
            gpus.push(Gpu {
                name: gpu_name,
                vendor,
                memory_total: None,
                memory_used: None,
                utilization_pct: None,
                temperature_c: None,
                power_w: None,
            });
            continue;
        }

        let utilization_pct = tree
            .read_u64(&device_dir.join("gpu_busy_percent"))
            .filter(|&v| v <= 100)
            .map(|v| round1(v as f64));
        let memory_total = tree
            .read_u64(&device_dir.join("mem_info_vram_total"))
            .filter(|&v| v > 0);
        let memory_used =
            memory_total.and_then(|_| tree.read_u64(&device_dir.join("mem_info_vram_used")));

        let (temperature_c, power_w) = read_drm_hwmon(&tree, &device_dir.join("hwmon"));

        gpus.push(Gpu {
            name: gpu_name,
            vendor,
            memory_total,
            memory_used,
            utilization_pct,
            temperature_c,
            power_w,
        });
    }
    gpus
}

fn pci_vendor_name(raw: &str) -> Option<String> {
    let lower = raw.trim().to_ascii_lowercase();
    let known = match lower.as_str() {
        "0x10de" => "nvidia",
        "0x1002" => "amd",
        "0x8086" => "intel",
        "0x106b" => "apple",
        "0x1414" => "microsoft",
        "0x1af4" => "virtio",
        "0x15ad" => "vmware",
        "" => return None,
        other => return Some(other.to_owned()),
    };
    Some(known.to_owned())
}

fn read_drm_gpu_name(tree: &SysfsTree, device_dir: &Path, vendor: Option<&str>) -> String {
    if let Some(product) = tree.read_trimmed(&device_dir.join("product_name"))
        && !product.is_empty()
    {
        return product;
    }
    match vendor {
        Some("amd") => "AMD GPU".to_owned(),
        Some("intel") => "Intel GPU".to_owned(),
        Some("nvidia") => "NVIDIA GPU".to_owned(),
        Some("apple") => "Apple GPU".to_owned(),
        Some(other) => format!("{other} GPU"),
        None => "GPU".to_owned(),
    }
}

fn read_drm_hwmon(tree: &SysfsTree, hwmon_dir: &Path) -> (Option<f64>, Option<f64>) {
    let Some(entries) = sorted_dir_entries(hwmon_dir) else {
        return (None, None);
    };
    let mut max_temp: Option<f64> = None;
    let mut power_w: Option<f64> = None;

    for dir in entries {
        for (_, temp) in read_hwmon_temps(tree, &dir) {
            max_temp = Some(max_temp.map_or(temp, |prev| prev.max(temp)));
        }
        if power_w.is_none() {
            let microwatts = tree
                .read_u64(&dir.join("power1_average"))
                .or_else(|| tree.read_u64(&dir.join("power1_input")));
            if let Some(uw) = microwatts {
                let watts = (uw as f64) / 1_000_000.0;
                if watts.is_finite() && (0.0..2000.0).contains(&watts) {
                    power_w = Some(round1(watts));
                }
            }
        }
    }

    (max_temp.map(round1), power_w)
}

// ── nvidia-smi parsers ───────────────────────────────────────────────────────────────────────

/// Parse `nvidia-smi --query-gpu=name,memory.total,memory.used,utilization.gpu,temperature.gpu,power.draw --format=csv,noheader,nounits`.
///
/// Requires all 6 columns and drops any line whose numeric columns are neither valid numbers nor
/// recognized `nvidia-smi` N/A sentinels (`[N/A]`, `[Not Supported]`, `N/A`, `-`).
pub fn parse_nvidia_gpus(text: &str) -> Vec<Gpu> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            if fields.len() != 6 {
                return None;
            }
            let name = fields[0];
            if name.is_empty() {
                return None;
            }
            let memory_total = parse_nvidia_opt_u64(fields[1])
                .ok()?
                .map(|mib| mib.saturating_mul(MIB));
            let memory_used = parse_nvidia_opt_u64(fields[2])
                .ok()?
                .map(|mib| mib.saturating_mul(MIB));
            let utilization_pct = parse_nvidia_opt_f64(fields[3])
                .ok()?
                .filter(|u| (0.0..=100.0).contains(u))
                .map(round1);
            let temperature_c = parse_nvidia_opt_f64(fields[4])
                .ok()?
                .filter(|t| (-40.0..150.0).contains(t))
                .map(round1);
            let power_w = parse_nvidia_opt_f64(fields[5])
                .ok()?
                .filter(|w| (0.0..2000.0).contains(w))
                .map(round1);

            Some(Gpu {
                name: name.to_owned(),
                vendor: Some("nvidia".to_owned()),
                memory_total,
                memory_used,
                utilization_pct,
                temperature_c,
                power_w,
            })
        })
        .collect()
}

/// Parse `nvidia-smi --query-compute-apps=pid,used_gpu_memory --format=csv,noheader,nounits`,
/// summing bytes per PID across GPUs.
pub fn parse_nvidia_compute_procs(text: &str) -> HashMap<u32, u64> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        if fields.len() != 2 {
            continue;
        }
        let Ok(pid) = fields[0].parse::<u32>() else {
            continue;
        };
        let Ok(Some(mib)) = parse_nvidia_opt_u64(fields[1]) else {
            continue;
        };
        if mib == 0 {
            continue;
        }
        let bytes = mib.saturating_mul(MIB);
        out.entry(pid)
            .and_modify(|sum: &mut u64| *sum = sum.saturating_add(bytes))
            .or_insert(bytes);
    }
    out
}

/// Parse `nvidia-smi pmon -c 1 -s m`, summing framebuffer memory (in bytes) per PID across GPUs.
pub fn parse_nvidia_pmon_procs(text: &str) -> HashMap<u32, u64> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = trimmed.split_whitespace().collect();
        // Columns: gpu, pid, type (C/G/C+G), fb (MB), [ccpm (MB)], command
        if cols.len() < 5 {
            continue;
        }
        if cols[0].parse::<u32>().is_err() {
            continue;
        }
        let Ok(pid) = cols[1].parse::<u32>() else {
            continue;
        };
        if !matches!(cols[2], "C" | "G" | "C+G") {
            continue;
        }
        let Ok(Some(mib)) = parse_nvidia_opt_u64(cols[3]) else {
            continue;
        };
        if mib == 0 {
            continue;
        }
        let bytes = mib.saturating_mul(MIB);
        out.entry(pid)
            .and_modify(|sum: &mut u64| *sum = sum.saturating_add(bytes))
            .or_insert(bytes);
    }
    out
}

fn is_nvidia_na_sentinel(trimmed: &str) -> bool {
    trimmed == "-"
        || trimmed.eq_ignore_ascii_case("N/A")
        || trimmed.eq_ignore_ascii_case("[N/A]")
        || trimmed.eq_ignore_ascii_case("[Not Supported]")
}

fn parse_nvidia_opt_u64(raw: &str) -> Result<Option<u64>, ()> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(());
    }
    if is_nvidia_na_sentinel(trimmed) {
        return Ok(None);
    }
    if let Ok(val) = trimmed.parse::<u64>() {
        return Ok(Some(val));
    }
    if let Ok(val) = trimmed.parse::<f64>()
        && val.is_finite()
        && val >= 0.0
    {
        return Ok(Some(val.round() as u64));
    }
    Err(())
}

fn parse_nvidia_opt_f64(raw: &str) -> Result<Option<f64>, ()> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(());
    }
    if is_nvidia_na_sentinel(trimmed) {
        return Ok(None);
    }
    trimmed
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .map(Some)
        .ok_or(())
}

// ── macOS & Windows battery parsers ──────────────────────────────────────────────────────────

/// Probe the macOS system battery by running `cmd` (normally `/usr/bin/pmset -g batt`) within
/// `timeout` and parsing its output.
pub async fn probe_macos_battery(cmd: CommandSpec, timeout: Duration) -> Option<Battery> {
    let text = run_probe_command(cmd, timeout).await?;
    parse_macos_pmset_batt(&text)
}

/// Parse macOS `pmset -g batt` output.
///
/// Desktop Macs without an internal battery (or with only an external UPS) print no present
/// `InternalBattery` line and return `None`.
pub fn parse_macos_pmset_batt(text: &str) -> Option<Battery> {
    let mut on_ac: Option<bool> = None;
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.contains("'ac power'") {
            on_ac = Some(true);
        } else if lower.contains("'battery power'") {
            on_ac = Some(false);
        }
        if !line.contains("InternalBattery") {
            continue;
        }
        if lower.contains("present: false") {
            continue;
        }
        // Examples:
        // `-InternalBattery-0 (id=1234567)\t84%; charging; 0:42 remaining present: true`
        // `-InternalBattery-0 (id=1234567)\t80%; AC attached; not charging present: true`
        // `-InternalBattery-0 (id=1234567)\t100%; charged; 0:00 remaining present: true`
        let after_tab = line.split_once('\t').map_or(line, |(_, rhs)| rhs);
        let parts: Vec<&str> = after_tab.split(';').map(str::trim).collect();
        let percent_token = parts.first()?.split_whitespace().last()?;
        let percent_str = percent_token.strip_suffix('%')?.trim();
        let percent = percent_str.parse::<u8>().ok()?.min(100);

        let state_raw = parts.get(1).copied().unwrap_or("").to_ascii_lowercase();
        let state = if state_raw.contains("discharging") {
            BatteryState::Discharging
        } else if state_raw.contains("not charging") {
            BatteryState::Unknown
        } else if state_raw.contains("charging") || state_raw.contains("finishing charge") {
            BatteryState::Charging
        } else if state_raw.contains("charged") {
            BatteryState::Full
        } else {
            BatteryState::Unknown
        };

        let time_left_s = if matches!(state, BatteryState::Charging | BatteryState::Discharging) {
            parts
                .iter()
                .skip(2)
                .find_map(|segment| parse_pmset_remaining(segment))
        } else {
            None
        };

        return Some(Battery {
            percent,
            state,
            on_ac,
            time_left_s,
        });
    }
    None
}

fn parse_pmset_remaining(segment: &str) -> Option<u64> {
    // Format: "4:15 remaining present: true" or "(no estimate)"
    let token = segment.split_whitespace().next()?;
    let (h, m) = token.split_once(':')?;
    let hours = h.parse::<u64>().ok()?;
    let mins = m.parse::<u64>().ok()?;
    if mins >= 60 {
        return None;
    }
    let secs = hours.saturating_mul(3600).saturating_add(mins * 60);
    (secs > 0).then_some(secs)
}

/// Parse Win32 `SYSTEM_POWER_STATUS` fields into [`Battery`].
///
/// Returns `None` when Windows reports no system battery (`BatteryFlag & 128 != 0`), unknown
/// battery flag (`255`), or unknown charge percentage (`255`).
pub fn parse_windows_power_status(
    ac_line_status: u8,
    battery_flag: u8,
    battery_life_percent: u8,
    battery_life_time: u32,
) -> Option<Battery> {
    if battery_flag == 255 || (battery_flag & 128) != 0 || battery_life_percent > 100 {
        return None;
    }
    let on_ac = match ac_line_status {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    };
    let state = if (battery_flag & 8) != 0 {
        BatteryState::Charging
    } else if on_ac == Some(false) {
        BatteryState::Discharging
    } else if on_ac == Some(true) && battery_life_percent >= 99 {
        BatteryState::Full
    } else {
        BatteryState::Unknown
    };
    let time_left_s = if state == BatteryState::Discharging
        && battery_life_time != u32::MAX
        && battery_life_time > 0
    {
        Some(u64::from(battery_life_time))
    } else {
        None
    };
    Some(Battery {
        percent: battery_life_percent,
        state,
        on_ac,
        time_left_s,
    })
}

// ── Small filesystem and math helpers ────────────────────────────────────────────────────────

fn sorted_dir_entries(dir: &Path) -> Option<Vec<PathBuf>> {
    let entries = fs::read_dir(dir).ok()?;
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    Some(paths)
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    #[cfg(unix)]
    use std::time::Instant;

    use super::*;

    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn hwmon_fixture_prefers_cpu_package_and_ignores_malformed_values() {
        let dir = tempfile::tempdir().unwrap();
        // Unrelated sensor (nvme) with higher temp must be ignored.
        write(dir.path(), "class/hwmon/hwmon0/name", "nvme\n");
        write(dir.path(), "class/hwmon/hwmon0/temp1_input", "78000\n");
        // Intel coretemp with Package id 0 and individual cores.
        write(dir.path(), "class/hwmon/hwmon1/name", "coretemp\n");
        write(
            dir.path(),
            "class/hwmon/hwmon1/temp1_label",
            "Package id 0\n",
        );
        write(dir.path(), "class/hwmon/hwmon1/temp1_input", "64500\n");
        write(dir.path(), "class/hwmon/hwmon1/temp2_label", "Core 0\n");
        write(dir.path(), "class/hwmon/hwmon1/temp2_input", "61000\n");
        write(dir.path(), "class/hwmon/hwmon1/temp3_input", "not-a-num\n");

        assert_eq!(read_linux_cpu_temperature(dir.path()), Some(64.5));
    }

    #[test]
    fn hwmon_fixture_reads_amd_k10temp_and_thermal_zone_fallback() {
        let amd = tempfile::tempdir().unwrap();
        write(amd.path(), "class/hwmon/hwmon0/name", "acpitz\n");
        write(amd.path(), "class/hwmon/hwmon0/temp1_input", "40000\n");
        write(amd.path(), "class/hwmon/hwmon1/name", "k10temp\n");
        write(amd.path(), "class/hwmon/hwmon1/temp1_label", "Tctl\n");
        write(amd.path(), "class/hwmon/hwmon1/temp1_input", "72375\n");
        write(amd.path(), "class/hwmon/hwmon1/temp2_label", "Tccd1\n");
        write(amd.path(), "class/hwmon/hwmon1/temp2_input", "69000\n");
        assert_eq!(read_linux_cpu_temperature(amd.path()), Some(72.4));

        let tz = tempfile::tempdir().unwrap();
        write(
            tz.path(),
            "class/thermal/thermal_zone0/type",
            "cpu-thermal\n",
        );
        write(tz.path(), "class/thermal/thermal_zone0/temp", "51200\n");
        assert_eq!(read_linux_cpu_temperature(tz.path()), Some(51.2));

        let empty = tempfile::tempdir().unwrap();
        assert_eq!(read_linux_cpu_temperature(empty.path()), None);
    }

    #[test]
    fn power_supply_fixture_reads_battery_and_ignores_peripheral_devices() {
        let dir = tempfile::tempdir().unwrap();
        // Wireless mouse battery must be ignored.
        write(dir.path(), "class/power_supply/hid-mouse/type", "Battery\n");
        write(dir.path(), "class/power_supply/hid-mouse/scope", "Device\n");
        write(dir.path(), "class/power_supply/hid-mouse/capacity", "12\n");
        write(
            dir.path(),
            "class/power_supply/hid-mouse/status",
            "Discharging\n",
        );

        // AC adapter offline + system battery discharging with energy/power readings.
        write(dir.path(), "class/power_supply/AC/type", "Mains\n");
        write(dir.path(), "class/power_supply/AC/online", "0\n");
        write(dir.path(), "class/power_supply/BAT0/type", "Battery\n");
        write(dir.path(), "class/power_supply/BAT0/present", "1\n");
        write(dir.path(), "class/power_supply/BAT0/capacity", "62\n");
        write(
            dir.path(),
            "class/power_supply/BAT0/status",
            "Discharging\n",
        );
        // 40 Wh remaining at 10 W draw -> 4 hours = 14400 s.
        write(
            dir.path(),
            "class/power_supply/BAT0/energy_now",
            "40000000\n",
        );
        write(
            dir.path(),
            "class/power_supply/BAT0/energy_full",
            "64000000\n",
        );
        write(
            dir.path(),
            "class/power_supply/BAT0/power_now",
            "10000000\n",
        );

        assert_eq!(
            read_linux_battery(dir.path()),
            Some(Battery {
                percent: 62,
                state: BatteryState::Discharging,
                on_ac: Some(false),
                time_left_s: Some(14400),
            })
        );
    }

    #[test]
    fn power_supply_fixture_absent_or_malformed_returns_none() {
        let desktop = tempfile::tempdir().unwrap();
        write(desktop.path(), "class/power_supply/AC/type", "Mains\n");
        write(desktop.path(), "class/power_supply/AC/online", "1\n");
        assert_eq!(read_linux_battery(desktop.path()), None);

        let bad = tempfile::tempdir().unwrap();
        write(bad.path(), "class/power_supply/BAT0/type", "Battery\n");
        write(bad.path(), "class/power_supply/BAT0/capacity", "oops\n");
        assert_eq!(read_linux_battery(bad.path()), None);
    }

    #[test]
    fn drm_fixture_reads_amd_and_intel_cards_with_partial_fields() {
        let dir = tempfile::tempdir().unwrap();
        // Connector directory must be skipped.
        write(dir.path(), "class/drm/card0-DP-1/status", "connected\n");
        // AMD discrete GPU with full metrics.
        write(dir.path(), "class/drm/card0/device/vendor", "0x1002\n");
        write(
            dir.path(),
            "class/drm/card0/device/product_name",
            "Radeon RX 7900 XTX\n",
        );
        write(
            dir.path(),
            "class/drm/card0/device/gpu_busy_percent",
            "42\n",
        );
        write(
            dir.path(),
            "class/drm/card0/device/mem_info_vram_total",
            "25769803776\n",
        );
        write(
            dir.path(),
            "class/drm/card0/device/mem_info_vram_used",
            "4294967296\n",
        );
        write(
            dir.path(),
            "class/drm/card0/device/hwmon/hwmon3/temp1_input",
            "58000\n",
        );
        write(
            dir.path(),
            "class/drm/card0/device/hwmon/hwmon3/power1_average",
            "115400000\n",
        );
        // Intel iGPU with no VRAM or busy percent exposed.
        write(dir.path(), "class/drm/card1/device/vendor", "0x8086\n");
        // NVIDIA card in DRM (deduplicated when nvidia-smi also reports).
        write(dir.path(), "class/drm/card2/device/vendor", "0x10de\n");

        let drm = read_linux_drm_gpus(dir.path());
        assert_eq!(drm.len(), 3);
        assert_eq!(
            drm[0],
            Gpu {
                name: "Radeon RX 7900 XTX".to_owned(),
                vendor: Some("amd".to_owned()),
                memory_total: Some(25_769_803_776),
                memory_used: Some(4_294_967_296),
                utilization_pct: Some(42.0),
                temperature_c: Some(58.0),
                power_w: Some(115.4),
            }
        );
        assert_eq!(
            drm[1],
            Gpu {
                name: "Intel GPU".to_owned(),
                vendor: Some("intel".to_owned()),
                memory_total: None,
                memory_used: None,
                utilization_pct: None,
                temperature_c: None,
                power_w: None,
            }
        );

        let nvidia = parse_nvidia_gpus("NVIDIA GeForce RTX 5080, 16303, 9120, 69, 68, 245.50\n");
        let merged = merge_gpus(nvidia, drm);
        // NVIDIA card from DRM was deduplicated, keeping nvidia-smi + AMD + Intel.
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].vendor.as_deref(), Some("nvidia"));
        assert_eq!(merged[0].temperature_c, Some(68.0));
        assert_eq!(merged[0].power_w, Some(245.5));
        assert_eq!(merged[1].vendor.as_deref(), Some("amd"));
        assert_eq!(merged[2].vendor.as_deref(), Some("intel"));
    }

    #[test]
    fn nvidia_parsers_handle_na_garbage_and_multi_gpu_process_memory() {
        let gpus = parse_nvidia_gpus(
            "NVIDIA GeForce RTX 5080, 16303, 1024, 7, 42, 31.25\n\
             NVIDIA A2, [N/A], 0, [N/A], [Not Supported], [N/A]\n\
             NVIDIA GeForce RTX 5080, 16303, 1024, 7\n\
             Failed to initialize NVML, driver error, please check, logs, code, 99\n\
             NVIDIA GeForce RTX 5080, 16303, 1024, 7, 42, 31.25, extra\n\
             garbage\n",
        );
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].temperature_c, Some(42.0));
        assert_eq!(gpus[0].power_w, Some(31.3));
        assert_eq!(gpus[1].memory_total, None);
        assert_eq!(gpus[1].temperature_c, None);
        assert_eq!(gpus[1].power_w, None);

        let compute =
            parse_nvidia_compute_procs("100, 4096\n100, 1024\n200, [N/A]\n300, 512, extra\nbad\n");
        let pmon = parse_nvidia_pmon_procs(
            "# gpu         pid   type     fb   ccpm    command\n\
             # Idx           #    C/G     MB     MB    name\n\
                 0         100    C+G   4000      -    python3\n\
                 0         300      G    512      -    firefox\n\
                 0         400    BAD    256      -    bogus\n\
                 0           -      -      -      -    -\n",
        );
        let merged = merge_gpu_process_memory(compute, pmon);
        // PID 100 sums 4096 + 1024 MiB from compute (> 4000 MiB from pmon).
        assert_eq!(merged.get(&100), Some(&(5120 * MIB)));
        assert_eq!(merged.get(&200), None);
        // PID 300 is graphics-only, picked up from pmon (extra-column compute row dropped).
        assert_eq!(merged.get(&300), Some(&(512 * MIB)));
        assert_eq!(merged.get(&400), None);
    }

    #[test]
    fn drm_fixture_skips_dynamic_reads_for_runtime_suspended_gpu() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "class/drm/card0/device/vendor", "0x8086\n");
        write(
            dir.path(),
            "class/drm/card0/device/power/runtime_status",
            "active\n",
        );
        write(
            dir.path(),
            "class/drm/card0/device/gpu_busy_percent",
            "15\n",
        );

        // Second card is runtime-suspended: must report only static facts and ignore dynamic files.
        write(dir.path(), "class/drm/card1/device/vendor", "0x1002\n");
        write(
            dir.path(),
            "class/drm/card1/device/product_name",
            "Radeon RX 7600M\n",
        );
        write(
            dir.path(),
            "class/drm/card1/device/power/runtime_status",
            "suspended\n",
        );
        write(
            dir.path(),
            "class/drm/card1/device/gpu_busy_percent",
            "99\n",
        );
        write(
            dir.path(),
            "class/drm/card1/device/mem_info_vram_total",
            "8589934592\n",
        );
        write(
            dir.path(),
            "class/drm/card1/device/mem_info_vram_used",
            "1073741824\n",
        );
        write(
            dir.path(),
            "class/drm/card1/device/hwmon/hwmon4/temp1_input",
            "65000\n",
        );

        let gpus = read_linux_drm_gpus(dir.path());
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].vendor.as_deref(), Some("intel"));
        assert_eq!(gpus[0].utilization_pct, Some(15.0));
        assert_eq!(
            gpus[1],
            Gpu {
                name: "Radeon RX 7600M".to_owned(),
                vendor: Some("amd".to_owned()),
                memory_total: None,
                memory_used: None,
                utilization_pct: None,
                temperature_c: None,
                power_w: None,
            }
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probe_gpu_process_memory_runs_command_once() {
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("calls.log");
        let script = format!(
            "echo call >> '{}' && printf '# gpu pid type fb ccpm command\\n0 4242 C+G 1024 - python3\\n'",
            counter.display()
        );
        let map = probe_gpu_process_memory(
            CommandSpec::argv("sh", ["-c", &script]),
            Duration::from_secs(2),
        )
        .await;
        assert_eq!(map.get(&4242), Some(&(1024 * MIB)));
        let lines = fs::read_to_string(&counter).unwrap();
        assert_eq!(lines.lines().count(), 1, "expected exactly one invocation");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn fifo_sysfs_and_stuck_slot_keep_thread_count_flat_across_200_calls() {
        use std::ffi::CString;
        use std::sync::atomic::AtomicUsize;

        fn mkfifo_at(path: &Path) {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            let cpath = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: `cpath` is a valid NUL-terminated path and mode 0o600 is valid.
            #[allow(unsafe_code)]
            let rc = unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) };
            assert_eq!(rc, 0, "mkfifo failed for {}", path.display());
        }

        fn count_threads_named(names: &[&str]) -> usize {
            let Ok(entries) = fs::read_dir("/proc/self/task") else {
                return 0;
            };
            let mut count = 0;
            for entry in entries.flatten() {
                if let Ok(comm) = fs::read_to_string(entry.path().join("comm"))
                    && names.contains(&comm.trim_end_matches('\n'))
                {
                    count += 1;
                }
            }
            count
        }

        // 1. FIFOs placed where sysfs regular files belong are rejected by symlink_metadata
        // without ever opening or blocking; 200 calls leave no sysfs probe threads behind.
        let dir = tempfile::tempdir().unwrap();
        mkfifo_at(&dir.path().join("class/hwmon/hwmon0/name"));
        write(dir.path(), "class/power_supply/BAT0/type", "Battery\n");
        mkfifo_at(&dir.path().join("class/power_supply/BAT0/capacity"));
        mkfifo_at(&dir.path().join("class/drm/card0/device/vendor"));

        let missing_cmd =
            CommandSpec::argv("legion-nonexistent-nvidia-smi-probe", Vec::<&str>::new());
        let sysfs_thread_names = ["probe-cpu-temp", "probe-drm-gpus", "probe-battery"];
        for _ in 0..200 {
            let snap = probe_all(ProbeConfig {
                sys_root: dir.path().to_path_buf(),
                nvidia_gpu_cmd: missing_cmd.clone(),
                macos_battery_cmd: missing_cmd.clone(),
                timeout: Duration::from_millis(200),
            })
            .await;
            assert_eq!(snap.cpu_temperature_c, None);
            assert!(snap.gpus.is_empty());
            assert_eq!(snap.battery, None);
        }
        // Let detached probe threads finish exiting, then check only sysfs probe threads so
        // unrelated parallel tests in this process cannot cause false failures.
        let drain_deadline = Instant::now() + Duration::from_secs(5);
        while count_threads_named(&sysfs_thread_names) > 0 && Instant::now() < drain_deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let after_sysfs = count_threads_named(&sysfs_thread_names);
        assert_eq!(
            after_sysfs, 0,
            "FIFO sysfs fixture left {after_sysfs} probe thread(s) alive"
        );

        // 2. Even when a driver read genuinely blocks inside a probe slot (simulated by opening
        // a real FIFO directly), 200 calls spawn at most one thread, stay non-blocking after the
        // first timeout, and clear the stuck mark once the read unblocks.
        let stuck_fifo = dir.path().join("stuck.fifo");
        mkfifo_at(&stuck_fifo);
        let slot = ProbeSlot::new();
        let spawned = Arc::new(AtomicUsize::new(0));

        let started = Instant::now();
        for _ in 0..200 {
            let fifo_path = stuck_fifo.clone();
            let count = Arc::clone(&spawned);
            let res = run_guarded_probe(&slot, "fifo-test", Duration::from_millis(40), move || {
                count.fetch_add(1, Ordering::AcqRel);
                fs::read_to_string(&fifo_path).ok()
            })
            .await;
            assert_eq!(res, None);
        }
        assert_eq!(
            spawned.load(Ordering::Acquire),
            1,
            "stuck probe slot must spawn at most one thread across 200 calls"
        );
        assert!(
            slot.is_busy(),
            "slot must remain marked stuck while blocked"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "subsequent calls while stuck must return immediately (200x40ms would exceed 8s), took {:?}",
            started.elapsed()
        );
        let during_stuck = count_threads_named(&["probe-fifo-test"]);
        assert_eq!(
            during_stuck, 1,
            "stuck slot must hold exactly one probe-fifo-test thread, saw {during_stuck}"
        );

        // Unblock the FIFO by writing to it; the stuck mark must clear and the next call succeeds.
        fs::write(&stuck_fifo, "unblocked").unwrap();
        let unblock_deadline = Instant::now() + Duration::from_secs(10);
        while (slot.is_busy() || count_threads_named(&["probe-fifo-test"]) > 0)
            && Instant::now() < unblock_deadline
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!slot.is_busy(), "stuck mark must clear after read returns");
        assert_eq!(
            count_threads_named(&["probe-fifo-test"]),
            0,
            "probe-fifo-test thread should exit after FIFO unblocks"
        );
        let ok =
            run_guarded_probe(&slot, "fifo-test", Duration::from_secs(5), || Some(99u32)).await;
        assert_eq!(ok, Some(99));
    }

    #[test]
    fn macos_and_windows_battery_parsers_handle_all_states() {
        // Charging on AC with remaining time.
        let charging = "Now drawing from 'AC Power'\n \
             -InternalBattery-0 (id=4653155)\t84%; charging; 0:42 remaining present: true\n";
        assert_eq!(
            parse_macos_pmset_batt(charging),
            Some(Battery {
                percent: 84,
                state: BatteryState::Charging,
                on_ac: Some(true),
                time_left_s: Some(42 * 60),
            })
        );

        // Discharging on battery power with hours:minutes remaining.
        let discharging = "Now drawing from 'Battery Power'\n \
             -InternalBattery-0 (id=4653155)\t78%; discharging; 6:12 remaining present: true\n";
        assert_eq!(
            parse_macos_pmset_batt(discharging),
            Some(Battery {
                percent: 78,
                state: BatteryState::Discharging,
                on_ac: Some(false),
                time_left_s: Some(6 * 3600 + 12 * 60),
            })
        );

        // Discharging with `(no estimate)`.
        let no_est = "Now drawing from 'Battery Power'\n \
             -InternalBattery-0 (id=4653155)\t65%; discharging; (no estimate) present: true\n";
        assert_eq!(
            parse_macos_pmset_batt(no_est),
            Some(Battery {
                percent: 65,
                state: BatteryState::Discharging,
                on_ac: Some(false),
                time_left_s: None,
            })
        );

        // Fully charged on AC (`0:00 remaining` -> `time_left_s: None`).
        let full = "Now drawing from 'AC Power'\n \
             -InternalBattery-0 (id=4653155)\t100%; charged; 0:00 remaining present: true\n";
        assert_eq!(
            parse_macos_pmset_batt(full),
            Some(Battery {
                percent: 100,
                state: BatteryState::Full,
                on_ac: Some(true),
                time_left_s: None,
            })
        );

        // Finishing charge on AC.
        let finishing = "Now drawing from 'AC Power'\n \
             -InternalBattery-0 (id=4653155)\t99%; finishing charge; 0:08 remaining present: true\n";
        assert_eq!(
            parse_macos_pmset_batt(finishing),
            Some(Battery {
                percent: 99,
                state: BatteryState::Charging,
                on_ac: Some(true),
                time_left_s: Some(8 * 60),
            })
        );

        // Optimized Battery Charging / charge-limit hold: `AC attached; not charging`.
        let ac_hold = "Now drawing from 'AC Power'\n \
             -InternalBattery-0 (id=4653155)\t80%; AC attached; not charging present: true\n";
        assert_eq!(
            parse_macos_pmset_batt(ac_hold),
            Some(Battery {
                percent: 80,
                state: BatteryState::Unknown,
                on_ac: Some(true),
                time_left_s: None,
            })
        );

        // Space-separated fallback without a tab character.
        let spaced = "Now drawing from 'Battery Power'\n \
             -InternalBattery-0 (id=4653155) 52%; discharging; 2:05 remaining present: true\n";
        assert_eq!(
            parse_macos_pmset_batt(spaced),
            Some(Battery {
                percent: 52,
                state: BatteryState::Discharging,
                on_ac: Some(false),
                time_left_s: Some(2 * 3600 + 5 * 60),
            })
        );

        // Battery absent (`present: false`), desktop Mac, and external UPS only -> `None`.
        let not_present = "Now drawing from 'AC Power'\n \
             -InternalBattery-0 (id=4653155)\t0%; not charging; (no estimate) present: false\n";
        assert_eq!(parse_macos_pmset_batt(not_present), None);

        let desktop = "Now drawing from 'AC Power'\n";
        assert_eq!(parse_macos_pmset_batt(desktop), None);

        let ups_only = "Now drawing from 'AC Power'\n \
             -Back-UPS ES 750G (id=123456)\t100%; charged; 0:00 remaining present: true\n";
        assert_eq!(parse_macos_pmset_batt(ups_only), None);

        // Invalid minutes in remaining estimate are ignored without dropping the battery reading.
        let bad_time = "Now drawing from 'Battery Power'\n \
             -InternalBattery-0 (id=4653155)\t40%; discharging; 1:75 remaining present: true\n";
        assert_eq!(
            parse_macos_pmset_batt(bad_time),
            Some(Battery {
                percent: 40,
                state: BatteryState::Discharging,
                on_ac: Some(false),
                time_left_s: None,
            })
        );

        assert_eq!(
            parse_windows_power_status(0, 1, 15, 3600),
            Some(Battery {
                percent: 15,
                state: BatteryState::Discharging,
                on_ac: Some(false),
                time_left_s: Some(3600),
            })
        );
        assert_eq!(
            parse_windows_power_status(1, 8, 80, u32::MAX),
            Some(Battery {
                percent: 80,
                state: BatteryState::Charging,
                on_ac: Some(true),
                time_left_s: None,
            })
        );
        // No system battery flag (128).
        assert_eq!(parse_windows_power_status(1, 128, 255, u32::MAX), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probe_macos_battery_runs_command_parses_recorded_output_and_respects_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("pmset.txt");
        fs::write(
            &fixture,
            "Now drawing from 'Battery Power'\n \
             -InternalBattery-0 (id=4653155)\t73%; discharging; 4:30 remaining present: true\n",
        )
        .unwrap();
        let batt = probe_macos_battery(
            CommandSpec::argv("cat", [fixture.to_string_lossy().as_ref()]),
            Duration::from_secs(2),
        )
        .await;
        assert_eq!(
            batt,
            Some(Battery {
                percent: 73,
                state: BatteryState::Discharging,
                on_ac: Some(false),
                time_left_s: Some(4 * 3600 + 30 * 60),
            })
        );

        let missing = probe_macos_battery(
            CommandSpec::argv("legion-nonexistent-pmset", Vec::<&str>::new()),
            Duration::from_millis(200),
        )
        .await;
        assert_eq!(missing, None);

        let started = Instant::now();
        let slow = probe_macos_battery(
            CommandSpec::argv("sh", ["-c", "sleep 30"]),
            Duration::from_millis(120),
        )
        .await;
        assert_eq!(slow, None);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "slow macOS battery probe delayed caller past bound: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn slow_or_missing_probes_never_fail_or_delay_past_limit() {
        let missing = probe_nvidia_gpus(
            CommandSpec::argv("legion-nonexistent-nvidia-smi-probe", Vec::<&str>::new()),
            Duration::from_millis(200),
        )
        .await;
        assert!(missing.is_empty());

        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let probe_finished = Arc::new(AtomicBool::new(false));
        let probe_finished_bg = Arc::clone(&probe_finished);
        let slow = run_blocking_probe(Duration::from_millis(80), move || {
            let _ = release_rx.recv_timeout(Duration::from_secs(10));
            probe_finished_bg.store(true, Ordering::Release);
            Some(42u32)
        })
        .await;
        assert_eq!(slow, None);
        assert!(
            !probe_finished.load(Ordering::Acquire),
            "run_blocking_probe must return None while the blocked probe is still held"
        );
        let _ = release_tx.send(());

        #[cfg(unix)]
        {
            let cmd_started = Instant::now();
            let slow_cmd = probe_nvidia_gpus(
                CommandSpec::argv("sh", ["-c", "sleep 30"]),
                Duration::from_millis(120),
            )
            .await;
            assert!(slow_cmd.is_empty());
            assert!(
                cmd_started.elapsed() < Duration::from_secs(10),
                "slow command probe delayed caller for {:?}",
                cmd_started.elapsed()
            );
        }
    }
}
