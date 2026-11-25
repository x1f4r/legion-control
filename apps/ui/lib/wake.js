// Shared "Wake it and continue" coordinator (`ensureAwake`) with injectable clock.
//
// One controller backs every starting point that needs an off machine (opening its screen or
// files, sending or moving a file to or from it, running an action or a command on it).
// Two waiters for the same machine share one wake and one sheet.

export const WAKE_POLL_INTERVAL_MS = 3_000;
export const WAKE_TIMEOUT_MS = 180_000;
export const WAKE_ELAPSED_TICK_MS = 1_000;

const FORGETTABLE_KINDS = new Set(['screen', 'files', 'wake']);

/**
 * Formats the plain "Then: ..." line for the wake prompt sheet from the first waiter.
 */
export function formatWakeContinueLine(waiter, machineName) {
  if (!waiter || waiter.kind === 'wake') return null;
  const rawLabel = typeof waiter.label === 'string' ? waiter.label.trim() : '';
  if (rawLabel) {
    if (/^then:/i.test(rawLabel)) return rawLabel;
    return `Then: ${rawLabel}`;
  }
  switch (waiter.kind) {
    case 'files':
      return 'Then: open its files';
    case 'transfer':
      return `Then: transfer files with ${machineName}`;
    case 'command':
      return `Then: run command on ${machineName}`;
    case 'action':
      return `Then: run action on ${machineName}`;
    case 'screen':
    default:
      return `Then: open ${machineName}`;
  }
}

function defaultClock() {
  return {
    now: () => Date.now(),
    setInterval: (fn, ms) => setInterval(fn, ms),
    clearInterval: (id) => clearInterval(id),
  };
}

let defaultController = null;

/**
 * Registers the active UI wake controller so any module can import `ensureAwake(machine, opts)`.
 */
export function setDefaultWakeController(controller) {
  defaultController = controller || null;
}

/**
 * Ensures `machine` is awake before running `options.onContinue`, showing the shared wake sheet
 * when the machine is off, wakeable, and allowed for this device.
 */
export function ensureAwake(machine, options = {}) {
  if (!defaultController) {
    throw new Error('Wake controller is not initialized.');
  }
  return defaultController.ensureAwake(machine, options);
}

/**
 * Creates a testable wake coordinator with an injectable clock.
 */
