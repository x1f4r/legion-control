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

function isPairingError(err) {
  const reason = err?.details?.reason;
  return (
    err?.code === 'denied' &&
    (reason === 'device_removed_or_unknown' ||
      reason === 'auth_failed' ||
      reason === 'host_key_mismatch')
  );
}

function readCachedNodeIdentity(nodeName) {
  if (!nodeName || typeof window === 'undefined') return null;
  try {
    const raw = window.localStorage?.getItem(`legion.nodeIdentity.${nodeName}`);
    if (!raw) return null;
    const parsed = JSON.parse(raw);
    if (!parsed || typeof parsed !== 'object') return null;
    return parsed;
  } catch {
    return null;
  }
}

function writeCachedNodeIdentity(nodeName, entry) {
  if (!nodeName || typeof window === 'undefined') return;
  try {
    window.localStorage?.setItem(`legion.nodeIdentity.${nodeName}`, JSON.stringify(entry));
  } catch {
    // Ignore storage quota or private-mode failures.
  }
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
  function ensureLocalNetworkPermission() {
    if (typeof window === 'undefined') return Promise.resolve(true);
    const android = window.__LEGION_ANDROID__;
    if (!android || typeof android.requestLocalNetwork !== 'function') {
      return Promise.resolve(true);
    }
    try {
      if (typeof android.hasLocalNetwork === 'function' && android.hasLocalNetwork() === true) {
        return Promise.resolve(true);
      }
      return Promise.resolve(android.requestLocalNetwork()).then(Boolean, () => true);
    } catch {
      return Promise.resolve(true);
    }
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

      const primary = base.nodes[0];
      const targetNode = primary.node;

      try {
        const [whoami, version] = await Promise.all([
          call('meta.whoami', {}, { on: targetNode }),
          call('meta.version', {}, { on: targetNode }).catch(() => null),
        ]);
        if (primary.via !== 'local' && whoami?.node && whoami.node !== targetNode) {
          throw {
            code: 'denied',
            message: `Expected identity from ${targetNode}, but ${whoami.node} answered.`,
            hint: 'Check the connection settings for this machine.',
            machine: targetNode,
            details: { reason: 'node_mismatch', expected: targetNode, actual: whoami.node },
          };
        }
        const checkedAt = new Date().toISOString();
        const latest = await rawState().catch(() => base);
        const nodes =
          Array.isArray(latest?.nodes) && latest.nodes.length > 0 ? latest.nodes : base.nodes;
        const currentPrimary = nodes[0]?.node === targetNode ? nodes[0] : primary;
        const resolvedPrincipal = whoami.principal || currentPrimary.principal;
        const resolvedRole = whoami.role || currentPrimary.role;
        writeCachedNodeIdentity(targetNode, {
          principal: resolvedPrincipal,
          role: resolvedRole,
          tools: Array.isArray(whoami.tools) ? whoami.tools : null,
          checkedAt,
        });
        const enriched = {
          ...currentPrimary,
          node: targetNode,
          principal: resolvedPrincipal,
          role: resolvedRole,
          reachable: true,
          whoamiStale: false,
          roleCheckedAt: checkedAt,
          unreachableError: null,
          version: version?.version || '',
          build: version?.build || null,
          protocol: version?.protocol ?? 1,
          whoami,
        };
        return {
          ...latest,
          nodes: [enriched, ...nodes.filter((n) => n.node !== targetNode)],
        };
      } catch (err) {
        const normalized = normalizeError(err);
        if (isPairingError(normalized)) {
          return {
            ...base,
            error: normalized,
          };
        }
        // Selected machine did not answer: keep it selected and mark it unreachable with its
        // last known role ("as of <when>"), never substituting another node's identity.
        const cached = readCachedNodeIdentity(targetNode);
        const lastRole = cached?.role || (primary.role && primary.role !== 'unknown' ? primary.role : null);
        const lastPrincipal = cached?.principal || primary.principal || '';
        const checkedAt = cached?.checkedAt || primary.role_checked_at || null;
        const offlinePrimary = {
          ...primary,
          node: targetNode,
          principal: lastPrincipal,
          role: lastRole || primary.role || 'unknown',
          reachable: false,
          whoamiStale: true,
          roleCheckedAt: checkedAt,
          unreachableError: normalized,
          whoami: lastRole
            ? {
                node: targetNode,
                principal: lastPrincipal,
                role: lastRole,
                tools: Array.isArray(cached?.tools) ? cached.tools : null,
                stale: true,
              }
            : null,
        };
        return {
          ...base,
          nodes: [offlinePrimary, ...base.nodes.slice(1)],
        };
      }
    } finally {
      signalReady();
    }
  }

  async function deviceName() {
    if (
      typeof window !== 'undefined' &&
      window.__LEGION_ANDROID__ &&
      typeof window.__LEGION_ANDROID__.deviceModel === 'function'
    ) {
      const model = String(window.__LEGION_ANDROID__.deviceModel() || '').trim();
      if (model) return model;
    }
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
      const paired = await invoke('pair', { invite: inviteText, name: name || null });
      const next = await state();
      return {
        ...next,
        last_paired: paired?.last_paired || next?.last_paired || null,
      };
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function learn() {
    try {
      return await invoke('learn');
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function acceptIdentityChange(node, fingerprints) {
    try {
      await invoke('accept_identity_change', {
        node,
        fingerprints: Array.isArray(fingerprints) ? fingerprints : [],
      });
      return await state();
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function dismissIdentityChange(node) {
    try {
      await invoke('dismiss_identity_change', { node });
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

  async function invalidateSessions() {
    try {
      await invoke('invalidate_sessions');
    } catch {
      // Best-effort teardown on resume; subsequent calls still reconnect on error.
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

  async function updateState() {
    try {
      return await invoke('update_state');
    } catch (err) {
      throw normalizeError(err);
    }
  }

  function watchUpdates(onState) {
    const core = tauriCore();
    if (!core || typeof core.Channel !== 'function' || typeof onState !== 'function') {
      return () => {};
    }
    let active = true;
    const channel = new core.Channel();
    channel.onmessage = (st) => {
      if (!active) return;
      try {
        onState(st);
      } catch {
        // Ignore view callback errors.
      }
    };
    invoke('update_watch', { onState: channel }).catch(() => {});
    return () => {
      active = false;
    };
  }

  async function checkUpdates() {
    try {
      return await invoke('update_check');
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function installUpdate() {
    try {
      return await invoke('update_install');
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function setAutoUpdate(enabled) {
    try {
      return await invoke('update_set_auto', { enabled: Boolean(enabled) });
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function dismissUpdateNotice() {
    try {
      return await invoke('update_dismiss_notice');
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function maybeAutoInstall() {
    try {
      return await invoke('update_maybe_auto_install');
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function contentUrl(req = {}) {
    const { machine, path, download = false, signal } = req || {};
    if (signal?.aborted) {
      throw {
        code: 'cancelled',
        message: 'The content request was cancelled.',
        hint: null,
        machine: machine || null,
        details: null,
      };
    }
    try {
      const result = await invoke('content_url', {
        machine: machine || '',
        path: path || '',
        download: Boolean(download),
      });
      if (signal?.aborted) {
        throw {
          code: 'cancelled',
          message: 'The content request was cancelled.',
          hint: null,
          machine: machine || null,
          details: null,
        };
      }
      return result;
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function download(req = {}) {
    const { machine, path, name, size, mime } = req || {};
    try {
      return await invoke('transfer_download', {
        machine: machine || '',
        path: path || '',
        name: name ?? null,
        size: typeof size === 'number' && Number.isFinite(size) ? size : null,
        mime: mime ?? null,
      });
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function upload(req = {}) {
    const { machine, dir, files } = req || {};
    try {
      return await invoke('transfer_upload', {
        machine: machine || '',
        dir: dir || '',
        files: Array.isArray(files) ? files : null,
      });
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function openExternally(req = {}) {
    const { machine, path, name, mime } = req || {};
    try {
      return await invoke('transfer_open_externally', {
        machine: machine || '',
        path: path || '',
        name: name ?? null,
        mime: mime ?? null,
      });
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function transfers() {
    try {
      const list = await invoke('transfers');
      return Array.isArray(list) ? list : [];
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function transferControl(id, action) {
    try {
      return await invoke('transfer_control', { id, action });
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function openTransfer(id) {
    try {
      return await invoke('open_transfer', { id });
    } catch (err) {
      throw normalizeError(err);
    }
  }

  function onTransferEvent(listener) {
    const core = tauriCore();
    if (!core || typeof core.Channel !== 'function' || typeof listener !== 'function') {
      return () => {};
    }
    let active = true;
    const channel = new core.Channel();
    channel.onmessage = (list) => {
      if (!active) return;
      try {
        listener(Array.isArray(list) ? list : []);
      } catch {
        // Keep receiving queue updates if a view callback throws.
      }
    };
    invoke('transfer_watch', { onEvent: channel }).catch(() => {});
    return () => {
      active = false;
    };
  }

  async function transferSettings() {
    try {
      return await invoke('transfer_settings');
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function setTransferSettings(req = {}) {
    try {
      return await invoke('transfer_set_settings', {
        wifiOnlyLarge: Boolean(req?.wifi_only_large ?? req?.wifiOnlyLarge),
      });
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
    learn,
    acceptIdentityChange,
    dismissIdentityChange,
    repairStore,
    invalidateSessions,
    scanInvite,
    updateState,
    watchUpdates,
    checkUpdates,
    installUpdate,
    setAutoUpdate,
    dismissUpdateNotice,
    maybeAutoInstall,
    contentUrl,
    download,
    upload,
    openExternally,
    transfers,
    transferControl,
    openTransfer,
    onTransferEvent,
    transferSettings,
    setTransferSettings,
  };
}
