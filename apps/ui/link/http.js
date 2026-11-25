// HTTP link: talks to /v1/* on the origin that served the page, with a bearer token.

import { isFanOut } from '../lib/result.js';
import { createSseParser } from '../lib/sse.js';

const TOKEN_KEY = 'legion.token';

function toError(body, status) {
  if (body && typeof body === 'object' && typeof body.message === 'string') {
    return {
      code: body.code || (status === 401 ? 'denied' : 'failed'),
      message: body.message,
      hint: body.hint || null,
      machine: body.machine || null,
      status: status || null,
    };
  }
  return {
    code: status === 401 ? 'denied' : 'failed',
    message: status ? `The server answered with status ${status}.` : 'The request failed.',
    hint: null,
    machine: null,
    status: status || null,
  };
}

function readToken() {
  try {
    return localStorage.getItem(TOKEN_KEY) || '';
  } catch {
    return '';
  }
}

function writeToken(token) {
  try {
    if (token) localStorage.setItem(TOKEN_KEY, token);
    else localStorage.removeItem(TOKEN_KEY);
  } catch {
    // Ignore storage failures.
  }
}

export function createHttpLink() {
  let token = readToken();
  let doorSupportsContent = false;
  const unsupportedContentMachines = new Set();
  const contentSupportListeners = new Set();

  function machineKey(machine) {
    return typeof machine === 'string' ? machine.trim() : '';
  }

  function notifyContentSupport() {
    for (const listener of contentSupportListeners) {
      try {
        listener();
      } catch {
        // Keep notifying remaining listeners.
      }
    }
  }

  function markContentUnsupported(machine) {
    const key = machineKey(machine);
    if (!key) {
      if (doorSupportsContent) {
        doorSupportsContent = false;
        notifyContentSupport();
      }
      return;
    }
    if (!unsupportedContentMachines.has(key)) {
      unsupportedContentMachines.add(key);
      notifyContentSupport();
    }
  }

  function supportsContentFor(machine) {
    if (!doorSupportsContent) return false;
    const key = machineKey(machine);
    return !key || !unsupportedContentMachines.has(key);
  }

  function onContentSupportChange(listener) {
    if (typeof listener !== 'function') return () => {};
    contentSupportListeners.add(listener);
    return () => {
      contentSupportListeners.delete(listener);
    };
  }

  async function fetchHealth() {
    try {
      const res = await fetch('/v1/health', {
        headers: { Accept: 'application/json' },
      });
      if (!res.ok) return null;
      const health = await res.json();
      const next = health?.content === true;
      if (next !== doorSupportsContent) {
        doorSupportsContent = next;
        notifyContentSupport();
      }
      return health;
    } catch {
      return null;
    }
  }

  async function call(tool, args = {}, options = {}) {
    const { on, onEvent, signal } = options || {};
    const current = token || readToken();
    if (!current) {
      throw {
        code: 'denied',
        message: 'This browser does not have a token yet.',
        hint: 'Enter a token made with legion token.add.',
        machine: null,
        status: 401,
      };
    }

    const body = { tool, args: args && typeof args === 'object' ? args : {} };
    if (on !== undefined && on !== null && on !== '') {
      body.on = on;
    }

    const wantStream = typeof onEvent === 'function';
    let res;
    try {
      res = await fetch('/v1/call', {
        method: 'POST',
        headers: {
          Authorization: `Bearer ${current}`,
          'Content-Type': 'application/json',
          Accept: wantStream ? 'text/event-stream' : 'application/json',
        },
        body: JSON.stringify(body),
        signal,
      });
    } catch (err) {
      if (err && err.name === 'AbortError') {
        throw {
          code: 'cancelled',
          message: 'The call was cancelled.',
          hint: null,
          machine: null,
        };
      }
      throw {
        code: 'unreachable',
        message: 'Could not reach the server.',
        hint: 'Check that legion serve is running and reachable from this browser.',
        machine: null,
      };
    }

    const contentType = (res.headers.get('content-type') || '').toLowerCase();
    if (wantStream && res.ok && contentType.startsWith('text/event-stream') && res.body) {
      const reader = res.body.getReader();
      const decoder = new TextDecoder();
      let settled = false;
      let outcome = null;
      let failure = null;

      const parser = createSseParser((frame) => {
        if (frame.event === 'event') {
          try {
            onEvent(frame.data);
          } catch {
            // Keep reading stream even if a UI callback throws.
          }
        } else if (frame.event === 'result') {
          settled = true;
          outcome = frame.data;
        } else if (frame.event === 'error') {
          settled = true;
          if (isFanOut(frame.data)) {
            outcome = frame.data;
          } else {
            failure = toError(frame.data, res.status);
          }
        }
      });

      try {
        while (true) {
          const { done, value } = await reader.read();
          if (done) break;
          parser.push(decoder.decode(value, { stream: true }));
        }
        parser.push(decoder.decode());
        parser.finish();
      } catch (err) {
        try {
          await reader.cancel();
        } catch {
          // Ignore cancel error.
        }
        if (err && err.name === 'AbortError') {
          throw {
            code: 'cancelled',
            message: 'The call was cancelled.',
            hint: null,
            machine: null,
          };
        }
        throw {
          code: 'unreachable',
          message: 'The event stream ended unexpectedly.',
          hint: null,
          machine: null,
        };
      }

      if (failure) throw failure;
      if (settled) return outcome;
      throw {
        code: 'internal',
        message: 'The server closed the stream without a final result.',
        hint: null,
        machine: null,
      };
    }

    let parsed = null;
    try {
      parsed = await res.json();
    } catch {
      parsed = null;
    }

    if (res.ok) return parsed;
    if (isFanOut(parsed)) return parsed;
    throw toError(parsed, res.status);
  }

  async function tools() {
    const current = token || readToken();
    if (!current) {
      throw {
        code: 'denied',
        message: 'This browser does not have a token yet.',
        hint: null,
        machine: null,
        status: 401,
      };
    }
    let res;
    try {
      res = await fetch('/v1/tools?all=1', {
        headers: {
          Authorization: `Bearer ${current}`,
          Accept: 'application/json',
        },
      });
    } catch {
      throw {
        code: 'unreachable',
        message: 'Could not reach the server.',
        hint: null,
        machine: null,
      };
    }
    let parsed = null;
    try {
      parsed = await res.json();
    } catch {
      parsed = null;
    }
    if (!res.ok) throw toError(parsed, res.status);
    return Array.isArray(parsed?.tools) ? parsed.tools : [];
  }

  async function state() {
    const health = await fetchHealth();
    const current = token || readToken();
    if (!current) {
      return { ready: false, nodes: [], needs: 'token', health };
    }
    try {
      const [whoami, version] = await Promise.all([
        call('meta.whoami', {}),
        call('meta.version', {}).catch(() => null),
      ]);
      return {
        ready: true,
        needs: null,
        health,
        nodes: [
          {
            node: whoami.node || health?.node || 'node',
            principal: whoami.principal,
            role: whoami.role,
            via: 'http',
            origin: window.location.origin,
            version: version?.version || health?.version || '',
            build: version?.build || null,
            protocol: version?.protocol ?? 1,
            whoami,
          },
        ],
      };
    } catch (err) {
      if (err && err.status === 401) {
        return { ready: false, nodes: [], needs: 'token', health, error: err };
      }
      return { ready: false, nodes: [], needs: 'token', health, error: err };
    }
  }

  async function setToken(nextToken) {
    const trimmed = String(nextToken || '').trim();
    if (!trimmed) {
      throw {
        code: 'invalid_args',
        message: 'Enter a bearer token first.',
        hint: 'Create one on the node with legion token.add.',
        machine: null,
      };
    }
    const previous = token;
    token = trimmed;
    writeToken(trimmed);
    const nextState = await state();
    if (!nextState.ready) {
      token = previous;
      writeToken(previous);
      throw (
        nextState.error || {
          code: 'denied',
          message: 'That token was refused by the server.',
          hint: 'Check the token and try again.',
          machine: null,
        }
      );
    }
    return nextState;
  }

  async function forget() {
    token = '';
    writeToken('');
    if (unsupportedContentMachines.size > 0) {
      unsupportedContentMachines.clear();
      notifyContentSupport();
    }
  }

  async function contentUrl(options = {}) {
    const { machine, path, download = false, signal } = options || {};
    const current = token || readToken();
    if (!current) {
      throw {
        code: 'denied',
        message: 'This browser does not have a token yet.',
        hint: 'Enter a token made with legion token.add.',
        machine: null,
        status: 401,
      };
    }
    if (typeof path !== 'string' || path.trim() === '') {
      throw {
        code: 'invalid_args',
        message: 'Choose a file path first.',
        hint: null,
        machine: machine || null,
      };
    }

    const body = { path };
    if (machine !== undefined && machine !== null && machine !== '') {
      body.on = machine;
    }

    let res;
    try {
      res = await fetch('/v1/content', {
        method: 'POST',
        headers: {
          Authorization: `Bearer ${current}`,
          'Content-Type': 'application/json',
          Accept: 'application/json',
        },
        body: JSON.stringify(body),
        signal,
      });
    } catch (err) {
      if (err && err.name === 'AbortError') {
        throw {
          code: 'cancelled',
          message: 'The request was cancelled.',
          hint: null,
          machine: machine || null,
        };
      }
      throw {
        code: 'unreachable',
        message: 'Could not reach the server.',
        hint: 'Check that legion serve is running and reachable from this browser.',
        machine: machine || null,
      };
    }

    let parsed = null;
    try {
      parsed = await res.json();
    } catch {
      parsed = null;
    }
    if (!res.ok) {
      const err = toError(parsed, res.status);
      if (err.code === 'unsupported') {
        const target = machineKey(machine) || machineKey(err.machine);
        markContentUnsupported(target);
        if (err.machine && machineKey(err.machine) !== target) {
          markContentUnsupported(err.machine);
        }
      }
      throw err;
    }

    let url = typeof parsed?.url === 'string' ? parsed.url : '';
    if (!url) {
      throw {
        code: 'internal',
        message: 'The server returned no content address.',
        hint: null,
        machine: machine || null,
      };
    }
    if (download) {
      url += url.includes('?') ? '&download=1' : '?download=1';
    }
    return {
      url,
      expires_in: typeof parsed.expires_in === 'number' ? parsed.expires_in : null,
    };
  }

  let browserDownloadSeq = 1;

  /**
   * Non-blocking browser download using a ticketed `?download=1` content address.
   */
  async function download({ machine, path, name } = {}) {
    const id = `browser-dl-${browserDownloadSeq++}`;
    const res = await contentUrl({ machine, path, download: true });
    if (res?.url && typeof document !== 'undefined') {
      const a = document.createElement('a');
      a.href = res.url;
      a.download = name || '';
      a.rel = 'noopener noreferrer';
      document.body.appendChild(a);
      a.click();
      a.remove();
    }
    return { id };
  }

  return {
    kind: 'http',
    get supportsContent() {
      return doorSupportsContent;
    },
    supportsContentFor,
    onContentSupportChange,
    state,
    tools,
    call,
    setToken,
    forget,
    contentUrl,
    download,
  };
}
