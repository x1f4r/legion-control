// Native app link: calls the Tauri 2 shell through window.__TAURI__.core.invoke and Channel.

function tauriCore() {
  const tauri = typeof window !== 'undefined' ? window.__TAURI__ : null;
  if (!tauri || !tauri.core || typeof tauri.core.invoke !== 'function') {
    return null;
  }
  return tauri.core;
}

function invoke(command, payload) {
  const core = tauriCore();
  if (!core) {
    return Promise.reject({
      code: 'unsupported',
      message: 'The native Legion shell is not available in this window.',
      hint: null,
      machine: null,
      details: null,
    });
  }
  return core.invoke(command, payload);
}

function normalizeError(err) {
  if (err && typeof err === 'object' && typeof err.message === 'string') {
    return {
      code: err.code || 'failed',
      message: err.message,
      hint: err.hint || null,
      machine: err.machine || null,
      details: err.details || null,
    };
  }
  return {
    code: 'failed',
    message: typeof err === 'string' ? err : 'The native command failed.',
    hint: null,
    machine: null,
    details: null,
  };
}

function randomCallPrefix() {
  if (typeof crypto !== 'undefined' && typeof crypto.getRandomValues === 'function') {
    const bytes = new Uint8Array(8);
    crypto.getRandomValues(bytes);
    return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
  }
  return Math.random().toString(16).slice(2, 10) + Date.now().toString(16);
}

export function createAppLink() {
  const callPrefix = randomCallPrefix();
  let nextCallId = 1;
  let readySent = false;

  function signalReady() {
    if (readySent) return;
    readySent = true;
    let fired = false;
    const fire = () => {
      if (fired) return;
      fired = true;
      invoke('ready').catch(() => {});
    };
    if (typeof window !== 'undefined' && typeof window.requestAnimationFrame === 'function') {
      window.requestAnimationFrame(() => window.requestAnimationFrame(fire));
      window.setTimeout(fire, 250);
    } else {
      fire();
    }
  }
  let nextPermRequestId = 1;
  const pendingPermRequests = new Map();

  if (typeof window !== 'undefined' && !window.__LEGION_ANDROID_PERMISSIONS_RESOLVE__) {
    window.__LEGION_ANDROID_PERMISSIONS_RESOLVE__ = (requestId, granted) => {
      const resolve = pendingPermRequests.get(requestId);
      if (resolve) {
        pendingPermRequests.delete(requestId);
        resolve(Boolean(granted));
      }
    };
  }

  function ensureLocalNetworkPermission() {
    if (typeof window === 'undefined') return Promise.resolve(true);
    const bridge = window.__LEGION_ANDROID_PERMISSIONS__;
    if (!bridge || typeof bridge.hasLocalNetwork !== 'function') {
      return Promise.resolve(true);
    }
    try {
      if (bridge.hasLocalNetwork()) return Promise.resolve(true);
    } catch {
      return Promise.resolve(true);
    }
    if (typeof bridge.requestLocalNetwork !== 'function') {
      return Promise.resolve(true);
    }
    return new Promise((resolve) => {
      const id = nextPermRequestId++;
      pendingPermRequests.set(id, resolve);
      try {
        bridge.requestLocalNetwork(id);
      } catch {
        pendingPermRequests.delete(id);
        resolve(true);
      }
    });
  }

  async function rawState() {
    try {
      return await invoke('state');
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function tools(options = {}) {
    try {
      return await invoke('tools', { on: options?.on ?? null });
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function call(tool, args = {}, options = {}) {
    const { on, onEvent, signal } = options || {};
    const id = `${callPrefix}-${nextCallId++}`;
    const core = tauriCore();

    const channel = core && typeof core.Channel === 'function' ? new core.Channel() : null;
    if (channel && typeof onEvent === 'function') {
      channel.onmessage = (evt) => {
        try {
          onEvent(evt);
        } catch {
          // Keep receiving events if a view callback throws.
        }
      };
    }

    const onAbort = () => {
      invoke('cancel', { id }).catch(() => {});
    };
    if (signal) {
      if (signal.aborted) {
        throw {
          code: 'cancelled',
          message: 'The call was cancelled.',
          hint: null,
          machine: null,
          details: null,
        };
      }
      signal.addEventListener('abort', onAbort, { once: true });
    }

    try {
      const payload = {
        id,
        tool,
        args: args && typeof args === 'object' ? args : {},
        on: on ?? null,
      };
      if (channel) {
        payload.onEvent = channel;
      }
      return await invoke('call', payload);
    } catch (err) {
      throw normalizeError(err);
    } finally {
      if (signal) signal.removeEventListener('abort', onAbort);
    }
  }

  async function state() {
    try {
      const base = await rawState();
      if (!base?.ready || !Array.isArray(base.nodes) || base.nodes.length === 0) {
        return base;
      }

      await ensureLocalNetworkPermission();

      try {
        const [whoami, version] = await Promise.all([
          call('meta.whoami', {}),
          call('meta.version', {}).catch(() => null),
        ]);
        // Re-read backend state in case failover switched the active node during the probe.
        const latest = await rawState().catch(() => base);
        const nodes = Array.isArray(latest?.nodes) && latest.nodes.length > 0 ? latest.nodes : base.nodes;
        const primary = nodes[0];
        const enriched = {
          ...primary,
          node: whoami.node || primary.node,
          principal: whoami.principal || primary.principal,
          role: whoami.role || primary.role,
          version: version?.version || '',
          build: version?.build || null,
          protocol: version?.protocol ?? 1,
          whoami,
        };
        return {
          ...latest,
          nodes: [enriched, ...nodes.slice(1)],
        };
      } catch (err) {
        return {
          ...base,
          error: normalizeError(err),
        };
      }
    } finally {
      signalReady();
    }
  }

  async function deviceName() {
    try {
      const name = await invoke('device_name');
      return typeof name === 'string' ? name : '';
    } catch {
      return '';
    }
  }

  async function pair(inviteText, name) {
    const granted = await ensureLocalNetworkPermission();
    if (!granted) {
      throw {
        code: 'denied',
        message: 'Local network permission was denied.',
        hint: 'Allow local network access for Legion in Android Settings to connect to machines on your LAN or Tailscale.',
        machine: null,
        details: null,
      };
    }
    try {
      await invoke('pair', { invite: inviteText, name: name || null });
      return await state();
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function unpair(node) {
    try {
      return await invoke('unpair', { node });
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function selectNode(node) {
    try {
      await invoke('select_node', { node });
      return await state();
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function repairStore() {
    try {
      const result = await invoke('repair_store');
      const nextState = await state();
      return {
        archived_to: result?.archived_to || '',
        message: result?.message || '',
        state: nextState,
      };
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function scanInvite() {
    try {
      const reply = await invoke('scan_invite');
      if (typeof reply === 'string') return reply;
      if (reply && (reply.use_plugin || reply.usePlugin || reply.mode === 'use_plugin')) {
        const scanned = await invoke('plugin:barcode-scanner|scan', {
          windowed: false,
          formats: ['QR_CODE'],
        });
        if (typeof scanned === 'string') return scanned;
        return scanned?.content || '';
      }
      return reply?.invite || reply?.content || '';
    } catch (err) {
      throw normalizeError(err);
    }
  }

  return {
    kind: 'app',
    state,
    tools,
    call,
    deviceName,
    pair,
    unpair,
    selectNode,
    repairStore,
    scanInvite,
  };
}
