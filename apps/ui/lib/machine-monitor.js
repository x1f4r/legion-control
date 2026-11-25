// Pure helpers and polling controller for the Machine Overview screen and Machines list warnings.

import {
  clampPct,
  diskUsagePct,
  formatBytePair,
  formatBytes,
  formatDuration,
  primaryDisk,
} from './format.js';
import { formatFriendlyUptime } from './activity.js';

export const OVERVIEW_POLL_MS = 5_000;
export const OVERVIEW_STALE_MS = 15_000;
export const BUSY_PCT_THRESHOLD = 60;
export const STORAGE_AMBER_FREE_PCT = 5;
export const TEMP_AMBER_C = 85;
export const BATTERY_AMBER_PCT = 15;
export const PROC_MIN_CPU_PCT = 5;
export const PROC_MIN_MEM_PCT = 5;
export const PROC_MIN_GPU_BYTES = 256 * 1000 * 1000;
export const MAX_USING_NOW_ROWS = 5;

/**
 * Returns true when processor or any graphics card utilization is strictly above 60%.
 */
export function isOverviewBusy(sysInfo) {
  if (!sysInfo || typeof sysInfo !== 'object') return false;
  if (Number.isFinite(sysInfo.cpu?.usage_pct) && sysInfo.cpu.usage_pct > BUSY_PCT_THRESHOLD) {
    return true;
  }
  if (Array.isArray(sysInfo.gpus)) {
    for (const gpu of sysInfo.gpus) {
      if (Number.isFinite(gpu?.utilization_pct) && gpu.utilization_pct > BUSY_PCT_THRESHOLD) {
        return true;
      }
    }
  }
  return false;
}

/**
 * Formats the single state line at the top of Overview: "On for 3 days · Busy" or "On for 3 days · Idle".
 */
export function formatOverviewStateLine(sysInfo) {
  const status = isOverviewBusy(sysInfo) ? 'Busy' : 'Idle';
  if (sysInfo && Number.isFinite(sysInfo.uptime_s) && sysInfo.uptime_s >= 0) {
    const up = formatFriendlyUptime(sysInfo.uptime_s);
    if (up) return `On for ${up} · ${status}`;
  }
  return status;
}

/**
 * Returns the hottest reported temperature across processor and graphics cards, or null if none exist.
 */
export function maxTemperature(sysInfo) {
  if (!sysInfo || typeof sysInfo !== 'object') return null;
  let hottest = null;
  if (Number.isFinite(sysInfo.cpu?.temperature_c)) {
    hottest = {
      tempC: sysInfo.cpu.temperature_c,
      source: 'cpu',
      sourceLabel: 'processor',
    };
  }
  if (Array.isArray(sysInfo.gpus)) {
    for (const gpu of sysInfo.gpus) {
      if (!Number.isFinite(gpu?.temperature_c)) continue;
      if (!hottest || gpu.temperature_c > hottest.tempC) {
        hottest = {
          tempC: gpu.temperature_c,
          source: 'gpu',
          sourceLabel: 'graphics',
        };
      }
    }
  }
  return hottest;
}

export function isStorageAmber(disk) {
  if (!disk || !Number.isFinite(disk.total) || disk.total <= 0) return false;
  if (!Number.isFinite(disk.available)) return false;
  return (disk.available / disk.total) * 100 < STORAGE_AMBER_FREE_PCT;
}

export function isTemperatureAmber(tempC) {
  return Number.isFinite(tempC) && tempC >= TEMP_AMBER_C;
}

export function isBatteryAmber(battery) {
  if (!battery || !Number.isFinite(battery.percent)) return false;
  if (battery.state === 'charging') return false;
  return battery.percent <= BATTERY_AMBER_PCT;
}

/**
 * Returns the single amber attention phrase for the Machines list, derived from `sys.info` already
 * held by the list, or null when nothing needs attention.
 */
export function machineAttentionLabel(sysInfo) {
  if (!sysInfo || typeof sysInfo !== 'object') return null;
  const hottest = maxTemperature(sysInfo);
  if (hottest && isTemperatureAmber(hottest.tempC)) {
    return 'Running hot';
  }
  if (isBatteryAmber(sysInfo.battery)) {
    return 'Battery low';
  }
  const disk = primaryDisk(sysInfo.disks);
  if (disk && isStorageAmber(disk.disk)) {
    return 'Storage almost full';
  }
  return null;
}

