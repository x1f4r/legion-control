// Whether a service is answering.
//
// Health is not the same question as "is the process there". A process that is
// up but not serving is exactly the state the update cycle has to notice, which
// is why the http probe wants a 200 and not just an open socket. Status keeps
// the two apart and reports both: `process` says the thing exists, `health` says
// it works.

import { describeFailure, runArgv, runArgvAsync } from '../config.mjs';
import { httpGet } from '../http.mjs';

/** The port a service answers on, or null when health is not measured over HTTP. */
export function healthPort(service) {
  return service.health?.type === 'http' ? service.health.port : null;
}

function shape(ok, { status = 0, error = null, startedAt }) {
  return {
    ok,
    status,
    error,
    checkedAt: new Date().toISOString(),
    elapsedMs: startedAt === undefined ? 0 : Math.max(0, Date.now() - startedAt),
  };
}

/**
 * One health check. `running` is passed in because health type "none" means
 * "healthy whenever the process is running", so that answer is already known by
 * the caller and asking twice would only cost another probe.
 *
 * `async` decides which runner a command health check uses. Mutations want the
 * synchronous one — they are a sequence and nothing else is waiting — while
 * status wants the asynchronous one so several services can be checked inside
 * one shared budget instead of adding up.
 */
export async function checkHealth(service, { running = true, timeoutMs = null, useAsync = false } = {}) {
  const startedAt = Date.now();
  const probe = service.health ?? { type: 'none' };
  const budget = timeoutMs ?? (probe.timeoutSeconds ?? 5) * 1000;

  if (probe.type === 'http') {
    if (budget <= 0) return shape(false, { error: 'there was no time left in the budget for a health check', startedAt });
    const response = await httpGet({ host: probe.host, port: probe.port, path: probe.path, timeoutMs: budget });
    return shape(response.ok, {
      status: response.status,
      error: response.ok ? null : response.error ?? `HTTP ${response.status}`,
      startedAt,
    });
  }

  if (probe.type === 'command') {
    if (budget <= 0) return shape(false, { error: 'there was no time left in the budget for a health check', startedAt });
    const result = useAsync
      ? await runArgvAsync(probe.command, { timeoutMs: budget })
      : runArgv(probe.command, { timeoutMs: budget });
    return shape(result.ok, { error: result.ok ? null : describeFailure(result), startedAt });
  }

  return shape(running === true, { error: running === true ? null : `${service.name} is not running`, startedAt });
}

/** How a health failure reads in a message. */
export function describeHealth(health) {
  return health.error ?? `HTTP ${health.status}`;
}
