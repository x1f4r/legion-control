// Shared folder change announcements, staleness tracking, lightweight folder cache,
// and non-overlapping coalesced refetching for live folder views.

import { parentFilePath } from './files.js';

export const DEFAULT_THROTTLE_MS = 500;
export const STALE_AFTER_MS = 10_000;

/**
 * Normalizes a folder path for comparison while preserving the machine's path syntax
 * (POSIX `/`, home `~`, and Windows `C:\`).
 */
export function normalizeFolderPath(rawPath) {
  if (rawPath == null) return '';
  const s = String(rawPath).trim();
  if (!s) return '';
  if (s === '~' || s === '/' || s === '\\') return s;
  if (/^[a-zA-Z]:[\\/]?$/.test(s)) {
    return `${s[0]}:\\`;
  }
  const trimmed = s.replace(/[\\/]+$/, '');
  if (!trimmed) return s.startsWith('\\') ? '\\' : '/';
  if (/^[a-zA-Z]:$/.test(trimmed)) return `${trimmed}\\`;
  return trimmed;
}

/**
 * Parses a transfer endpoint (`machine:/path`, `machine:C:\path`, or plain `/path`)
 * into `{ machine, path, dir }`.
 */
export function parseTransferTarget(spec, fallbackMachine = '') {
  const raw = String(spec || '').trim();
  if (!raw) return null;
  const colonIdx = raw.indexOf(':');
  const slashIdx = raw.search(/[\\/]/);
  const hasMachinePrefix =
    colonIdx > 1 && (slashIdx === -1 || colonIdx < slashIdx);
  const machine = hasMachinePrefix ? raw.slice(0, colonIdx) : String(fallbackMachine || '');
  const fullPath = hasMachinePrefix ? raw.slice(colonIdx + 1) : raw;
  const normFull = normalizeFolderPath(fullPath);
  if (!machine || !normFull) return null;
  const parent = parentFilePath(normFull);
  const dir = normalizeFolderPath(parent || normFull);
  return { machine, path: normFull, dir };
}

/**
 * Wraps an async refetch function so executions never overlap.
 * Any triggers arriving while a run is in flight coalesce into exactly one follow-up run.
 */
export function createCoalescedRefetch(runFn) {
  let inFlight = false;
  let pending = false;
  let pendingArg = undefined;
  let cancelled = false;
  let currentWaiters = [];
  let nextWaiters = [];

  async function drain(initialArg) {
    inFlight = true;
    let arg = initialArg;
    try {
      while (!cancelled) {
        const batchWaiters = currentWaiters;
        currentWaiters = [];
        try {
          const result = await runFn(arg);
          for (const w of batchWaiters) w.resolve(result);
        } catch (err) {
          for (const w of batchWaiters) w.resolve(undefined);
        }
        if (cancelled || !pending) break;
        pending = false;
        arg = pendingArg;
        pendingArg = undefined;
        currentWaiters = nextWaiters;
        nextWaiters = [];
      }
    } finally {
      inFlight = false;
      pending = false;
      pendingArg = undefined;
      for (const w of currentWaiters) w.resolve(undefined);
      for (const w of nextWaiters) w.resolve(undefined);
      currentWaiters = [];
      nextWaiters = [];
    }
  }

  return {
    trigger(arg) {
      if (cancelled) return Promise.resolve(undefined);
      return new Promise((resolve) => {
        if (inFlight) {
          pending = true;
          pendingArg = arg;
          nextWaiters.push({ resolve });
          return;
        }
        currentWaiters.push({ resolve });
        drain(arg);
      });
    },
    cancel() {
      cancelled = true;
      pending = false;
      pendingArg = undefined;
    },
    isRunning() {
      return inFlight;
    },
  };
}

/**
 * Creates a store that coordinates live folder change announcements, throttled transfer
 * updates, parent-folder staleness, and an in-memory folder listing cache.
 */