function formatBatteryDetail(battery) {
  const pct = clampPct(battery.percent);
  const time =
    Number.isFinite(battery.time_left_s) && battery.time_left_s > 0
      ? `${formatDuration(battery.time_left_s)} left`
      : null;

  if (battery.state === 'charging') {
    return time ? `${pct}% · charging, ${time}` : `${pct}% · charging`;
  }
  if (battery.state === 'full') {
    return `${pct}% · full`;
  }
  if (battery.state === 'discharging') {
    return time ? `${pct}% · ${time}` : `${pct}%`;
  }
  if (battery.on_ac === true) {
    return `${pct}% · plugged in`;
  }
  return `${pct}%`;
}

/**
 * Builds Overview meters strictly from values the machine actually reports.
 * Nothing is emitted for missing optional fields (no empty meters, no dashes).
 */
export function buildOverviewMeters(sysInfo, { technical = false } = {}) {
  if (!sysInfo || typeof sysInfo !== 'object') return [];
  const meters = [];

  // 1. Processor
  if (Number.isFinite(sysInfo.cpu?.usage_pct)) {
    const pct = clampPct(sysInfo.cpu.usage_pct);
    const loadText =
      technical && Array.isArray(sysInfo.cpu.load) && sysInfo.cpu.load.length > 0
        ? `load ${sysInfo.cpu.load[0].toFixed(1)}`
        : null;
    const coresText =
      technical && Number.isFinite(sysInfo.cpu.cores) && sysInfo.cpu.cores > 0
        ? `${sysInfo.cpu.cores} cores`
        : null;
    const detail = [`${pct}%`, loadText, coresText].filter(Boolean).join(' · ');
    meters.push({
      id: 'cpu',
      label: 'Processor',
      detail,
      pct,
      amber: false,
    });
  }

  // 2. Memory
  if (
    Number.isFinite(sysInfo.memory?.total) &&
    sysInfo.memory.total > 0 &&
    Number.isFinite(sysInfo.memory?.used)
  ) {
    const used = Math.max(0, sysInfo.memory.used);
    const total = sysInfo.memory.total;
    const pct = clampPct((used / total) * 100);
    meters.push({
      id: 'memory',
      label: 'Memory',
      detail: formatBytePair(used, total),
      pct,
      amber: false,
    });
  }

  // 3. Graphics (use and memory; one row per card when there are several)
  if (Array.isArray(sysInfo.gpus)) {
    const reportable = sysInfo.gpus.filter((gpu) => {
      if (!gpu || typeof gpu !== 'object') return false;
      const hasUtil = Number.isFinite(gpu.utilization_pct);
      const hasMem =
        Number.isFinite(gpu.memory_total) &&
        gpu.memory_total > 0 &&
        Number.isFinite(gpu.memory_used);
      return hasUtil || hasMem;
    });
    const multi = reportable.length > 1;
    reportable.forEach((gpu, idx) => {
      const hasUtil = Number.isFinite(gpu.utilization_pct);
      const hasMem =
        Number.isFinite(gpu.memory_total) &&
        gpu.memory_total > 0 &&
        Number.isFinite(gpu.memory_used);
      const utilPct = hasUtil ? clampPct(gpu.utilization_pct) : null;
      const memPct = hasMem ? clampPct((Math.max(0, gpu.memory_used) / gpu.memory_total) * 100) : null;
      const parts = [];
      if (utilPct != null) parts.push(`${utilPct}%`);
      if (hasMem) parts.push(formatBytePair(Math.max(0, gpu.memory_used), gpu.memory_total));
      if (technical && Number.isFinite(gpu.power_w)) {
        parts.push(`${Math.round(gpu.power_w)} W`);
      }
      const label = multi ? gpu.name || `Graphics ${idx + 1}` : 'Graphics';
      meters.push({
        id: `gpu-${idx}`,
        label,
        subLabel: !multi && technical && gpu.name ? gpu.name : null,
        detail: parts.join(' · '),
        pct: utilPct != null ? utilPct : memPct ?? 0,
        amber: false,
      });
    });
  }

  // 4. Storage (fullest real disk, as free space)
  const disk = primaryDisk(sysInfo.disks);
  if (disk && Number.isFinite(disk.disk?.available)) {
    const avail = Math.max(0, disk.disk.available);
    const pct = diskUsagePct(disk.disk);
    const amber = isStorageAmber(disk.disk);
    const detailParts = [`${formatBytes(avail)} free`];
    if (technical && disk.disk.mount) {
      detailParts.push(disk.disk.mount);
    }
    meters.push({
      id: 'storage',
      label: 'Storage',
      detail: detailParts.join(' · '),
      pct,
      amber,
    });
  }

  // 5. Temperature (hottest reported part)
  const hottest = maxTemperature(sysInfo);
  if (hottest) {
    const rounded = Math.round(hottest.tempC);
    const hasCpu = Number.isFinite(sysInfo.cpu?.temperature_c);
    const hasGpu = Array.isArray(sysInfo.gpus) && sysInfo.gpus.some((g) => Number.isFinite(g?.temperature_c));
    const detail =
      hasCpu && hasGpu
        ? `${rounded} °C · ${hottest.sourceLabel}`
        : `${rounded} °C`;
    meters.push({
      id: 'temperature',
      label: 'Temperature',
      detail,
      pct: clampPct(rounded),
      amber: isTemperatureAmber(hottest.tempC),
    });
  }

  // 6. Battery
  if (sysInfo.battery && Number.isFinite(sysInfo.battery.percent)) {
    const pct = clampPct(sysInfo.battery.percent);
    meters.push({
      id: 'battery',
      label: 'Battery',
      detail: formatBatteryDetail(sysInfo.battery),
      pct,
      amber: isBatteryAmber(sysInfo.battery),
    });
  }

  return meters;
}

