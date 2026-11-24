// Native app link: calls the Tauri 2 shell through window.__TAURI__.core.invoke.

function invoke(command, payload) {
  const tauri = typeof window !== 'undefined' ? window.__TAURI__ : null;
  if (!tauri || !tauri.core || typeof tauri.core.invoke !== 'function') {
    return Promise.reject({
      code: 'unsupported',
      message: 'The native Legion shell is not available in this window.',
      hint: null,
      machine: null,
    });
  }
  return tauri.core.invoke(command, payload);
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
    const callId = `c${nextCallId++}`;
    let unlisten = null;

    if (typeof onEvent === 'function' && window.__TAURI__?.event?.listen) {
      try {
        unlisten = await window.__TAURI__.event.listen(`legion:event:${callId}`, (evt) => {
          onEvent(evt?.payload ?? evt);
        });
      } catch {
        unlisten = null;
      }
    }

    const onAbort = () => {
      invoke('cancel', { callId }).catch(() => {});
    };
    if (signal) {
      if (signal.aborted) {
        if (unlisten) unlisten();
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
      return await invoke('call', {
        callId,
        tool,
        args: args && typeof args === 'object' ? args : {},
        on: on ?? null,
      });
    } catch (err) {
      throw normalizeError(err);
    } finally {
      if (signal) signal.removeEventListener('abort', onAbort);
      if (typeof unlisten === 'function') unlisten();
    }
  }

  async function pair(inviteText, deviceName) {
    try {
      return await invoke('pair', { inviteText, deviceName });
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
      return await invoke('scanInvite');
    } catch (err) {
      throw normalizeError(err);
    }
  }

  return {
    kind: 'app',
    state,
    tools,
    call,
    pair,
    unpair,
    scanInvite,
  };
}
