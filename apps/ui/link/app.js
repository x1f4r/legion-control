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
    };
  }
  return {
    code: 'failed',
    message: typeof err === 'string' ? err : 'The native command failed.',
    hint: null,
    machine: null,
  };
}

export function createAppLink() {
  let nextCallId = 1;

  async function state() {
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
    const id = `c${nextCallId++}`;
    const core = tauriCore();

    let channel = null;
    if (typeof onEvent === 'function' && core && typeof core.Channel === 'function') {
      channel = new core.Channel();
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

  async function deviceName() {
    try {
      const name = await invoke('device_name');
      return typeof name === 'string' ? name : '';
    } catch {
      return '';
    }
  }

  async function pair(inviteText, name) {
    try {
      await invoke('pair', { inviteText, deviceName: name });
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
    scanInvite,
  };
}