/**
 * Earliest RFC 3339 start time across a grouped program's members, or null when unavailable.
 */
export function earliestMemberStartTime(members) {
  if (!Array.isArray(members) || members.length === 0) return null;
  let bestStr = null;
  let bestMs = Infinity;
  for (const member of members) {
    const raw = member?.start_time;
    if (!raw || typeof raw !== 'string') continue;
    const ms = Date.parse(raw);
    if (Number.isFinite(ms) && ms < bestMs) {
      bestMs = ms;
      bestStr = raw;
    }
  }
  return bestStr;
}

/**
 * Filters grouped `sys.procs` rows to up to five programs using at least 5% of the whole
 * processor, 5% of memory, or 256 MB of graphics memory, formatting each row's largest figure in
 * plain words.
 */
export function selectBusyPrograms(rows, sysInfo) {
  if (!Array.isArray(rows) || rows.length === 0) return [];
  const cores = Number.isFinite(sysInfo?.cpu?.cores) && sysInfo.cpu.cores > 0 ? sysInfo.cpu.cores : 1;
  const memTotal = Number.isFinite(sysInfo?.memory?.total) && sysInfo.memory.total > 0 ? sysInfo.memory.total : 0;
  const gpuMemTotal = Array.isArray(sysInfo?.gpus)
    ? sysInfo.gpus.reduce(
        (sum, g) => sum + (Number.isFinite(g?.memory_total) && g.memory_total > 0 ? g.memory_total : 0),
        0,
      )
    : 0;

  const qualified = [];
  for (const row of rows) {
    if (!row || typeof row !== 'object' || !row.name) continue;
    const rawCpu = Number.isFinite(row.cpu_pct) ? Math.max(0, row.cpu_pct) : 0;
    const wholeCpuPct = rawCpu / cores;
    const memBytes = Number.isFinite(row.memory) ? Math.max(0, row.memory) : 0;
    const memPct = memTotal > 0 ? (memBytes / memTotal) * 100 : 0;
    const gpuBytes = Number.isFinite(row.gpu_memory) ? Math.max(0, row.gpu_memory) : 0;
    const gpuPct = gpuMemTotal > 0 ? (gpuBytes / gpuMemTotal) * 100 : gpuBytes >= PROC_MIN_GPU_BYTES ? 50 : 0;

    const passesCpu = wholeCpuPct >= PROC_MIN_CPU_PCT;
    const passesMem = memPct >= PROC_MIN_MEM_PCT;
    const passesGpu = gpuBytes >= PROC_MIN_GPU_BYTES;
    if (!passesCpu && !passesMem && !passesGpu) continue;

    // Pick the largest figure among the qualifying metrics.
    const candidates = [];
    if (passesCpu) {
      candidates.push({
        kind: 'cpu',
        score: wholeCpuPct,
        text: `${Math.round(wholeCpuPct)}% of the processor`,
      });
    }
    if (passesMem) {
      candidates.push({
        kind: 'memory',
        score: memPct,
        text: `${formatBytes(memBytes)} of memory`,
      });
    }
    if (passesGpu) {
      candidates.push({
        kind: 'gpu',
        score: gpuPct,
        text: `${formatBytes(gpuBytes)} of graphics memory`,
      });
    }
    candidates.sort((a, b) => b.score - a.score);
    const primary = candidates[0];

    qualified.push({
      ...row,
      count: Number.isFinite(row.count) && row.count > 0 ? row.count : 1,
      wholeCpuPct,
      memPct,
      gpuMemBytes: gpuBytes > 0 ? gpuBytes : null,
      primaryKind: primary.kind,
      primaryScore: primary.score,
      primaryText: primary.text,
      earliestStartTime: earliestMemberStartTime(row.members),
    });
  }

  qualified.sort((a, b) => b.primaryScore - a.primaryScore);
  return qualified.slice(0, MAX_USING_NOW_ROWS);
}

