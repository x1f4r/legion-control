// Can anything OUTSIDE this machine actually reach the service?
//
// This is a different question from every other probe, and keeping it separate
// is the point. The process probe says something exists; the health probe says
// it answers on localhost; the relay probe says a tunnel process is running.
// None of them says the thing a person actually cares about, which is whether
// the address they use works — and a relay process that is running while its
// tunnel is broken looks perfectly healthy to all three.
//
// It is never part of an ordinary status poll. Turning the request people make
// twenty times a day into an Internet round trip would make it slower and
// flakier for no gain, and would send traffic through somebody's relay every few
// seconds. It runs from `doctor --deep`, where the question was asked out loud.

import http from 'node:http';
import https from 'node:https';

/**
 * One GET against the configured endpoint. Never rejects; every failure is a
 * value, because a probe that throws turns an unreachable service into a crashed
 * agent.
 */
export function checkEndpoint(endpoint, { timeoutMs = null } = {}) {
  return new Promise((resolve) => {
    const startedAt = Date.now();
    const budget = timeoutMs ?? endpoint.timeoutSeconds * 1000;
    let settled = false;
    const finish = (value) => {
      if (settled) return;
      settled = true;
      resolve({ ...value, url: endpoint.url, checkedAt: new Date().toISOString(), elapsedMs: Date.now() - startedAt });
    };

    let target;
    try {
      target = new URL(endpoint.url);
    } catch (err) {
      finish({ reachable: false, status: 0, error: `${endpoint.url} is not a usable URL: ${err.message}` });
      return;
    }

    const transport = target.protocol === 'https:' ? https : http;
    const request = transport.get(
      target,
      { timeout: budget, headers: { 'user-agent': 'legionctl', connection: 'close' } },
      (response) => {
        const status = response.statusCode ?? 0;
        response.resume();
        const reachable = status === endpoint.expectStatus;
        finish({
          reachable,
          status,
          error: reachable ? null : `answered HTTP ${status}, expected ${endpoint.expectStatus}`,
        });
      },
    );
    request.on('timeout', () => {
      request.destroy();
      finish({ reachable: false, status: 0, error: `no answer within ${budget} ms` });
    });
    request.on('error', (err) => finish({ reachable: false, status: 0, error: err.message }));
  });
}