export function createWakeController({
  link,
  getMachines = () => [],
  getLocalNode = () => null,
  getAllowedSet = () => null,
  onWakeSent = null,
  onWakeTimeout = null,
  onMachineAwake = null,
  onBackgroundComplete = null,
  clock = defaultClock(),
  pollIntervalMs = WAKE_POLL_INTERVAL_MS,
  timeoutMs = WAKE_TIMEOUT_MS,
} = {}) {
  const entries = new Map();
  const listeners = new Set();
  let waiterSeq = 1;
  let activeSheetMachine = null;

  function emit() {
    const snap = getState();
    for (const fn of listeners) {
      try {
        fn(snap);
      } catch {
        // Ignore subscriber errors.
      }
    }
  }

  function resolveMachine(machineOrName) {
    const list = getMachines() || [];
    if (typeof machineOrName === 'string') {
      return list.find((m) => m.name === machineOrName) || { name: machineOrName, reachable: false, canWake: false };
    }
    if (machineOrName && typeof machineOrName === 'object' && machineOrName.name) {
      return list.find((m) => m.name === machineOrName.name) || machineOrName;
    }
    return null;
  }

  function canUseWake(machine) {
    if (!machine?.canWake) return false;
    const allowed = getAllowedSet();
    if (allowed && !allowed.has('power.wake')) return false;
    return true;
  }

  function pickWakeRunner(targetName) {
    const list = getMachines() || [];
    const online = list.find((m) => m.reachable && m.name !== targetName);
    return online?.name || getLocalNode() || undefined;
  }

  function stopTimers(entry) {
    if (entry.pollTimer !== null) {
      clock.clearInterval(entry.pollTimer);
      entry.pollTimer = null;
    }
    if (entry.tickTimer !== null) {
      clock.clearInterval(entry.tickTimer);
      entry.tickTimer = null;
    }
  }

  function clearEntry(name) {
    const entry = entries.get(name);
    if (!entry) return;
    stopTimers(entry);
    entries.delete(name);
    if (activeSheetMachine === name) {
      const nextVisible = Array.from(entries.values()).find((e) => e.visible);
      activeSheetMachine = nextVisible ? nextVisible.machineName : null;
    }
    emit();
  }

  async function runWaiterOnce(waiter, result, inBackground) {
    if (waiter.done) return;
    waiter.done = true;
    try {
      if (typeof waiter.onContinue === 'function') {
        await waiter.onContinue({ woke: result.woke, inBackground });
      }
      if (inBackground && typeof onBackgroundComplete === 'function') {
        onBackgroundComplete({
          machine: waiter.machineName,
          kind: waiter.kind,
          label: waiter.label || null,
        });
      }
    } finally {
      waiter.resolve(result);
    }
  }

  async function finishAwake(entry) {
    if (entry.finished) return;
    entry.finished = true;
    stopTimers(entry);
    const waiters = entry.waiters.slice();
    const wasBackground = !entry.visible;
    entries.delete(entry.machineName);
    if (activeSheetMachine === entry.machineName) {
      const nextVisible = Array.from(entries.values()).find((e) => e.visible);
      activeSheetMachine = nextVisible ? nextVisible.machineName : null;
    }
    emit();

    if (typeof onMachineAwake === 'function') {
      try {
        await onMachineAwake(entry.machineName);
      } catch {
        // Continue even if fleet refresh lags.
      }
    }

    for (const waiter of waiters) {
      if (waiter.forgotten || waiter.done) continue;
      await runWaiterOnce(waiter, { awake: true, woke: true }, wasBackground);
    }
  }

  async function pollOnce(entry) {
    if (entry.finished || entry.phase !== 'waking' || entry.pingInFlight) return;
    if (clock.now() - entry.startedAt >= timeoutMs) {
      handleTimeout(entry);
      return;
    }
    entry.pingInFlight = true;
    try {
      const res = await link.call(
        'fleet.ping',
        { machines: [entry.machineName], timeout_s: 3 },
        { on: pickWakeRunner(entry.machineName) },
      );
      if (entry.finished || entry.phase !== 'waking') return;
      const results = Array.isArray(res?.results) ? res.results : [];
      const hit = results.find((r) => r.machine === entry.machineName) || results[0];
      if (hit?.reachable) {
        await finishAwake(entry);
        return;
      }
      if (clock.now() - entry.startedAt >= timeoutMs) {
        handleTimeout(entry);
      }
    } catch {
      if (!entry.finished && entry.phase === 'waking' && clock.now() - entry.startedAt >= timeoutMs) {
        handleTimeout(entry);
      }
    } finally {
      entry.pingInFlight = false;
    }
  }

  function handleTimeout(entry) {
    if (entry.finished) return;
    stopTimers(entry);
    entry.phase = 'timeout';
    entry.elapsedS = Math.floor(timeoutMs / 1000);
    entry.visible = true;
    activeSheetMachine = entry.machineName;
    if (typeof onWakeTimeout === 'function') {
      try {
        onWakeTimeout(entry.machineName);
      } catch {
        // Ignore callback errors.
      }
    }
    emit();
  }

  async function startWaking(entry) {
    if (entry.finished || entry.phase === 'waking') return;
    entry.error = null;
    entry.phase = 'waking';
    entry.startedAt = clock.now();
    entry.elapsedS = 0;
    entry.visible = true;
    activeSheetMachine = entry.machineName;
    emit();

    try {
      await link.call(
        'power.wake',
        { machine: entry.machineName },
        { on: pickWakeRunner(entry.machineName) },
      );
      if (typeof onWakeSent === 'function') {
        try {
          onWakeSent(entry.machineName);
        } catch {
          // Ignore callback errors.
        }
      }
    } catch (err) {
      if (entry.finished) return;
      stopTimers(entry);
      entry.phase = 'error';
      entry.error = err;
      emit();
      return;
    }

    if (entry.finished || entry.phase !== 'waking') return;

    entry.tickTimer = clock.setInterval(() => {
      if (entry.finished || entry.phase !== 'waking') return;
      const elapsedMs = Math.max(0, clock.now() - entry.startedAt);
      entry.elapsedS = Math.floor(elapsedMs / 1000);
      if (elapsedMs >= timeoutMs) {
        handleTimeout(entry);
        return;
      }
      emit();
    }, WAKE_ELAPSED_TICK_MS);

    entry.pollTimer = clock.setInterval(() => {
      pollOnce(entry);
    }, pollIntervalMs);
  }

  /**
   * Ensures `machine` is awake. If already reachable, runs `onContinue` immediately.
   * If off and wakeable (and allowed), opens or joins the shared wake sheet for that machine.
   */
  async function ensureAwakeImpl(machineOrName, options = {}) {
    const machine = resolveMachine(machineOrName);
    if (!machine?.name) {
      return { awake: false, canWake: false };
    }

    const kind = options.kind || 'screen';
    const forgettable =
      typeof options.forgettable === 'boolean' ? options.forgettable : FORGETTABLE_KINDS.has(kind);
    const onContinue = typeof options.onContinue === 'function' ? options.onContinue : null;

    if (machine.reachable) {
      if (onContinue) {
        await onContinue({ woke: false, inBackground: false });
      }
      return { awake: true, woke: false };
    }

    if (!canUseWake(machine)) {
      return { awake: false, canWake: false };
    }

    return new Promise((resolve) => {
      const waiter = {
        id: waiterSeq++,
        machineName: machine.name,
        kind,
        label: options.label || null,
        forgettable,
        onContinue,
        resolve,
        done: false,
        forgotten: false,
      };

      const existing = entries.get(machine.name);
      if (existing && !existing.finished) {
        existing.waiters.push(waiter);
        existing.visible = true;
        activeSheetMachine = machine.name;
        if (options.immediate && existing.phase !== 'waking') {
          startWaking(existing);
        } else {
          emit();
        }
        return;
      }

      const entry = {
        machineName: machine.name,
        phase: 'prompt', // 'prompt' | 'waking' | 'timeout' | 'error'
        startedAt: null,
        elapsedS: 0,
        error: null,
        visible: true,
        finished: false,
        pingInFlight: false,
        pollTimer: null,
        tickTimer: null,
        waiters: [waiter],
      };
      entries.set(machine.name, entry);
      activeSheetMachine = machine.name;
      if (options.immediate) {
        startWaking(entry);
      } else {
        emit();
      }
    });
  }

  function confirmWake(machineName = activeSheetMachine) {
    const entry = entries.get(machineName);
    if (!entry || entry.finished) return Promise.resolve();
    return startWaking(entry);
  }

  function retry(machineName = activeSheetMachine) {
    const entry = entries.get(machineName);
    if (!entry || entry.finished) return Promise.resolve();
    return startWaking(entry);
  }

  /**
   * Dismisses the sheet (user pressed "Not now" or closed the dialog).
   * In `prompt`, `timeout`, or `error`, cancels every waiter.
   * In `waking`, behaves like leaving the screen: forgets navigation waiters while keeping
   * background work (transfers, actions, commands) alive until the machine answers.
   */
  function dismiss(machineName = activeSheetMachine) {
    const entry = entries.get(machineName);
    if (!entry || entry.finished) return;
    if (entry.phase === 'waking') {
      leaveScreen(machineName);
      return;
    }
    const waiters = entry.waiters.slice();
    clearEntry(machineName);
    for (const w of waiters) {
      if (!w.done) {
        w.done = true;
        w.resolve({ awake: false, cancelled: true });
      }
    }
  }

  /**
   * Called when the user leaves the current screen while waiting.
   * Forgets screen/files opening waiters; keeps transfers, actions, and commands waiting in the
   * background so they still run once the machine answers.
   */
  function leaveScreen(targetMachine = null) {
    const names = targetMachine ? [targetMachine] : Array.from(entries.keys());
    let changed = false;

    for (const name of names) {
      const entry = entries.get(name);
      if (!entry || entry.finished) continue;

      if (entry.phase !== 'waking') {
        const waiters = entry.waiters.slice();
        stopTimers(entry);
        entries.delete(name);
        if (activeSheetMachine === name) activeSheetMachine = null;
        for (const w of waiters) {
          if (!w.done) {
            w.done = true;
            w.resolve({ awake: false, forgotten: true });
          }
        }
        changed = true;
        continue;
      }

      const remaining = [];
      for (const w of entry.waiters) {
        if (w.forgettable) {
          if (!w.done) {
            w.forgotten = true;
            w.done = true;
            w.resolve({ awake: false, forgotten: true });
          }
        } else {
          remaining.push(w);
        }
      }
      entry.waiters = remaining;
      entry.visible = false;
      if (activeSheetMachine === name) activeSheetMachine = null;
      if (remaining.length === 0) {
        stopTimers(entry);
        entries.delete(name);
      }
      changed = true;
    }

    if (changed) {
      if (!activeSheetMachine) {
        const nextVisible = Array.from(entries.values()).find((e) => e.visible);
        activeSheetMachine = nextVisible ? nextVisible.machineName : null;
      }
      emit();
    }
  }

  function getState() {
    const active = activeSheetMachine ? entries.get(activeSheetMachine) || null : null;
    const wakingMachines = new Set();
    for (const entry of entries.values()) {
      if (entry.phase === 'waking') wakingMachines.add(entry.machineName);
    }
    const primaryWaiter = active?.waiters?.find((w) => !w.forgotten && !w.done) || active?.waiters?.[0] || null;
    return {
      sheet: active && active.visible
        ? {
            machineName: active.machineName,
            phase: active.phase,
            elapsedS: active.elapsedS,
            error: active.error,
            continueText: formatWakeContinueLine(primaryWaiter, active.machineName),
            waiterCount: active.waiters.length,
            hasBackgroundWork: active.waiters.some((w) => !w.forgettable),
          }
        : null,
      wakingMachines,
    };
  }

  function subscribe(fn) {
    if (typeof fn !== 'function') return () => {};
    listeners.add(fn);
    return () => listeners.delete(fn);
  }

  function dispose() {
    for (const entry of entries.values()) {
      stopTimers(entry);
    }
    entries.clear();
    activeSheetMachine = null;
  }

  return {
    ensureAwake: ensureAwakeImpl,
    confirmWake,
    retry,
    dismiss,
    leaveScreen,
    getState,
    subscribe,
    dispose,
  };
}
