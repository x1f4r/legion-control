// Per-machine tool capability store and guarded tool call wrapper.
//
// Rule: the interface asks each machine what it can do, per machine, and never sends a machine
// something it has not said it understands. Not knowing yet means assuming the least.

export const DEFAULT_CAPS_TIMEOUT_MS = 8_000;
const UNREACHABLE_COOLDOWN_MS = 2_000;

/**
 * Creates a typed error indicating the target machine's Legion binary is missing a required tool
 * or essential argument.
 */
export function makeNeedsNewerLegionError({ machine = null, tool = null, missingArgs = [] } = {}) {
  const target = machine || 'This machine';
  const argsList = Array.isArray(missingArgs) ? missingArgs.filter(Boolean) : [];
  const hint =
    tool && argsList.length > 0
      ? `Tool \`${tool}\` on ${target} does not accept ${argsList.map((a) => `\`${a}\``).join(', ')}.`
      : tool
        ? `Tool \`${tool}\` is not available on ${target}.`
        : null;
  return {
    code: 'needs_newer_legion',
    needsNewerLegion: true,
    machine: machine || null,
    tool: tool || null,
    missingTool: argsList.length === 0 ? tool || null : null,
    missingArgs: argsList,
    message: `${target} needs a newer Legion for this.`,
    hint,
  };
}

export function isNeedsNewerLegionError(err) {
  return Boolean(err && typeof err === 'object' && (err.code === 'needs_newer_legion' || err.needsNewerLegion === true));
}

/**
 * Parses an `invalid_args` error message from a Legion node for an unknown field name and any
 * listed expected field names.
 */
