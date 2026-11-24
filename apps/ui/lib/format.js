// Pure formatting helpers for bytes, durations, times, disk selection, and machine metrics.

export function clampPct(value) {
  if (!Number.isFinite(value)) return 0;
  return Math.max(0, Math.min(100, Math.round(value)));
}

export function meterLevel(pct) {
  if (!Number.isFinite(pct)) return '';
  if (pct >= 90) return 'full';
  if (pct >= 70) return 'high';
  return '';
}

export function formatBytes(bytes) {
  if (!Number.isFinite(bytes) || bytes < 0) return '0 B';
  if (bytes < 1024) return `${Math.round(bytes)} B`;
  const units = ['KB', 'MB', 'GB', 'TB', 'PB'];
  let value = bytes;
  let index = -1;
  while (value >= 1000 && index < units.length - 1) {
    value /= 1000;
    index += 1;
  }
  const rounded = value >= 10 ? Math.round(value) : Math.round(value * 10) / 10;
  return `${rounded} ${units[index]}`;
}

export function formatBytePair(used, total) {
  if (!Number.isFinite(used) || !Number.isFinite(total) || total <= 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB', 'PB'];
  let scale = 1;
  let unit = 0;
  while (total / scale >= 1000 && unit < units.length - 1) {
    scale *= 1000;
    unit += 1;
  }
  const fmt = (n) => {
    const v = n / scale;
    return v >= 10 ? String(Math.round(v)) : String(Math.round(v * 10) / 10);
  };
  if (used / scale < 1 && unit > 0) {
    return `${formatBytes(used)} of ${formatBytes(total)}`;
  }
  return `${fmt(used)} of ${fmt(total)} ${units[unit]}`;
}

export function formatDuration(seconds) {
  if (!Number.isFinite(seconds) || seconds < 0) return '0 s';
  const total = Math.floor(seconds);
  if (total < 60) return `${total} s`;
  const days = Math.floor(total / 86400);
  const hours = Math.floor((total % 86400) / 3600);
  const mins = Math.floor((total % 3600) / 60);
  if (days >= 2) return `${days} days`;
  if (days === 1) return hours > 0 ? `1 day ${hours} h` : '1 day';
  if (hours > 0) return mins > 0 ? `${hours} h ${mins} min` : `${hours} h`;
  return `${mins} min`;
}

export function formatDurationMs(ms) {
  if (!Number.isFinite(ms) || ms < 0) return '0 ms';
  if (ms < 1000) return `${Math.round(ms)} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(ms < 10_000 ? 1 : 0)} s`;
  return formatDuration(ms / 1000);
}

export function formatElapsed(seconds) {
  if (!Number.isFinite(seconds) || seconds < 0) return '0:00';
  const total = Math.floor(seconds);
  const hours = Math.floor(total / 3600);
  const mins = Math.floor((total % 3600) / 60);
  const secs = total % 60;
  const pad = (n) => String(n).padStart(2, '0');
  if (hours > 0) return `${hours}:${pad(mins)}:${pad(secs)}`;
  return `${mins}:${pad(secs)}`;
}

export function formatRelativeTime(value, nowMs = Date.now()) {
  if (!value) return '';
  const ts = typeof value === 'number' ? value : Date.parse(value);
  if (!Number.isFinite(ts)) return '';
  const diffSec = Math.max(0, Math.floor((nowMs - ts) / 1000));
  if (diffSec < 45) return 'just now';
  if (diffSec < 3600) return `${Math.max(1, Math.round(diffSec / 60))} min ago`;
  if (diffSec < 86400) return `${Math.round(diffSec / 3600)} h ago`;
  const days = Math.round(diffSec / 86400);
  return days === 1 ? '1 day ago' : `${days} days ago`;
}

export function splitFirstSentence(text) {
  if (!text) return { first: '', rest: '' };
  const clean = String(text).replace(/\s+/g, ' ').trim();
  const match = clean.match(/^(.+?[.!?])(?:\s+([\s\S]+))?$/);
  if (!match) return { first: clean, rest: '' };
  return { first: match[1].trim(), rest: (match[2] || '').trim() };
}

export function firstSentence(text) {
  return splitFirstSentence(text).first;
}

export function tokenizeCode(text) {
  if (!text) return [];
  const str = String(text);
  const parts = [];
  const regex = /`([^`\n]+)`/g;
  let last = 0;
  let match;
  while ((match = regex.exec(str)) !== null) {
    if (match.index > last) {
      parts.push({ code: false, text: str.slice(last, match.index) });
    }
    parts.push({ code: true, text: match[1] });
    last = match.index + match[0].length;
  }
  if (last < str.length) {
    parts.push({ code: false, text: str.slice(last) });
  }
  return parts;
}

export function osLabel(family) {
  if (!family) return 'Unknown';
  const lower = String(family).toLowerCase();
  if (lower === 'linux') return 'Linux';
  if (lower === 'macos' || lower === 'darwin') return 'macOS';
  if (lower === 'windows') return 'Windows';
  return family;
}

const IGNORED_MOUNTS = new Set(['/efi', '/boot', '/boot/efi', '/boot/firmware']);

export function realDisks(disks) {
  if (!Array.isArray(disks)) return [];
  const valid = disks.filter((d) => d && Number.isFinite(d.total) && d.total > 0);
  const primary = valid.filter((d) => !IGNORED_MOUNTS.has(d.mount));
  return primary.length > 0 ? primary : valid;
}

export function diskUsagePct(disk) {
  if (!disk || !Number.isFinite(disk.total) || disk.total <= 0) return 0;
  const avail = Number.isFinite(disk.available) ? disk.available : 0;
  const used = Math.max(0, disk.total - avail);
  return clampPct((used / disk.total) * 100);
}

export function primaryDisk(disks) {
  const candidates = realDisks(disks);
  if (candidates.length === 0) return null;
  let fullest = candidates[0];
  let fullestPct = diskUsagePct(fullest);
  for (let i = 1; i < candidates.length; i += 1) {
    const pct = diskUsagePct(candidates[i]);
    if (pct > fullestPct) {
      fullest = candidates[i];
      fullestPct = pct;
    }
  }
  return {
    disk: fullest,
    used: Math.max(0, fullest.total - (fullest.available ?? 0)),
    total: fullest.total,
    pct: fullestPct,
  };
}

export function machineMetrics(sysInfo) {
  if (!sysInfo) return null;
  const cpuPct = clampPct(sysInfo.cpu?.usage_pct ?? 0);
  const memTotal = sysInfo.memory?.total ?? 0;
  const memUsed = sysInfo.memory?.used ?? 0;
  const memPct = memTotal > 0 ? clampPct((memUsed / memTotal) * 100) : 0;
  const disk = primaryDisk(sysInfo.disks);
  const diskPct = disk ? disk.pct : 0;
  return {
    cpuPct,
    memPct,
    diskPct,
    memUsed,
    memTotal,
    disk,
    gpus: Array.isArray(sysInfo.gpus)
      ? sysInfo.gpus.map((gpu) => ({
          name: gpu.name,
          used: gpu.memory_used ?? 0,
          total: gpu.memory_total ?? 0,
          pct: gpu.memory_total > 0 ? clampPct(((gpu.memory_used ?? 0) / gpu.memory_total) * 100) : 0,
          utilizationPct: clampPct(gpu.utilization_pct ?? 0),
        }))
      : [],
  };
}

/**
 * Extracts a short human label for one `power.status` busy reason (never raw `detail`).
 */
export function busyReasonLabel(reason) {
  if (typeof reason === 'string') {
    return reason.trim();
  }
  if (!reason || typeof reason !== 'object') {
    return '';
  }
  const source = typeof reason.source === 'string' ? reason.source.trim() : '';
  const name = typeof reason.name === 'string' ? reason.name.trim() : '';
  if (source === 'job') {
    if (name) return name;
    const command = typeof reason.command === 'string' ? reason.command.trim() : '';
    if (command) {
      return command.length > 32 ? `${command.slice(0, 31).trimEnd()}…` : command;
    }
    const id = typeof reason.id === 'string' ? reason.id.trim() : '';
    if (id) return id;
    return 'job';
  }
  return name || source || '';
}

/**
 * Formats a compact list-row phrase for busy reasons (`busy: agents` or `busy: agents +1`).
 */
export function formatBusyReasonsShort(reasons) {
  if (!Array.isArray(reasons) || reasons.length === 0) return '';
  const labels = reasons.map(busyReasonLabel).filter(Boolean);
  if (labels.length === 0) return '';
  if (labels.length === 1) return `busy: ${labels[0]}`;
  return `busy: ${labels[0]} +${labels.length - 1}`;
}

/**
 * Formats one busy reason as a full detail line (`name: detail`) for Overview and Power views.
 */
export function formatBusyReasonLine(reason) {
  if (typeof reason === 'string') {
    return reason.trim();
  }
  if (!reason || typeof reason !== 'object') {
    return '';
  }
  const label = busyReasonLabel(reason);
  const detail = typeof reason.detail === 'string' ? reason.detail.trim() : '';
  if (label && detail) return `${label}: ${detail}`;
  return label || detail || '';
}

/**
 * Formats all busy reasons as non-empty full detail lines.
 */
export function formatBusyReasonLines(reasons) {
  if (!Array.isArray(reasons) || reasons.length === 0) return [];
  return reasons.map(formatBusyReasonLine).filter(Boolean);
}

/**
 * Formats the short, plain subtitle for an offline machine row in the Machines list.
 */
export function formatOfflineSubtext(lastSeenAt, nowMs = Date.now()) {
  if (lastSeenAt) {
    const seen = formatRelativeTime(lastSeenAt, nowMs);
    if (seen) return `Did not answer · last seen ${seen}`;
  }
  return 'Did not answer';
}