/**
 * Non-overlapping 5-second overview poller that pauses while hidden and dims stale readings after
 * 15 seconds. Uses an injectable clock for deterministic tests.
 */
export function createOverviewPoller({
  fetchTick,
  onStaleChange,
  isVisible = () => (typeof document === 'undefined' ? true : document.visibilityState === 'visible'),
  clock = {
    now: () => Date.now(),
    setInterval: (fn, ms) => setInterval(fn, ms),
    clearInterval: (id) => clearInterval(id),
  },
  pollMs = OVERVIEW_POLL_MS,
  staleMs = OVERVIEW_STALE_MS,
}) {
  let active = false;
  let inFlight = false;
  let pollTimer = null;
  let staleTimer = null;
  let lastFreshAt = clock.now();
  let stale = false;
  let generation = 0;

  function setStale(next) {
    if (stale === next) return;
    stale = next;
    if (typeof onStaleChange === 'function') onStaleChange(stale);
  }

  function checkStale() {
    if (!active) return;
    setStale(clock.now() - lastFreshAt > staleMs);
  }

  async function runTick() {
    if (!active || inFlight || !isVisible()) return false;
    inFlight = true;
    const gen = generation;
    try {
      const ok = await fetchTick();
      if (!active || gen !== generation) return false;
      if (ok !== false) {
        lastFreshAt = clock.now();
        setStale(false);
      } else {
        checkStale();
      }
      return true;
    } catch {
      if (active && gen === generation) checkStale();
      return false;
    } finally {
      if (gen === generation) {
        inFlight = false;
      }
    }
  }

  function start({ immediate = true } = {}) {
    if (active) return;
    active = true;
    generation += 1;
    lastFreshAt = clock.now();
    setStale(false);
    pollTimer = clock.setInterval(() => {
      runTick();
    }, pollMs);
    staleTimer = clock.setInterval(() => {
      checkStale();
    }, 1000);
    if (immediate) {
      runTick();
    }
  }

  function stop() {
    active = false;
    generation += 1;
    inFlight = false;
    if (pollTimer !== null) {
      clock.clearInterval(pollTimer);
      pollTimer = null;
    }
    if (staleTimer !== null) {
      clock.clearInterval(staleTimer);
      staleTimer = null;
    }
  }

  function onVisibilityChange() {
    if (!active) return;
    if (isVisible()) {
      checkStale();
      runTick();
    }
  }

  return {
    start,
    stop,
    refreshNow: runTick,
    onVisibilityChange,
    getState() {
      return { active, inFlight, stale, lastFreshAt };
    },
  };
}