export function parseUnknownFieldError(err) {
  if (!err || typeof err !== 'object' || err.code !== 'invalid_args') return null;
  const msg = String(err.message || '');
  const fieldMatch = msg.match(/unknown field\s+[`"']?([a-zA-Z0-9_]+)[`"']?/i);
  if (!fieldMatch) return null;
  const field = fieldMatch[1];
  let expected = null;
  const expMatch = msg.match(/expected one of\s+([^.]+)/i);
  if (expMatch) {
    const names = [];
    for (const m of expMatch[1].matchAll(/[`"']([a-zA-Z0-9_]+)[`"']/g)) {
      names.push(m[1]);
    }
    if (names.length > 0) expected = names;
  }
  return { field, expected };
}

function buildToolMap(tools) {
  const map = new Map();
  for (const spec of tools || []) {
    if (spec && typeof spec.name === 'string' && spec.name) {
      map.set(spec.name, spec);
    }
  }
  return map;
}

function specHasProp(spec, propName) {
  const props = spec?.input_schema?.properties;
  return Boolean(props && typeof props === 'object' && Object.prototype.hasOwnProperty.call(props, propName));
}

function isUnreachableError(err) {
  const code = String(err?.code || '').toLowerCase();
  return code === 'unreachable' || code === 'timeout';
}

function isUnsupportedOrUnknown(err) {
  const code = String(err?.code || '').toLowerCase();
  return code === 'unknown_tool' || code === 'unsupported';
}

function withBoundedWait(promise, { signal, timeoutMs, machine }) {
  if (signal?.aborted) {
    return Promise.reject({
      code: 'cancelled',
      message: 'The call was cancelled.',
      hint: null,
      machine: machine || null,
    });
  }
  return new Promise((resolve, reject) => {
    let settled = false;
    let timer = null;

    const cleanup = () => {
      if (timer !== null) {
        clearTimeout(timer);
        timer = null;
      }
      if (signal) signal.removeEventListener('abort', onAbort);
    };

    const onAbort = () => {
      if (settled) return;
      settled = true;
      cleanup();
      reject({
        code: 'cancelled',
        message: 'The call was cancelled.',
        hint: null,
        machine: machine || null,
      });
    };

    if (signal) {
      signal.addEventListener('abort', onAbort, { once: true });
    }

    if (timeoutMs > 0) {
      timer = setTimeout(() => {
        if (settled) return;
        settled = true;
        cleanup();
        const secs = Math.max(1, Math.round(timeoutMs / 1000));
        reject({
          code: 'unreachable',
          message: `Timed out waiting for ${machine || 'machine'} after ${secs} s.`,
          hint: null,
          machine: machine || null,
        });
      }, timeoutMs);
    }

    promise.then(
      (val) => {
        if (settled) return;
        settled = true;
        cleanup();
        resolve(val);
      },
      (err) => {
        if (settled) return;
        settled = true;
        cleanup();
        reject(err);
      },
    );
  });
}

/**
 * Pure helper computing file capabilities from a single machine's capability snapshot and link.
 * Conservative when capabilities are not yet known: unknown means false, never true.
 */
export function computeFileCapabilities({
  known = false,
  tools = [],
  allowedSet = null,
  link = null,
  machine = null,
} = {}) {
  const map = buildToolMap(tools);
  const isKnown = Boolean(known && map.size > 0);

  const hasToolInCatalog = (name) => isKnown && map.has(name);
  const hasTool = (name) => {
    if (!hasToolInCatalog(name)) return false;
    return allowedSet ? allowedSet.has(name) : true;
  };
  const propOf = (toolName, propName) => {
    if (!isKnown) return false;
    return specHasProp(map.get(toolName), propName);
  };

  const hasList = hasTool('fs.list');
  const hasRead = hasTool('fs.read');
  const hasFind = hasTool('fs.find');
  const hasThumb = hasTool('fs.thumb');
  const hasFav = hasTool('fav.list');
  const hasFavWrite = hasFav && hasTool('fav.add') && hasTool('fav.remove');
  const hasSync = hasTool('sync.list');
  const hasSyncWrite = hasSync && (hasTool('sync.pause') || hasTool('sync.add'));
  const hasMkdir = hasTool('fs.mkdir');
  const hasCopy = hasTool('fs.copy');
  const hasMove = hasTool('fs.move');
  const hasRemove = hasTool('fs.remove');
  const hasHash = hasTool('fs.hash');
  const hasTransfer = hasTool('fs.transfer');
  const hasTransferBg = hasTransfer && propOf('fs.transfer', 'background');
  const hasTransferMove = hasTransfer && propOf('fs.transfer', 'move') && hasRemove;
  const hasTransferIfExists = hasTransfer && propOf('fs.transfer', 'if_exists');

  const hasListSort = hasList && propOf('fs.list', 'sort');
  const hasListOffset = hasList && propOf('fs.list', 'offset');
  const hasListCategories = hasList && propOf('fs.list', 'categories');
  const hasFindEnhanced = hasFind && (propOf('fs.find', 'name') || propOf('fs.find', 'categories'));
  const hasFindFilters =
    hasFind &&
    (propOf('fs.find', 'min_size') ||
      propOf('fs.find', 'max_size') ||
      propOf('fs.find', 'modified_after'));

  // Content addresses shipped alongside `fs.thumb` and `fs.list.offset`. When the target
  // machine's tool list is known to lack both (older node), do not attempt content addresses
  // even if the link itself supports them for newer nodes.
  const linkHasContent = typeof link?.contentUrl === 'function';
  const catalogSupportsContent =
    isKnown && (hasToolInCatalog('fs.thumb') || propOf('fs.list', 'offset'));
  const linkAllowsMachineContent =
    typeof link?.supportsContentFor === 'function'
      ? Boolean(link.supportsContentFor(machine))
      : link?.kind === 'mock'
        ? true
        : link?.supportsContent === true || catalogSupportsContent;

  const hasContentUrl = isKnown && linkHasContent && linkAllowsMachineContent && catalogSupportsContent;

  const hasDownload =
    isKnown &&
    typeof link?.download === 'function' &&
    (link?.kind !== 'http' || hasContentUrl);
  const hasOpenExternally = isKnown && typeof link?.openExternally === 'function';
  const hasUpload = isKnown && typeof link?.upload === 'function';
  const hasDeviceTransfers = typeof link?.transfers === 'function';

  return {
    known: isKnown,
    hasList,
    hasRead,
    hasListSort,
    hasListOffset,
    hasListCategories,
    hasFind,
    hasFindEnhanced,
    hasFindFilters,
    hasThumb,
    hasFav,
    hasFavWrite,
    hasSync,
    hasSyncWrite,
    hasMkdir,
    hasCopy,
    hasMove,
    hasRemove,
    hasHash,
    hasTransfer,
    hasTransferBg,
    hasTransferMove,
    hasTransferIfExists,
    hasContentUrl,
    hasDownload,
    hasOpenExternally,
    hasUpload,
    hasDeviceTransfers,
  };
}

/**
 * Lists notable features missing from a machine's binary catalog (independent of role policy).
 * Returns an empty array when capabilities are unknown or the machine supports the current set.
 */
export function describeMissingFeatures(entry) {
  if (!entry || entry.status !== 'known' || !(entry.toolMap instanceof Map) || entry.toolMap.size === 0) {
    return [];
  }
  const map = entry.toolMap;
  const missing = [];

  const listSpec = map.get('fs.list');
  if (listSpec) {
    const missingListArgs = ['sort', 'order', 'categories', 'offset'].filter((p) => !specHasProp(listSpec, p));
    if (missingListArgs.length > 0) {
      missing.push(`fs.list (${missingListArgs.join(', ')})`);
    }
  }

  const findSpec = map.get('fs.find');
  if (findSpec) {
    const missingFindArgs = ['name', 'categories', 'min_size', 'modified_after'].filter((p) => !specHasProp(findSpec, p));
    if (missingFindArgs.length > 0) {
      missing.push(`fs.find (${missingFindArgs.join(', ')})`);
    }
  }

  if (!map.has('fs.thumb')) {
    missing.push('fs.thumb');
  }

  if (!map.has('fav.list')) {
    missing.push('fav.*');
  }

  const xferSpec = map.get('fs.transfer');
  if (xferSpec) {
    // Moving between places (`move`) is switched off in default builds, so omit it here:
    // a feature disabled everywhere is not a missing feature of an older machine.
    const missingXferArgs = ['background', 'if_exists'].filter((p) => !specHasProp(xferSpec, p));
    if (missingXferArgs.length > 0) {
      missing.push(`fs.transfer (${missingXferArgs.join(', ')})`);
    }
  }

  const procsSpec = map.get('sys.procs');
  if (procsSpec && !specHasProp(procsSpec, 'group')) {
    missing.push('sys.procs (group)');
  }

  const killSpec = map.get('sys.kill');
  if (killSpec && !specHasProp(killSpec, 'targets')) {
    missing.push('sys.kill (targets)');
  }

  if (!map.has('update.list')) {
    missing.push('update.*');
  }

  // Only list sync.* when the machine is already missing core newer features so optional sync
  // configurations (such as `?mock=nosync`) are not mislabelled as an older Legion binary.
  if (missing.length > 0 && !map.has('sync.list')) {
    missing.push('sync.*');
  }

  return missing;
}

/**
 * Creates a per-machine capabilities store backed by a Legion link (`app`, `http`, or `mock`).
 */
export function createCapabilitiesStore(link, options = {}) {
  const defaultTimeoutMs = Number.isFinite(options.timeoutMs)
    ? options.timeoutMs
    : DEFAULT_CAPS_TIMEOUT_MS;
  const nowFn = typeof options.now === 'function' ? options.now : () => Date.now();

  const entries = new Map(); // machine -> entry
  const inFlight = new Map(); // machine -> { promise, generation }
  const observedMeta = new Map(); // machine -> { version, build, reachable }
  const machineGeneration = new Map(); // machine -> number
  const listeners = new Set();
  let revision = 0;
  let globalGeneration = 0;

  function generationOf(machine) {
    return globalGeneration + (machineGeneration.get(machine) || 0);
  }

  function bumpMachineGeneration(machine) {
    machineGeneration.set(machine, (machineGeneration.get(machine) || 0) + 1);
  }

  function notify() {
    revision += 1;
    for (const fn of listeners) {
      try {
        fn(revision);
      } catch {
        // Keep notifying other subscribers.
      }
    }
  }

  let unwatchContent = null;
  if (typeof link?.onContentSupportChange === 'function') {
    unwatchContent = link.onContentSupportChange(() => notify());
  }

  function keyOf(machine) {
    return typeof machine === 'string' ? machine.trim() : '';
  }

  function makeUnknownEntry(machine) {
    const meta = observedMeta.get(machine);
    return {
      machine,
      status: 'unknown',
      known: false,
      tools: [],
      toolMap: new Map(),
      allowedSet: null,
      version: meta?.version || null,
      build: meta?.build || null,
      schemaComplete: false,
      error: null,
      fetchedAt: 0,
      generation: generationOf(machine),
    };
  }

  function get(machine) {
    const k = keyOf(machine);
    if (!k) return makeUnknownEntry('');
    return entries.get(k) || makeUnknownEntry(k);
  }

  function isKnown(machine) {
    const k = keyOf(machine);
    if (!k) return false;
    return entries.get(k)?.status === 'known';
  }

  function hasToolInCatalog(machine, toolName) {
    const e = get(machine);
    if (e.status !== 'known') return false;
    return e.toolMap.has(toolName);
  }

  function hasTool(machine, toolName) {
    const e = get(machine);
    if (e.status !== 'known') return false;
    if (!e.toolMap.has(toolName)) return false;
    return e.allowedSet ? e.allowedSet.has(toolName) : true;
  }

  function acceptsArg(machine, toolName, argName) {
    const e = get(machine);
    if (e.status !== 'known') return false;
    const spec = e.toolMap.get(toolName);
    if (!spec) return false;
    return specHasProp(spec, argName);
  }

  function getToolSpec(machine, toolName) {
    const e = get(machine);
    if (e.status !== 'known') return null;
    return e.toolMap.get(toolName) || null;
  }

  function getTools(machine) {
    const e = get(machine);
    return e.status === 'known' ? e.tools : [];
  }

  function seed(machine, { tools, allowedSet = null, version = null, build = null, schemaComplete = true } = {}) {
    const k = keyOf(machine);
    if (!k || !Array.isArray(tools) || tools.length === 0) return;
    const prev = entries.get(k);
    const prevMeta = observedMeta.get(k);
    const resolvedVersion = version || prev?.version || prevMeta?.version || null;
    const resolvedBuild = build || prev?.build || prevMeta?.build || null;
    const nextAllowed =
      allowedSet instanceof Set
        ? allowedSet
        : Array.isArray(allowedSet)
          ? new Set(allowedSet)
          : prev?.allowedSet || null;
    const toolMap = buildToolMap(tools);
    entries.set(k, {
      machine: k,
      status: 'known',
      known: true,
      tools: [...tools],
      toolMap,
      allowedSet: nextAllowed,
      version: resolvedVersion,
      build: resolvedBuild,
      schemaComplete: Boolean(schemaComplete),
      error: null,
      fetchedAt: nowFn(),
      generation: generationOf(k),
    });
    observedMeta.set(k, {
      version: resolvedVersion,
      build: resolvedBuild,
      reachable: true,
    });
    notify();
  }

  function invalidate(machine) {
    if (machine) {
      const k = keyOf(machine);
      if (!k) return;
      bumpMachineGeneration(k);
      entries.delete(k);
      inFlight.delete(k);
      notify();
      return;
    }
    globalGeneration += 1;
    entries.clear();
    inFlight.clear();
    notify();
  }

  /**
   * Moves cached capabilities and observed metadata from one machine key (such as `"local"` or
   * `"This computer"`) to the resolved node name without losing already-fetched tool schemas.
   */
  function renameMachine(fromName, toName) {
    const fromKey = keyOf(fromName);
    const toKey = keyOf(toName);
    if (!fromKey || !toKey || fromKey === toKey) return false;
    let changed = false;

    const prevEntry = entries.get(fromKey);
    if (prevEntry) {
      entries.delete(fromKey);
      const destEntry = entries.get(toKey);
      if (!destEntry || (destEntry.status !== 'known' && prevEntry.status === 'known')) {
        entries.set(toKey, {
          ...prevEntry,
          machine: toKey,
          generation: generationOf(toKey),
        });
      }
      changed = true;
    }

    const prevMeta = observedMeta.get(fromKey);
    if (prevMeta) {
      observedMeta.delete(fromKey);
      if (!observedMeta.has(toKey)) {
        observedMeta.set(toKey, { ...prevMeta });
      }
      changed = true;
    }

    if (inFlight.has(fromKey)) {
      inFlight.delete(fromKey);
      bumpMachineGeneration(fromKey);
      changed = true;
    }

    if (changed) notify();
    return changed;
  }

  /**
   * Synchronizes machine version/build/reachability metadata observed from fleet refreshes or
   * state updates. Refetches are triggered automatically if a machine's build/version changes
   * or if it reconnects after being offline.
   */
  function syncMachineMeta(machine, { version = null, build = null, reachable = undefined } = {}) {
    const k = keyOf(machine);
    if (!k) return false;
    const prevMeta = observedMeta.get(k) || { version: null, build: null, reachable: undefined };
    const existing = entries.get(k);

    const nextVersion = version || prevMeta.version || null;
    const nextBuild = build || prevMeta.build || null;
    const nextReachable = reachable !== undefined ? Boolean(reachable) : prevMeta.reachable;

    const buildChanged =
      Boolean(build) &&
      ((existing?.build && existing.build !== build) || (prevMeta.build && prevMeta.build !== build));
    const versionChanged =
      Boolean(version) &&
      ((existing?.version && existing.version !== version) ||
        (prevMeta.version && prevMeta.version !== version));
    const reconnected = prevMeta.reachable === false && nextReachable === true;

    observedMeta.set(k, {
      version: nextVersion,
      build: nextBuild,
      reachable: nextReachable,
    });

    if (buildChanged || versionChanged || reconnected) {
      bumpMachineGeneration(k);
      entries.delete(k);
      inFlight.delete(k);
      notify();
      return true;
    }

    if (existing && ((nextVersion && !existing.version) || (nextBuild && !existing.build))) {
      entries.set(k, {
        ...existing,
        version: existing.version || nextVersion,
        build: existing.build || nextBuild,
      });
      notify();
    }
    return false;
  }

  function markArgUnsupported(machine, toolName, argName) {
    const k = keyOf(machine);
    const existing = entries.get(k);
    if (!existing || existing.status !== 'known') return;
    const spec = existing.toolMap.get(toolName);
    if (!spec || !specHasProp(spec, argName)) return;

    const nextProps = { ...(spec.input_schema?.properties || {}) };
    delete nextProps[argName];
    const nextSpec = {
      ...spec,
      input_schema: {
        ...(spec.input_schema || { type: 'object' }),
        properties: nextProps,
      },
    };
    const nextMap = new Map(existing.toolMap);
    nextMap.set(toolName, nextSpec);
    const nextTools = existing.tools.map((t) => (t.name === toolName ? nextSpec : t));
    entries.set(k, {
      ...existing,
      tools: nextTools,
      toolMap: nextMap,
    });
    notify();
  }

  function markToolAbsent(machine, toolName) {
    const k = keyOf(machine);
    const existing = entries.get(k);
    if (!existing || existing.status !== 'known') return;
    if (!existing.toolMap.has(toolName)) return;
    const nextMap = new Map(existing.toolMap);
    nextMap.delete(toolName);
    const nextTools = existing.tools.filter((t) => t.name !== toolName);
    entries.set(k, {
      ...existing,
      tools: nextTools,
      toolMap: nextMap,
    });
    notify();
  }

  async function fetchRawCatalog(machine) {
    let toolsList = null;
    let toolsErr = null;
    let schemaComplete = true;

    // 1. Primary path: `link.tools({ on: machine })`.
    if (typeof link?.tools === 'function') {
      try {
        const res = await link.tools({ on: machine });
        if (Array.isArray(res)) {
          toolsList = res;
          if (res.schemaComplete === false) schemaComplete = false;
        } else if (res && Array.isArray(res.tools)) {
          toolsList = res.tools;
          if (res.schemaComplete === false) schemaComplete = false;
        }
      } catch (err) {
        toolsErr = err;
        if (isUnreachableError(err)) throw err;
      }
    }

    // 2. Fallback: `meta.tools` tool if link.tools was absent or unsupported.
    if ((!toolsList || toolsList.length === 0) && typeof link?.call === 'function' && (!toolsErr || isUnsupportedOrUnknown(toolsErr))) {
      try {
        const res = await link.call('meta.tools', {}, { on: machine });
        if (Array.isArray(res?.tools)) {
          toolsList = res.tools;
          schemaComplete = true;
        } else if (Array.isArray(res)) {
          toolsList = res;
          schemaComplete = true;
        }
      } catch (err) {
        if (isUnreachableError(err)) throw err;
        if (!toolsErr) toolsErr = err;
      }
    }

    // 3. Best-effort per-machine whoami (for role permissions) and version/build.
    const meta = observedMeta.get(machine);
    const [whoami, ver] =
      typeof link?.call === 'function'
        ? await Promise.all([
            link.call('meta.whoami', {}, { on: machine }).catch(() => null),
            !meta?.build || !meta?.version
              ? link.call('meta.version', {}, { on: machine }).catch(() => null)
              : Promise.resolve(null),
          ])
        : [null, null];

    const allowedSet = Array.isArray(whoami?.tools) ? new Set(whoami.tools) : null;
    const version = ver?.version || meta?.version || null;
    const build = ver?.build || meta?.build || null;

    if ((!toolsList || toolsList.length === 0) && Array.isArray(whoami?.tools) && whoami.tools.length > 0) {
      // Fallback when only tool names are available: construct minimal specs with no extra
      // refinements declared so guarded calls remain strictly conservative.
      toolsList = whoami.tools.map((name) => ({
        name,
        input_schema: { type: 'object', properties: {} },
      }));
      schemaComplete = false;
    }

    if (!toolsList || toolsList.length === 0) {
      throw (
        toolsErr || {
          code: 'unreachable',
          message: `${machine} did not report its capabilities.`,
          hint: null,
          machine,
        }
      );
    }

    return {
      tools: toolsList,
      allowedSet,
      version,
      build,
      schemaComplete,
    };
  }

  /**
   * Ensures capabilities for `machine` are loaded, deduplicating concurrent requests and
   * enforcing a bounded timeout.
   */
  async function ensure(machine, { signal, timeoutMs = defaultTimeoutMs, force = false } = {}) {
    const k = keyOf(machine);
    if (!k) return makeUnknownEntry('');

    const existing = entries.get(k);
    if (!force && existing?.status === 'known') {
      return existing;
    }
    if (!force && existing?.status === 'unreachable' && nowFn() - existing.fetchedAt < UNREACHABLE_COOLDOWN_MS) {
      throw existing.error || { code: 'unreachable', message: `${k} did not answer.`, hint: null, machine: k };
    }

    let flight = inFlight.get(k);
    if (!flight || force) {
      const gen = generationOf(k);
      const prevMeta = observedMeta.get(k);
      if (!existing || existing.status !== 'loading') {
        entries.set(k, {
          ...makeUnknownEntry(k),
          status: 'loading',
          version: existing?.version || prevMeta?.version || null,
          build: existing?.build || prevMeta?.build || null,
          generation: gen,
        });
      }
      const rawPromise = fetchRawCatalog(k).then(
        (res) => {
          if (inFlight.get(k)?.promise === rawPromise) {
            inFlight.delete(k);
          }
          // If this machine was invalidated while in-flight, do not cache stale tools;
          // re-fetch if not already populated so awaiting callers never get an empty entry.
          if (gen !== generationOf(k)) {
            const current = entries.get(k);
            if (current?.status === 'known') return current;
            return ensure(k, { signal, timeoutMs });
          }
          const toolMap = buildToolMap(res.tools);
          const entry = {
            machine: k,
            status: 'known',
            known: true,
            tools: res.tools,
            toolMap,
            allowedSet: res.allowedSet,
            version: res.version,
            build: res.build,
            schemaComplete: res.schemaComplete,
            error: null,
            fetchedAt: nowFn(),
            generation: gen,
          };
          entries.set(k, entry);
          observedMeta.set(k, {
            version: res.version,
            build: res.build,
            reachable: true,
          });
          notify();
          return entry;
        },
        (err) => {
          if (inFlight.get(k)?.promise === rawPromise) {
            inFlight.delete(k);
          }
          if (gen === generationOf(k)) {
            const unreachable = isUnreachableError(err);
            entries.set(k, {
              ...makeUnknownEntry(k),
              status: unreachable ? 'unreachable' : 'unknown',
              error: err,
              fetchedAt: nowFn(),
              generation: gen,
            });
            notify();
          }
          throw err;
        },
      );
      flight = { promise: rawPromise, generation: gen };
      inFlight.set(k, flight);
    }

    return withBoundedWait(flight.promise, { signal, timeoutMs, machine: k });
  }

  /**
   * Computes Files capabilities strictly for `machine` from its cached tool catalogue.
   */
  function fileCapabilities(machine, { fallbackAllowedSet = null } = {}) {
    const e = get(machine);
    return computeFileCapabilities({
      known: e.status === 'known',
      tools: e.tools,
      allowedSet: e.allowedSet || fallbackAllowedSet,
      link,
      machine: e.machine || null,
    });
  }

  function missingFeatures(machine) {
    return describeMissingFeatures(get(machine));
  }

  /**
   * Guarded tool call:
   * 1. Waits (bounded) for the target machine's capabilities if not known yet.
   * 2. Refuses without calling if the tool is absent or an essential argument is not declared.
   * 3. Drops undeclared optional refinements before calling and reports them in `dropped`.
   * 4. Second line of defence: if the node still replies `invalid_args` naming an unknown optional
   *    refinement field, drops it, corrects the cached capabilities, and retries once.
   */
  async function call(tool, args = {}, callOptions = {}) {
    const {
      on,
      optional = [],
      essential = null,
      signal,
      onEvent,
      timeoutMs = defaultTimeoutMs,
      onDrop = null,
    } = callOptions || {};

    const machine = keyOf(on);
    if (!machine) {
      const value = await link.call(tool, args, { on, signal, onEvent });
      return { value, dropped: [], machine: null };
    }

    if (!isKnown(machine)) {
      await ensure(machine, { signal, timeoutMs });
    }

    const entry = get(machine);
    if (entry.status !== 'known') {
      throw (
        entry.error || {
          code: 'unreachable',
          message: `${machine} did not answer.`,
          hint: null,
          machine,
        }
      );
    }

    if (!entry.toolMap.has(tool)) {
      throw makeNeedsNewerLegionError({ machine, tool });
    }

    const optionalSet = new Set(Array.isArray(optional) ? optional : []);
    const essentialSet = Array.isArray(essential) ? new Set(essential) : null;
    const sentArgs = {};
    const dropped = [];
    const missingEssential = [];

    for (const [argKey, argVal] of Object.entries(args || {})) {
      if (argVal === undefined) continue;
      if (acceptsArg(machine, tool, argKey)) {
        sentArgs[argKey] = argVal;
      } else if (optionalSet.has(argKey) && !(essentialSet && essentialSet.has(argKey))) {
        dropped.push(argKey);
      } else {
        missingEssential.push(argKey);
      }
    }

    if (missingEssential.length > 0) {
      throw makeNeedsNewerLegionError({ machine, tool, missingArgs: missingEssential });
    }

    if (dropped.length > 0 && typeof onDrop === 'function') {
      onDrop(sentArgs, dropped);
    }

    try {
      const value = await link.call(tool, sentArgs, { on: machine, signal, onEvent });
      return { value, dropped, machine };
    } catch (err) {
      if (err?.code === 'unknown_tool') {
        markToolAbsent(machine, tool);
        throw makeNeedsNewerLegionError({ machine, tool });
      }
      const parsed = parseUnknownFieldError(err);
      if (
        parsed &&
        optionalSet.has(parsed.field) &&
        Object.prototype.hasOwnProperty.call(sentArgs, parsed.field)
      ) {
        const retryArgs = { ...sentArgs };
        const newlyDropped = [];
        const expectedSet = Array.isArray(parsed.expected) ? new Set(parsed.expected) : null;

        delete retryArgs[parsed.field];
        markArgUnsupported(machine, tool, parsed.field);
        newlyDropped.push(parsed.field);

        if (expectedSet) {
          for (const k of Object.keys(retryArgs)) {
            if (optionalSet.has(k) && !expectedSet.has(k)) {
              delete retryArgs[k];
              markArgUnsupported(machine, tool, k);
              newlyDropped.push(k);
            }
          }
        }

        const allDropped = [...dropped, ...newlyDropped];
        if (typeof onDrop === 'function') {
          onDrop(retryArgs, allDropped);
        }

        const retryValue = await link.call(tool, retryArgs, { on: machine, signal, onEvent });
        return { value: retryValue, dropped: allDropped, machine };
      }
      throw err;
    }
  }

  function subscribe(listener) {
    if (typeof listener !== 'function') return () => {};
    listeners.add(listener);
    return () => {
      listeners.delete(listener);
    };
  }

  function dispose() {
    if (typeof unwatchContent === 'function') {
      unwatchContent();
      unwatchContent = null;
    }
    listeners.clear();
  }

  return {
    link,
    get,
    isKnown,
    hasTool,
    hasToolInCatalog,
    acceptsArg,
    getToolSpec,
    getTools,
    seed,
    syncMachineMeta,
    invalidate,
    renameMachine,
    markArgUnsupported,
    markToolAbsent,
    ensure,
    fileCapabilities,
    missingFeatures,
    call,
    subscribe,
    getRevision: () => revision,
    dispose,
  };
}
