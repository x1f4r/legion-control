// Progressive fleet loading, independent per-machine timeouts, and non-secret local cache.

import { firstSentence } from './format.js';
import {
  isLocalConnectionName,
  isPendingLocalName,
  isRealMachineName,
  PENDING_LOCAL_MACHINE_NAME,
} from './local-machine.js';

export const PING_TIMEOUT_S = 4;
export const PING_CLIENT_TIMEOUT_MS = 6_000;
export const DETAIL_TIMEOUT_MS = 5_000;
export const MAX_CONCURRENT_PINGS = 8;
export const OFFLINE_RECHECK_MS = 2 * 60 * 1000;

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
  return (
    err?.code === 'denied' &&
    (reason === 'device_removed_or_unknown' ||
      reason === 'auth_failed' ||
      reason === 'host_key_mismatch')
  );
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
    if (m.isLocalComputer) return 0;
    if (m.thisNode) return 1;
    if (m.status === 'answering' || m.reachable) return 2;
    if (m.status === 'asking' || m.asking) return 3;
    return 4;
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
    .filter((m) => m && isRealMachineName(m.name))
    .map((m) => ({
      name: m.name,
      thisNode: Boolean(m.thisNode),
      isLocalComputer: Boolean(m.isLocalComputer),
      os: m.os || null,
      arch: m.arch || null,
      system: m.system || null,
      version: m.version || null,
      build: m.build || null,
      canWake: Boolean(m.canWake),
      reachable: Boolean(m.reachable),
      lastSeenAt: typeof m.lastSeenAt === 'number' ? m.lastSeenAt : null,
      lastCheckedAt: typeof m.lastCheckedAt === 'number' ? m.lastCheckedAt : now,
    }));
  return {
    node: isRealMachineName(node) ? node : null,
    savedAt: now,
    machines: safeMachines,
  };
}

/**
 * Restores a cached fleet summary as an immediate initial snapshot while live checks run.
 * Machines known to be off stay calmly `offline` (never `asking…`).
 */
export function hydrateFleetCache(cached, expectedNode = null) {
  if (!cached || typeof cached !== 'object' || !Array.isArray(cached.machines)) return null;
  if (expectedNode && !isRealMachineName(expectedNode)) return null;
  if (expectedNode && cached.node && cached.node !== expectedNode) return null;
  if (cached.machines.length === 0) return null;

  const machines = cached.machines
    .filter((m) => m && isRealMachineName(m.name))
    .map((m) => {
      const wasOff = m.reachable === false && !m.thisNode;
      return {
        name: m.name,
        thisNode: Boolean(m.thisNode),
        isLocalComputer: Boolean(m.isLocalComputer),
        nodeNamePending: false,
        status: wasOff ? 'offline' : 'asking',
        reachable: false,
        asking: !wasOff,
        fromCache: true,
        reason: null,
        error: null,
        busy: false,
        busyState: null,
        busyReasons: [],
        runningJobs: 0,
        jobs: [],
        os: m.os || null,
        arch: m.arch || null,
        version: m.version || null,
        build: m.build || null,
        system: m.system || null,
        latencyMs: null,
        canWake: Boolean(m.canWake),
        lastSeenAt: typeof m.lastSeenAt === 'number' ? m.lastSeenAt : null,
        lastCheckedAt:
          typeof m.lastCheckedAt === 'number'
            ? m.lastCheckedAt
            : typeof cached.savedAt === 'number'
              ? cached.savedAt
              : null,
        sys: null,
      };
    });

  if (machines.length === 0) return null;
  return {
    machines: sortMachines(machines),
    runningJobs: [],
    cachedAt: typeof cached.savedAt === 'number' ? cached.savedAt : null,
    stale: true,
  };
}

