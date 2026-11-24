// Progressive fleet loading, independent per-machine timeouts, and non-secret local cache.

import { firstSentence } from './format.js';

export const PING_TIMEOUT_S = 4;
export const PING_CLIENT_TIMEOUT_MS = 6_000;
export const DETAIL_TIMEOUT_MS = 5_000;
export const MAX_CONCURRENT_PINGS = 8;

function makeTimeoutError(timeoutMs, machine) {
  const secs = Math.max(1, Math.round(timeoutMs / 1000));
  return {
    code: 'unreachable',
    message: `Timed out waiting for ${machine || 'machine'} after ${secs} s.`,
    hint: null,
    machine: machine || null,
  };
}

function isPairingError(err) {
  const reason = err?.details?.reason;
  return err?.code === 'denied' && (reason === 'auth_failed' || reason === 'host_key_mismatch');
}

/**
 * Calls a tool with an independent client-side timeout while respecting a parent AbortSignal.
 */
export async function callWithTimeout(link, tool, args, options = {}, timeoutMs = DETAIL_TIMEOUT_MS) {
  const parentSignal = options?.signal;
  if (parentSignal?.aborted) {
    throw {
      code: 'cancelled',
      message: 'The call was cancelled.',
      hint: null,
      machine: options?.on || null,
    };
  }

  const ctrl = new AbortController();
  let timedOut = false;

  function onParentAbort() {
    ctrl.abort();
  }
  if (parentSignal) {
    parentSignal.addEventListener('abort', onParentAbort, { once: true });
  }

  const timer =
    timeoutMs > 0
      ? setTimeout(() => {
          timedOut = true;
          ctrl.abort();
        }, timeoutMs)
      : null;

  try {
    return await link.call(tool, args, { ...options, signal: ctrl.signal });
  } catch (err) {
    if (timedOut) {
      throw makeTimeoutError(timeoutMs, typeof options?.on === 'string' ? options.on : null);
    }
    throw err;
  } finally {
    if (timer !== null) clearTimeout(timer);
    if (parentSignal) parentSignal.removeEventListener('abort', onParentAbort);
  }
}

export function sortMachines(machines) {
  const rank = (m) => {
    if (m.thisNode) return 0;
    if (m.status === 'answering' || m.reachable) return 1;
    if (m.status === 'asking' || m.asking) return 2;
    return 3;
  };
  return [...machines].sort((a, b) => {
    const ra = rank(a);
    const rb = rank(b);
    if (ra !== rb) return ra - rb;
    return a.name < b.name ? -1 : a.name > b.name ? 1 : 0;
  });
}

export function collectRunningJobs(machines) {
  const jobs = [];
  for (const m of machines) {
    if (!m.reachable || !Array.isArray(m.jobs)) continue;
    for (const job of m.jobs) {
      jobs.push({ ...job, machine: m.name });
    }
  }
  jobs.sort((a, b) => String(b.created || '').localeCompare(String(a.created || '')));
  return jobs;
}

/**
 * Strips a fleet snapshot down to non-secret machine metadata safe to store in localStorage.
 */
export function serializeFleetCache(node, machines, now = Date.now()) {
  const safeMachines = (machines || [])
    .filter((m) => m && typeof m.name === 'string' && m.name)
    .map((m) => ({
      name: m.name,
      thisNode: Boolean(m.thisNode),
      os: m.os || null,
      arch: m.arch || null,
      system: m.system || null,
      version: m.version || null,
      canWake: Boolean(m.canWake),
      reachable: Boolean(m.reachable),
      lastSeenAt: typeof m.lastSeenAt === 'number' ? m.lastSeenAt : null,
    }));
  return {
    node: node || null,
    savedAt: now,
    machines: safeMachines,
  };
}

/**
 * Restores a cached fleet summary as an immediate initial snapshot while live checks run.
 */
export function hydrateFleetCache(cached, expectedNode = null) {
  if (!cached || typeof cached !== 'object' || !Array.isArray(cached.machines)) return null;
  if (expectedNode && cached.node && cached.node !== expectedNode) return null;
  if (cached.machines.length === 0) return null;

  const machines = cached.machines
    .filter((m) => m && typeof m.name === 'string' && m.name)
    .map((m) => ({
      name: m.name,
      thisNode: Boolean(m.thisNode),
      status: 'asking',
      reachable: false,
      asking: true,
      fromCache: true,
      reason: null,
      error: null,
      busy: false,
      busyReasons: [],
      runningJobs: 0,
      jobs: [],
      os: m.os || null,
      arch: m.arch || null,
      version: m.version || null,
      system: m.system || null,
      latencyMs: null,
      canWake: Boolean(m.canWake),
      lastSeenAt: typeof m.lastSeenAt === 'number' ? m.lastSeenAt : null,
      sys: null,
    }));

  if (machines.length === 0) return null;
  return {
    machines: sortMachines(machines),
    runningJobs: [],
    cachedAt: typeof cached.savedAt === 'number' ? cached.savedAt : null,
    stale: true,
  };
}

