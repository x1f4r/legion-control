// One plain HTTP GET, used by the health and busy probes.
//
// It never rejects. Every failure mode a probe cares about — refused connection,
// timeout, a body that never ends — comes back as a value, because a probe that
// throws is a probe that turns an unreachable service into a crashed agent.

import http from 'node:http';

const MAX_BODY_BYTES = 1024 * 1024;

/** GET http://host:port/path. Resolves to { ok, status, body, error }; never rejects. */
export function httpGet({ host = '127.0.0.1', port, path = '/', timeoutMs = 5000, wantBody = false } = {}) {
  return new Promise((resolve) => {
    let settled = false;
    let timer;
    const finish = (value) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      resolve(value);
    };

    if (!Number.isInteger(port) || port <= 0) {
      finish({ ok: false, status: 0, body: null, error: 'no port is configured' });
      return;
    }
    if (timeoutMs <= 0) {
      finish({ ok: false, status: 0, body: null, error: 'there was no time left in the HTTP probe budget' });
      return;
    }

    const request = http.get(
      { host, port, path, timeout: timeoutMs, headers: { connection: 'close' } },
      (response) => {
        const status = response.statusCode ?? 0;
        if (!wantBody) {
          response.resume();
          response.on('end', () => finish({ ok: status === 200, status, body: null, error: null }));
          response.on('error', (err) => finish({ ok: false, status, body: null, error: err.message }));
          return;
        }
        let body = '';
        response.setEncoding('utf8');
        response.on('data', (chunk) => {
          if (body.length > MAX_BODY_BYTES) return;
          body += chunk;
        });
        response.on('end', () => finish({ ok: status === 200, status, body, error: null }));
        response.on('error', (err) => finish({ ok: false, status, body: null, error: err.message }));
      },
    );
    const timedOut = () => {
      request.destroy();
      finish({ ok: false, status: 0, body: null, error: `${host}:${port}${path} timed out after ${timeoutMs} ms` });
    };
    timer = setTimeout(timedOut, timeoutMs);
    request.on('timeout', timedOut);
    request.on('error', (err) => finish({ ok: false, status: 0, body: null, error: err.message }));
  });
}