export function createFolderChangesStore({
  throttleMs = DEFAULT_THROTTLE_MS,
  staleAfterMs = STALE_AFTER_MS,
  now = () => Date.now(),
  setTimer = (fn, ms) => setTimeout(fn, ms),
  clearTimer = (id) => clearTimeout(id),
  maxCacheEntries = 40,
} = {}) {
  const homeByMachine = new Map();
  const listeners = new Set(); // { machine, path, fn }
  const lastChangedByKey = new Map();
  const lastLoadedByKey = new Map();
  const throttleSlots = new Map(); // key -> { lastEmittedAt, timer, pendingEvent }
  const cache = new Map(); // cacheKey -> entry (LRU via Map insertion order)

  function canonicalize(machine, rawPath) {
    const norm = normalizeFolderPath(rawPath);
    if (!norm) return '';
    const home = machine ? homeByMachine.get(machine) : null;
    if (!home) return norm;
    if (norm === '~') return home;
    if (norm.startsWith('~/') || norm.startsWith('~\\')) {
      const sep = home.includes('\\') && !home.includes('/') ? '\\' : '/';
      const rest = norm.slice(2).replace(/[\\/]+/g, sep);
      const base = home.endsWith(sep) ? home.slice(0, -1) : home;
      return normalizeFolderPath(`${base}${sep}${rest}`);
    }
    return norm;
  }

  function makeKey(machine, rawPath) {
    const m = String(machine || '');
    const p = canonicalize(m, rawPath);
    if (!m || !p) return '';
    return `${m}\0${p}`;
  }

  function allKeysFor(machine, rawPath) {
    const m = String(machine || '');
    const norm = normalizeFolderPath(rawPath);
    if (!m || !norm) return [];
    const canon = canonicalize(m, norm);
    const keys = [`${m}\0${canon}`];
    if (norm !== canon) keys.push(`${m}\0${norm}`);
    const home = homeByMachine.get(m);
    if (home && canon === home) {
      const tildeKey = `${m}\0~`;
      if (!keys.includes(tildeKey)) keys.push(tildeKey);
    }
    return keys;
  }

  function pathsMatch(machine, a, b) {
    const ca = canonicalize(machine, a);
    const cb = canonicalize(machine, b);
    return Boolean(ca && cb && ca === cb);
  }

  function registerHome(machine, resolvedHomePath) {
    const m = String(machine || '');
    const norm = normalizeFolderPath(resolvedHomePath);
    if (!m || !norm || norm === '~') return;
    homeByMachine.set(m, norm);
    // Carry over any change timestamp recorded under `~` before home was resolved.
    const tildeTs = lastChangedByKey.get(`${m}\0~`) || 0;
    const homeKey = `${m}\0${norm}`;
    const homeTs = lastChangedByKey.get(homeKey) || 0;
    if (tildeTs > homeTs) {
      lastChangedByKey.set(homeKey, tildeTs);
    }
  }

  function emitEvent(event) {
    for (const sub of Array.from(listeners)) {
      if (sub.machine !== event.machine) continue;
      if (pathsMatch(event.machine, sub.path, event.path)) {
        try {
          sub.fn(event);
        } catch {
          // Keep notifying remaining listeners.
        }
      }
    }
  }

  function recordAndNotify(machine, rawPath, { throttle = false, reason = 'write', isParent = false, childPath = null } = {}) {
    const m = String(machine || '');
    const norm = normalizeFolderPath(rawPath);
    if (!m || !norm) return;
    const canon = canonicalize(m, norm);
    const ts = now();
    for (const k of allKeysFor(m, norm)) {
      lastChangedByKey.set(k, ts);
    }

    const slotKey = `${m}\0${canon}`;
    const event = {
      machine: m,
      path: canon,
      rawPath: norm,
      reason,
      isParent: Boolean(isParent),
      childPath: childPath || null,
      at: ts,
    };

    let slot = throttleSlots.get(slotKey);
    if (!slot) {
      slot = { lastEmittedAt: -Infinity, timer: null, pendingEvent: null };
      throttleSlots.set(slotKey, slot);
    }

    if (!throttle || throttleMs <= 0) {
      if (slot.timer !== null) {
        clearTimer(slot.timer);
        slot.timer = null;
      }
      slot.pendingEvent = null;
      slot.lastEmittedAt = ts;
      emitEvent(event);
      return;
    }

    const elapsed = ts - slot.lastEmittedAt;
    if (elapsed >= throttleMs) {
      if (slot.timer !== null) {
        clearTimer(slot.timer);
        slot.timer = null;
      }
      slot.pendingEvent = null;
      slot.lastEmittedAt = ts;
      emitEvent(event);
      return;
    }

    slot.pendingEvent = event;
    if (slot.timer === null) {
      const waitMs = Math.max(0, throttleMs - elapsed);
      slot.timer = setTimer(() => {
        slot.timer = null;
        const pending = slot.pendingEvent;
        slot.pendingEvent = null;
        if (!pending) return;
        slot.lastEmittedAt = now();
        emitEvent(pending);
      }, waitMs);
    }
  }

  /**
   * Announces that a folder's contents changed. Also marks the parent folder stale
   * so parent entry counts, sizes, or cached views update when shown.
   */
  function changed(machine, folderPath, options = {}) {
    const m = String(machine || '');
    const norm = normalizeFolderPath(folderPath);
    if (!m || !norm) return;
    const { throttle = false, reason = 'write', includeParent = true } = options;
    recordAndNotify(m, norm, { throttle, reason, isParent: false });

    if (includeParent) {
      const canon = canonicalize(m, norm);
      const parent = normalizeFolderPath(parentFilePath(canon));
      if (parent && parent !== canon) {
        recordAndNotify(m, parent, {
          throttle,
          reason,
          isParent: true,
          childPath: canon,
        });
      }
    }
  }

  function subscribe(machine, folderPath, fn) {
    if (!machine || !folderPath || typeof fn !== 'function') return () => {};
    const entry = {
      machine: String(machine),
      path: normalizeFolderPath(folderPath),
      fn,
    };
    listeners.add(entry);
    return () => {
      listeners.delete(entry);
    };
  }

  function markLoaded(machine, folderPath, loadedAt = now()) {
    for (const k of allKeysFor(machine, folderPath)) {
      lastLoadedByKey.set(k, loadedAt);
    }
  }

  function lastChangedAt(machine, folderPath) {
    let latest = 0;
    for (const k of allKeysFor(machine, folderPath)) {
      const ts = lastChangedByKey.get(k) || 0;
      if (ts > latest) latest = ts;
    }
    return latest;
  }

  function isStale(machine, folderPath, { loadedAt = null, maxAgeMs = staleAfterMs } = {}) {
    let effectiveLoadedAt = loadedAt;
    if (effectiveLoadedAt == null) {
      for (const k of allKeysFor(machine, folderPath)) {
        const ts = lastLoadedByKey.get(k);
        if (typeof ts === 'number' && (effectiveLoadedAt == null || ts > effectiveLoadedAt)) {
          effectiveLoadedAt = ts;
        }
      }
    }
    if (typeof effectiveLoadedAt !== 'number') return true;
    const changedTs = lastChangedAt(machine, folderPath);
    if (changedTs > effectiveLoadedAt) return true;
    if (maxAgeMs != null && now() - effectiveLoadedAt > maxAgeMs) return true;
    return false;
  }

  function getCached(machine, folderPath) {
    const key = makeKey(machine, folderPath);
    if (!key) return null;
    if (!cache.has(key)) return null;
    const val = cache.get(key);
    cache.delete(key);
    cache.set(key, val);
    return val;
  }

  function setCached(machine, folderPath, snapshot) {
    const key = makeKey(machine, folderPath);
    if (!key) return;
    if (cache.has(key)) cache.delete(key);
    cache.set(key, snapshot);
    while (cache.size > maxCacheEntries) {
      const oldest = cache.keys().next().value;
      cache.delete(oldest);
    }
  }

  function deleteCached(machine, folderPath) {
    for (const k of allKeysFor(machine, folderPath)) {
      cache.delete(k);
    }
  }

  function clearCache(machine = null) {
    if (!machine) {
      cache.clear();
      return;
    }
    const prefix = `${machine}\0`;
    for (const key of Array.from(cache.keys())) {
      if (key.startsWith(prefix)) cache.delete(key);
    }
  }

  /**
   * Observes device transfer queue snapshots (from `link.transfers` / `link.onTransferEvent`)
   * and announces destination folders ONLY when an upload transitions into `done`.
   * Transfers that are `queued`, `running`, `paused`, `cancelling`, `cancelled`, or `failed`
   * announce nothing.
   */
  function observeDeviceTransfers(list, prevStates) {
    if (!Array.isArray(list)) return;
    if (!(prevStates instanceof Map)) return;
    const newlyCompletedUploads = [];
    for (const item of list) {
      if (!item || item.direction !== 'upload' || !item.id) continue;
      if (item.state === 'done' && prevStates.get(item.id) !== 'done') {
        newlyCompletedUploads.push(item);
      }
    }
    if (newlyCompletedUploads.length === 0) return;

    const stillRunningDirs = new Set();
    for (const t of list) {
      if (t?.direction === 'upload' && (t.state === 'running' || t.state === 'queued')) {
        const dir = parentFilePath(t.path) || '~';
        stillRunningDirs.add(`${t.machine}\0${canonicalize(t.machine, dir)}`);
      }
    }
    const announced = new Set();
    for (const item of newlyCompletedUploads) {
      const dir = parentFilePath(item.path) || '~';
      const key = `${item.machine}\0${canonicalize(item.machine, dir)}`;
      if (announced.has(key)) continue;
      announced.add(key);
      changed(item.machine, dir, {
        throttle: stillRunningDirs.has(key),
        reason: 'upload',
      });
    }
  }

  /**
   * Observes transfer jobs (from `job.list`, `job.status`, or fleet snapshots) and announces
   * destination (and source for move) folder changes on progress and completion.
   */
  function observeTransferJobs(jobs, seenMap) {
    if (!Array.isArray(jobs) || !(seenMap instanceof Map)) return;
    for (const job of jobs) {
      if (!job || job.kind !== 'transfer' || !job.id) continue;
      const key = `${job.machine || ''}:${job.id}`;
      const prev = seenMap.get(key);
      const filesDone = job.progress?.files_done ?? 0;

      if (prev) {
        const justFinished = job.state === 'succeeded' && prev.state !== 'succeeded';
        const progressed =
          (job.state === 'running' || job.state === 'starting') && filesDone > (prev.filesDone || 0);
        if (justFinished || progressed) {
          const throttle = !justFinished;
          const dest = parseTransferTarget(job.to, job.destMachine || job.machine);
          if (dest) {
            changed(dest.machine, dest.dir, {
              throttle,
              reason: justFinished ? 'transfer' : 'transfer-progress',
            });
          }
          if (job.move && job.from) {
            const src = parseTransferTarget(job.from, job.srcMachine || job.machine);
            if (src) {
              changed(src.machine, src.dir, {
                throttle,
                reason: justFinished ? 'transfer' : 'transfer-progress',
              });
            }
          }
        }
      }
      seenMap.set(key, { state: job.state, filesDone });
    }
  }

  function dispose() {
    for (const slot of throttleSlots.values()) {
      if (slot.timer !== null) {
        clearTimer(slot.timer);
        slot.timer = null;
      }
      slot.pendingEvent = null;
    }
    throttleSlots.clear();
    listeners.clear();
  }

  return {
    registerHome,
    canonicalize,
    changed,
    subscribe,
    markLoaded,
    lastChangedAt,
    isStale,
    getCached,
    setCached,
    deleteCached,
    clearCache,
    observeDeviceTransfers,
    observeTransferJobs,
    dispose,
  };
}