function makeAskingEntry(name, isLocal, listed, prev) {
  const keepLive = prev && !prev.fromCache && prev.status === 'answering';
  return {
    name,
    thisNode: isLocal,
    status: keepLive ? 'answering' : 'asking',
    reachable: keepLive ? true : false,
    asking: !keepLive,
    fromCache: Boolean(prev?.fromCache && !keepLive),
    reason: null,
    error: null,
    busy: keepLive ? Boolean(prev.busy) : false,
    busyReasons: keepLive ? prev.busyReasons || [] : [],
    runningJobs: keepLive ? prev.runningJobs || 0 : 0,
    jobs: keepLive ? prev.jobs || [] : [],
    os: prev?.os || null,
    arch: prev?.arch || null,
    version: prev?.version || null,
    system: prev?.system || null,
    latencyMs: keepLive ? prev.latencyMs ?? null : null,
    canWake: Boolean(
      listed
        ? listed.mac || (Array.isArray(listed.wake_via) && listed.wake_via.length > 0)
        : prev?.canWake,
    ),
    lastSeenAt: typeof prev?.lastSeenAt === 'number' ? prev.lastSeenAt : null,
    sys: keepLive ? prev.sys || null : null,
  };
}

async function runPool(items, concurrency, worker) {
  const queue = [...items];
  const count = Math.min(Math.max(1, concurrency), queue.length);
  const runners = [];
  for (let i = 0; i < count; i += 1) {
    runners.push(
      (async () => {
        while (queue.length > 0) {
          const item = queue.shift();
          await worker(item);
        }
      })(),
    );
  }
  await Promise.all(runners);
}

/**
 * Progressively refreshes the fleet:
 * 1. Immediately queries this node (local-only, no `on`) and `fleet.list` in parallel.
 * 2. Paints this node as soon as it answers, and lists every configured remote as `asking`.
 * 3. Sends one `fleet.ping` per remote machine in parallel; only answering machines receive
 *    per-machine `sys.info`, `job.list`, and `power.status` calls, each with its own timeout.
 */