function makeEntry(name, isLocal, listed, prev, { isLocalComputer = false, nodeNamePending = false } = {}) {
  const keepLive = prev && !prev.fromCache && prev.status === 'answering';
  const keepOff = prev && prev.status === 'offline' && !isLocal;
  return {
    name,
    thisNode: isLocal,
    isLocalComputer: Boolean(isLocal && (isLocalComputer || prev?.isLocalComputer)),
    nodeNamePending: Boolean(isLocal && nodeNamePending),
    status: keepLive ? 'answering' : keepOff ? 'offline' : 'asking',
    reachable: keepLive ? true : false,
    asking: !keepLive && !keepOff,
    fromCache: Boolean(prev?.fromCache && !keepLive),
    reason: keepOff ? prev.reason || null : null,
    error: keepOff ? prev.error || null : null,
    busy: keepLive ? Boolean(prev.busy) : false,
    busyState: keepLive ? prev.busyState || null : null,
    busyReasons: keepLive ? prev.busyReasons || [] : [],
    runningJobs: keepLive ? prev.runningJobs || 0 : 0,
    jobs: keepLive ? prev.jobs || [] : [],
    os: prev?.os || null,
    arch: prev?.arch || null,
    version: prev?.version || null,
    build: prev?.build || null,
    system: prev?.system || null,
    latencyMs: keepLive ? prev.latencyMs ?? null : null,
    canWake: Boolean(
      listed
        ? listed.mac || (Array.isArray(listed.wake_via) && listed.wake_via.length > 0)
        : prev?.canWake,
    ),
    lastSeenAt: typeof prev?.lastSeenAt === 'number' ? prev.lastSeenAt : null,
    lastCheckedAt: typeof prev?.lastCheckedAt === 'number' ? prev.lastCheckedAt : null,
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
  isLocalComputer = false,
  nodeNamePending = false,
  initialMachines = [],
  cachedAt = null,
  signal,
  onUpdate,
  onSettled,
  onLocalNodeResolved = null,
  onActiveNodeChanged,
  now = () => Date.now(),
  pingTimeoutS = PING_TIMEOUT_S,
  pingClientTimeoutMs = PING_CLIENT_TIMEOUT_MS,
  detailTimeoutMs = DETAIL_TIMEOUT_MS,
  maxConcurrentPings = MAX_CONCURRENT_PINGS,
  forceOffline = false,
  checkOffline = true,
  offlineRecheckMs = OFFLINE_RECHECK_MS,
}) {
  const map = new Map();
  for (const m of initialMachines || []) {
    if (m && m.name && !isLocalConnectionName(m.name)) map.set(m.name, { ...m });
  }

  let localIsDesktop = Boolean(isLocalComputer || isLocalConnectionName(localNode));
  let localPending = Boolean(
    nodeNamePending || isLocalConnectionName(localNode) || isPendingLocalName(localNode),
  );
  let here = localPending ? PENDING_LOCAL_MACHINE_NAME : localNode;
  let activeCachedAt = cachedAt;

  if (here && !map.has(here)) {
    map.set(
      here,
      makeEntry(here, true, null, null, {
        isLocalComputer: localIsDesktop,
        nodeNamePending: localPending,
      }),
    );
  } else if (here && map.has(here)) {
    const prev = map.get(here);
    map.set(here, {
      ...prev,
      thisNode: true,
      isLocalComputer: Boolean(localIsDesktop || prev.isLocalComputer),
      nodeNamePending: localPending,
    });
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

  function settleLocalName(candidate) {
    if (!localPending || !isRealMachineName(candidate)) return false;
    const realName = candidate.trim();
    const prevKey = here;
    const pendingEntry = map.get(prevKey);
    const existingReal = map.get(realName);
    if (prevKey && prevKey !== realName) {
      map.delete(prevKey);
    }
    const base = existingReal || pendingEntry || makeEntry(realName, true, null, null);
    const reachable = Boolean(pendingEntry?.reachable || existingReal?.reachable);
    const status =
      pendingEntry?.status === 'answering' || existingReal?.status === 'answering'
        ? 'answering'
        : pendingEntry?.status || existingReal?.status || 'asking';
    map.set(realName, {
      ...base,
      ...(pendingEntry || {}),
      name: realName,
      thisNode: true,
      isLocalComputer: localIsDesktop,
      nodeNamePending: false,
      status,
      reachable,
      asking: !reachable && status === 'asking',
      os: pendingEntry?.os || existingReal?.os || null,
      arch: pendingEntry?.arch || existingReal?.arch || null,
      version: pendingEntry?.version || existingReal?.version || null,
      build: pendingEntry?.build || existingReal?.build || null,
      system: pendingEntry?.system || existingReal?.system || null,
      sys: pendingEntry?.sys || existingReal?.sys || null,
      jobs: pendingEntry?.jobs?.length ? pendingEntry.jobs : existingReal?.jobs || [],
      runningJobs: Math.max(pendingEntry?.runningJobs || 0, existingReal?.runningJobs || 0),
      canWake: Boolean(pendingEntry?.canWake || existingReal?.canWake),
      lastSeenAt: Math.max(pendingEntry?.lastSeenAt || 0, existingReal?.lastSeenAt || 0) || null,
      lastCheckedAt:
        Math.max(pendingEntry?.lastCheckedAt || 0, existingReal?.lastCheckedAt || 0) || null,
    });
    here = realName;
    localPending = false;
    if (typeof link?.setLocalMachineName === 'function') {
      link.setLocalMachineName(realName);
    }
    if (typeof onLocalNodeResolved === 'function') {
      onLocalNodeResolved(realName);
    }
    emit(false);
    return true;
  }

  function updateMachine(name, updater) {
    const key = name;
    const isSelected = key === here;
    const prev =
      map.get(key) ||
      makeEntry(key, isSelected, null, null, {
        isLocalComputer: isSelected && localIsDesktop,
        nodeNamePending: isSelected && localPending,
      });
    const next = updater(prev);
    map.set(key, next);
    emit(false);
    return next;
  }

  let fatalError = null;
  const capturePairingError = (fallback) => (err) => {
    if (isPairingError(err) && !fatalError) fatalError = err;
    return fallback;
  };

  async function fetchMachineDetails(name, isLocal) {
    const on = name || undefined;
    const targetKey = () => (isLocal ? here : name);
    const onErr = isLocal ? capturePairingError(undefined) : () => {};
    const tasks = [
      callWithTimeout(link, 'sys.info', {}, { on, signal }, detailTimeoutMs)
        .then((sys) => {
          if (signal?.aborted || !sys) return;
          if (isLocal && localPending && isRealMachineName(sys.node)) {
            settleLocalName(sys.node);
          }
          updateMachine(targetKey(), (prev) => ({
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
          updateMachine(targetKey(), (prev) => ({
            ...prev,
            jobs,
            runningJobs: jobs.length,
            busy: jobs.length > 0 || prev.busyState === 'busy' || (prev.busyReasons?.length || 0) > 0,
          }));
        })
        .catch(onErr),
      callWithTimeout(link, 'power.status', {}, { on, signal }, detailTimeoutMs)
        .then((power) => {
          if (signal?.aborted || !power) return;
          const busyState = typeof power?.busy?.state === 'string' ? power.busy.state : null;
          const busyReasons =
            busyState === 'busy' && Array.isArray(power.busy.reasons) ? power.busy.reasons : [];
          updateMachine(targetKey(), (prev) => ({
            ...prev,
            system: power.system || prev.system,
            busyState,
            busyReasons,
            busy: (prev.runningJobs || 0) > 0 || busyState === 'busy',
          }));
        })
        .catch(onErr),
    ];

    if (isLocal) {
      tasks.push(
        callWithTimeout(link, 'meta.version', {}, { on, signal }, detailTimeoutMs)
          .then((ver) => {
            if (signal?.aborted || !ver) return;
            if (localPending && isRealMachineName(ver.node)) {
              settleLocalName(ver.node);
            }
            updateMachine(targetKey(), (prev) => ({
              ...prev,
              status: 'answering',
              reachable: true,
              asking: false,
              fromCache: false,
              lastSeenAt: now(),
              os: prev.os || ver.os || null,
              arch: prev.arch || ver.arch || null,
              version: ver.version || prev.version,
              build: ver.build || prev.build || null,
            }));
          })
          .catch(onErr),
      );
      if (localPending) {
        tasks.push(
          callWithTimeout(link, 'meta.whoami', {}, { on, signal }, detailTimeoutMs)
            .then((whoami) => {
              if (signal?.aborted || !whoami) return;
              if (isRealMachineName(whoami.node)) {
                settleLocalName(whoami.node);
              }
            })
            .catch(onErr),
        );
      }
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

  // If the link learned the local node's real name during fleet.list (or a local-pipe fleet.list
  // returned it directly), settle "This computer" into that real name before reconciling rows.
  if (localPending) {
    const linkKnown =
      typeof link?.getLocalMachineName === 'function' ? link.getLocalMachineName() : null;
    if (isRealMachineName(linkKnown)) {
      settleLocalName(linkKnown);
    } else {
      const rawAns = fleetList?.answered_by;
      const answeredViaLocal = !rawAns || isLocalConnectionName(rawAns);
      if (answeredViaLocal) {
        if (isRealMachineName(fleetList?.node)) {
          settleLocalName(fleetList.node);
        } else {
          const selfEntry = (fleetList?.machines || []).find(
            (m) => m?.this_node && isRealMachineName(m.name),
          );
          if (selfEntry) settleLocalName(selfEntry.name);
        }
      }
    }
  }

  const rawAnsweredBy = fleetList?.answered_by;
  const rawListNode = fleetList?.node;
  const answeringNode = isLocalConnectionName(rawAnsweredBy)
    ? here
    : rawAnsweredBy || (isLocalConnectionName(rawListNode) ? here : rawListNode) || here;
  const answeredByPeer = Boolean(
    answeringNode && here && answeringNode !== here && !isLocalConnectionName(answeringNode),
  );
  const listedMap = new Map();
  for (const m of fleetList?.machines || []) {
    if (m && m.name && !isLocalConnectionName(m.name)) {
      listedMap.set(m.name, m);
    }
  }
  if (here && !listedMap.has(here)) {
    listedMap.set(here, { name: here, this_node: true, systems: [], wake_via: [] });
  }
  if (answeringNode && !isLocalConnectionName(answeringNode) && !listedMap.has(answeringNode)) {
    listedMap.set(answeringNode, { name: answeringNode, this_node: false, systems: [], wake_via: [] });
  }

  // Reconcile the machine map with fleet.toml; never-seen remotes show as `asking`, while machines
  // known to be off stay calmly `offline`. Keep the selected node (`here`) fixed — another node
  // answering `fleet.list` never switches the selected node.
  for (const existing of Array.from(map.keys())) {
    if (!listedMap.has(existing)) map.delete(existing);
  }
  const remoteNames = [];
  for (const [name, listed] of listedMap.entries()) {
    const isSelected = name === here;
    const isAnsweringPeer = answeredByPeer && name === answeringNode;
    const prev = map.get(name);
    if (isSelected) {
      map.set(name, {
        ...(prev ||
          makeEntry(name, true, listed, null, {
            isLocalComputer: localIsDesktop,
            nodeNamePending: localPending,
          })),
        thisNode: true,
        isLocalComputer: localIsDesktop,
        nodeNamePending: localPending,
        canWake: Boolean(listed.mac || (listed.wake_via && listed.wake_via.length > 0)),
      });
      if (answeredByPeer && !localPending) {
        // Selected node did not answer `fleet.list`; probe it like any peer so if it is off it
        // settles cleanly into `offline` rather than staying `asking`.
        remoteNames.push(name);
      }
    } else if (isAnsweringPeer) {
      map.set(name, {
        ...makeEntry(name, false, listed, prev),
        thisNode: false,
        isLocalComputer: false,
        nodeNamePending: false,
        canWake: Boolean(listed.mac || (listed.wake_via && listed.wake_via.length > 0)),
      });
    } else {
      const nextEntry = makeEntry(name, false, listed, prev);
      nextEntry.thisNode = false;
      nextEntry.isLocalComputer = false;
      nextEntry.nodeNamePending = false;
      map.set(name, nextEntry);
      remoteNames.push(name);
    }
  }
  emit(false);

  // When a peer answered `fleet.list`, fetch live details for that answering peer directly too.
  const answeringPeerPromise = answeredByPeer
    ? fetchMachineDetails(answeringNode, false)
    : Promise.resolve();

  // Probe each remote machine once; machines known to be off are checked quietly at most every
  // `offlineRecheckMs` (or when `forceOffline` is set) without flipping back to `asking`.
  const remotesPromise = runPool(remoteNames, maxConcurrentPings, async (name) => {
    if (signal?.aborted) return;
    const current = map.get(name);
    if (current?.status === 'offline' && !forceOffline) {
      if (!checkOffline) return;
      if (current.lastCheckedAt && now() - current.lastCheckedAt < offlineRecheckMs) {
        return;
      }
    }

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

    const checkedAt = now();
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
        lastCheckedAt: checkedAt,
        error,
        reason: firstSentence(error.message) || 'Did not answer',
        sys: null,
        jobs: [],
        runningJobs: 0,
        busy: false,
        busyState: null,
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
      lastSeenAt: checkedAt,
      lastCheckedAt: checkedAt,
      os: pingEntry.os || prev.os,
      arch: pingEntry.arch || prev.arch,
      version: pingEntry.version || prev.version,
      build: pingEntry.build || prev.build || null,
      system: pingEntry.system || prev.system,
      latencyMs: pingEntry.latency_ms ?? null,
    }));

    await fetchMachineDetails(name, false);
  });

  await Promise.all([localPromise, answeringPeerPromise, remotesPromise]);
  if (signal?.aborted) return null;
  if (fatalError) throw fatalError;

  if (here) {
    const localEntry = map.get(here);
    if (localEntry && !localEntry.reachable && (localEntry.status === 'asking' || localEntry.asking)) {
      map.set(here, {
        ...localEntry,
        status: 'offline',
        asking: false,
        fromCache: false,
        lastCheckedAt: now(),
      });
    }
  }

  activeCachedAt = null;
  const finalSnap = snapshot(true);
  if (typeof onUpdate === 'function') onUpdate(finalSnap);
  if (typeof onSettled === 'function') onSettled(finalSnap);
  return finalSnap;
}
