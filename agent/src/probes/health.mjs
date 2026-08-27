// Whether a service is answering.
//
// Health is not the same question as "is the process there". A process that is
// up but not serving is exactly the state the update cycle has to notice, which
// is why the http probe wants a 200 and not just an open socket.

import { describeFailure, runArgv } from '../config.mjs';
import { httpGet } from '../http.mjs';

/** The port a service answers on, or null when health is not measured over HTTP. */
export function healthPort(service) {
  return service.health?.type === 'http' ? service.health.port : null;
}

/**
 * One health check. `running` is passed in because health type "none" means
 * "healthy whenever the process is running", so that answer is already known by
 * the caller and asking twice would only cost another probe.
 */
export async function checkHealth(service, { running = true, timeoutMs = 5000 } = {}) {
  const probe = service.health ?? { type: 'none' };
  if (probe.type === 'http') {
    const response = await httpGet({
      host: probe.host,
      port: probe.port,
      path: probe.path,
      timeoutMs,
    });
    return { ok: response.ok, status: response.status, error: response.ok ? null : response.error ?? `HTTP ${response.status}` };
  }
  if (probe.type === 'command') {
    const result = runArgv(probe.command, { timeoutMs });
    return { ok: result.ok, status: 0, error: result.ok ? null : describeFailure(result) };
  }
  return {
    ok: running === true,
    status: 0,
    error: running === true ? null : `${service.name} is not running`,
  };
}

/** How a health failure reads in a message. */
export function describeHealth(health) {
  return health.error ?? `HTTP ${health.status}`;
}