export async function refreshFleetProgressive({
  link,
  localNode,
  initialMachines = [],
  cachedAt = null,
  signal,
  onUpdate,
  onSettled,
  now = () => Date.now(),
  pingTimeoutS = PING_TIMEOUT_S,
  pingClientTimeoutMs = PING_CLIENT_TIMEOUT_MS,
  detailTimeoutMs = DETAIL_TIMEOUT_MS,
  maxConcurrentPings = MAX_CONCURRENT_PINGS,
}) {
  const map = new Map();
  for (const m of initialMachines || []) {
    if (m && m.name) map.set(m.name, { ...m });
  }

  let here = localNode;
  let activeCachedAt = cachedAt;

  if (here && !map.has(here)) {
    map.set(here, makeAskingEntry(here, true, null, null));
  }

  function snapshot(settled = false) {
    const machines = sortMachines(Array.from(map.values()));
    return {
      machines,
      runningJobs: collectRunningJobs(machines),
      cachedAt: settled ? null : activeCachedAt,
      stale: settled ? false : Boolean(activeCachedAt),
    };
  }

  function emit(settled = false) {
    if (signal?.aborted) return;
    if (typeof onUpdate === 'function') onUpdate(snapshot(settled));
  }

  function updateMachine(name, updater) {
    const prev = map.get(name) || makeAskingEntry(name, name === here, null, null);
    const next = updater(prev);
    map.set(name, next);
    emit(false);
    return next;
  }

  let fatalError = null;
  const capturePairingError = (fallback) => (err) => {
    if (isPairingError(err) && !fatalError) fatalError = err;
    return fallback;
  };

  async function fetchMachineDetails(name, isLocal) {
    const on = isLocal ? undefined : name;
    const onErr = isLocal ? capturePairingError(undefined) : () => {};
    const tasks = [
      callWithTimeout(link, 'sys.info', {}, { on, signal }, detailTimeoutMs)
        .then((sys) => {
          if (signal?.aborted || !sys) return;
          updateMachine(name, (prev) => ({
            ...prev,
            status: 'answering',
            reachable: true,
            asking: false,
            fromCache: false,
            error: null,
            reason: null,
            lastSeenAt: now(),
            os: sys.os?.family || prev.os,
            arch: sys.os?.arch || prev.arch,
            sys,
          }));
        })
        .catch(onErr),
      callWithTimeout(link, 'job.list', { state: 'running', limit: 50 }, { on, signal }, detailTimeoutMs)
        .then((res) => {
          if (signal?.aborted || !res) return;
          const jobs = Array.isArray(res.jobs) ? res.jobs : [];
          updateMachine(name, (prev) => ({
            ...prev,
            jobs,
            runningJobs: jobs.length,
            busy: jobs.length > 0 || (prev.busyReasons?.length || 0) > 0,
          }));
        })
        .catch(onErr),
      callWithTimeout(link, 'power.status', {}, { on, signal }, detailTimeoutMs)
        .then((power) => {
          if (signal?.aborted || !power) return;
          const busyReasons = power?.busy?.state === 'busy' ? power.busy.reasons || [] : [];
          updateMachine(name, (prev) => ({
            ...prev,
            system: power.system || prev.system,
            busyReasons,
            busy: (prev.runningJobs || 0) > 0 || busyReasons.length > 0,
          }));
        })
        .catch(onErr),
    ];

    if (isLocal) {
      tasks.push(
        callWithTimeout(link, 'meta.version', {}, { signal }, detailTimeoutMs)
          .then((ver) => {
            if (signal?.aborted || !ver) return;
            updateMachine(name, (prev) => ({
              ...prev,
              status: 'answering',
              reachable: true,
              asking: false,
              fromCache: false,
              lastSeenAt: now(),
              os: prev.os || ver.os || null,
              arch: prev.arch || ver.arch || null,
              version: ver.version || prev.version,
            }));
          })
          .catch(onErr),
      );
    }

    await Promise.all(tasks);
  }

  // Kick off local node details and fleet.list concurrently without waiting for any remote machine.
  const localPromise = fetchMachineDetails(here, true);
  const listPromise = callWithTimeout(link, 'fleet.list', {}, { signal }, detailTimeoutMs).catch(
    capturePairingError({ node: here, machines: [] }),
  );

  const fleetList = await listPromise;
  if (signal?.aborted) return null;
  if (fatalError) throw fatalError;

  here = fleetList?.node || here;
  const listedMap = new Map();
  for (const m of fleetList?.machines || []) {
    if (m && m.name) listedMap.set(m.name, m);
  }
  if (here && !listedMap.has(here)) {
    listedMap.set(here, { name: here, this_node: true, systems: [], wake_via: [] });
  }

  // Reconcile the machine map with fleet.toml and immediately show remotes as `asking`.
  for (const existing of Array.from(map.keys())) {
    if (!listedMap.has(existing)) map.delete(existing);
  }
  const remoteNames = [];
  for (const [name, listed] of listedMap.entries()) {
    const isLocal = name === here || Boolean(listed.this_node);
    const prev = map.get(name);
    if (isLocal) {
      map.set(name, {
        ...(prev || makeAskingEntry(name, true, listed, null)),
        thisNode: true,
        canWake: Boolean(listed.mac || (listed.wake_via && listed.wake_via.length > 0)),
      });
    } else {
      map.set(name, makeAskingEntry(name, false, listed, prev));
      remoteNames.push(name);
    }
  }
  emit(false);

  // Probe each remote machine once; only machines that answer get detail calls.
  const remotesPromise = runPool(remoteNames, maxConcurrentPings, async (name) => {
    if (signal?.aborted) return;
    let pingEntry = null;
    let pingErr = null;
    try {
      const res = await callWithTimeout(
        link,
        'fleet.ping',
        { machines: [name], timeout_s: pingTimeoutS },
        { signal },
        pingClientTimeoutMs,
      );
      const list = Array.isArray(res?.results) ? res.results : [];
      pingEntry = list.find((r) => r.machine === name) || list[0] || null;
    } catch (err) {
      pingErr = err;
    }

    if (signal?.aborted) return;

    if (!pingEntry || !pingEntry.reachable) {
      const error = pingEntry?.error || pingErr || {
        code: 'unreachable',
        message: 'Did not answer.',
        hint: null,
        machine: name,
      };
      updateMachine(name, (prev) => ({
        ...prev,
        status: 'offline',
        reachable: false,
        asking: false,
        fromCache: false,
        error,
        reason: firstSentence(error.message) || 'Did not answer',
        sys: null,
        jobs: [],
        runningJobs: 0,
        busy: false,
        busyReasons: [],
      }));
      return;
    }

    updateMachine(name, (prev) => ({
      ...prev,
      status: 'answering',
      reachable: true,
      asking: false,
      fromCache: false,
      error: null,
      reason: null,
      lastSeenAt: now(),
      os: pingEntry.os || prev.os,
      arch: pingEntry.arch || prev.arch,
      version: pingEntry.version || prev.version,
      system: pingEntry.system || prev.system,
      latencyMs: pingEntry.latency_ms ?? null,
    }));

    await fetchMachineDetails(name, false);
  });

  await Promise.all([localPromise, remotesPromise]);
  if (signal?.aborted) return null;
  if (fatalError) throw fatalError;

  activeCachedAt = null;
  const finalSnap = snapshot(true);
  if (typeof onUpdate === 'function') onUpdate(finalSnap);
  if (typeof onSettled === 'function') onSettled(finalSnap);
  return finalSnap;
}
