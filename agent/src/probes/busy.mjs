// The busy gate.
//
// "Busy" means something would be lost by stopping the service, and this gate
// sits in front of every disruptive action: update, restart, boot, sleep,
// actions. IT FAILS CLOSED EVERYWHERE. A probe that cannot be read counts as
// busy, because deferring costs a delay and the next cycle tries again, while
// acting on a misread costs the user their work.
//
// The reply says WHERE the answer came from, which is the whole fix. The first
// version had one boolean plus an "unknown" flag, and everything it could not
// classify — an unrecognised probe type, a command probe with no command, a
// service nobody had described at all — collapsed to "none", which read as idle.
// The gate's central promise had a silent exception in it. Now every answer
// carries `evidence`:
//
//   t3-sqlite | command | http   a probe ran and answered.
//   none                          the config says out loud there is nothing to
//                                 protect. Permits action, and is not warned
//                                 about, because somebody decided it.
//   unmonitored                   nobody said. Reported honestly as
//                                 monitored:false, warned about in status and
//                                 doctor, and it does NOT permit a disruptive
//                                 action.
//   probe-error | timed-out       the probe could not be completed. Blocks.
//
// A configuration mistake — unknown type, command with no argv, http with no
// port — never reaches here at all: config loading refuses it.

import { deadline as makeDeadline, describeFailure, mapWithLimit, runArgvAsync } from '../config.mjs';
import { REASON } from '../contract.mjs';
import { httpGet } from '../http.mjs';
import { checkT3Sqlite } from './t3-sqlite.mjs';

/** How many probes may be in flight at once. */
export const BUSY_CONCURRENCY = 4;
/** The whole aggregate has this long when a mutation is asking. */
export const BUSY_BUDGET_MS = 24000;
/** The default per-probe ceiling; a service's busy.timeoutSeconds overrides it. */
export const BUSY_PROBE_MS = 20000;
/** Status is a different question with a much tighter budget. */
export const STATUS_PROBE_MS = 8000;

function answer(state, { busy, monitored, evidence, reason, error = null, extra = {}, startedAt }) {
  return {
    busy,
    unknown: state === 'unknown',
    monitored,
    reason,
    evidence,
    checkedAt: new Date().toISOString(),
    elapsedMs: startedAt === undefined ? 0 : Math.max(0, Date.now() - startedAt),
    error,
    runningTurns: 0,
    pendingTurns: 0,
    pendingApprovals: 0,
    staleTurns: 0,
    staleApprovals: 0,
    threads: [],
    threadsTruncated: 0,
    ...extra,
  };
}

export function idle(evidence, reason = 'idle', startedAt = undefined) {
  return answer('idle', { busy: false, monitored: true, evidence, reason, startedAt });
}

export function busyAnswer(evidence, reason, startedAt = undefined) {
  return answer('busy', { busy: true, monitored: true, evidence, reason, startedAt });
}

/** Declared never busy. Permits action, and nobody is warned about it. */
export function declaredIdle(startedAt = undefined) {
  return answer('idle', { busy: false, monitored: false, evidence: 'none', reason: 'declared never busy', startedAt });
}

/** Nobody said. Blocks, and says exactly what to write. */
export function unmonitored(serviceName, startedAt = undefined) {
  return answer('unknown', {
    busy: true,
    monitored: false,
    evidence: 'unmonitored',
    reason: 'not monitored',
    error:
      `${serviceName} has no "busy" block, so there is no way to tell whether stopping it would lose work. ` +
      'Add "busy": {"type": "none"} if there is nothing to protect, or a probe (t3-sqlite, command, http) if there is.',
    startedAt,
  });
}

export function unreadable(evidence, error, startedAt = undefined) {
  return answer('unknown', { busy: true, monitored: true, evidence, reason: 'busy state unknown', error, startedAt });
}

/**
 * Follow a dotted path into a parsed JSON body. A path that does not resolve is
 * undefined, which the caller treats as unreadable rather than as false.
 */
export function dottedPath(value, dotted) {
  let current = value;
  for (const key of String(dotted ?? '').split('.')) {
    if (key.length === 0) continue;
    if (current === null || typeof current !== 'object') return undefined;
    current = current[key];
  }
  return current;
}

/**
 * A busy command reports through its exit code: 0 is idle, anything else is
 * busy. If it also prints a JSON object with "busy" and "reason", those win, so
 * a probe that wants to explain itself can.
 */
export function readCommandBusy(commandResult, startedAt) {
  if (commandResult.timedOut) {
    return unreadable('timed-out', describeFailure(commandResult), startedAt);
  }
  if (commandResult.code === null) {
    return unreadable('probe-error', describeFailure(commandResult), startedAt);
  }

  let parsed = null;
  const text = (commandResult.stdout || '').trim();
  if (text.startsWith('{')) {
    try {
      parsed = JSON.parse(text);
    } catch {
      /* not JSON after all; the exit code still answers the question */
    }
  }
  if (parsed?.unknown === true) {
    const error = typeof parsed.reason === 'string' && parsed.reason.length > 0 ? parsed.reason : 'the command could not determine whether the service is busy';
    return unreadable('command', error, startedAt);
  }
  if (parsed && typeof parsed === 'object' && typeof parsed.busy === 'boolean') {
    const reason =
      typeof parsed.reason === 'string' && parsed.reason.length > 0 ? parsed.reason : parsed.busy ? 'busy' : 'idle';
    return parsed.busy ? busyAnswer('command', reason, startedAt) : idle('command', reason, startedAt);
  }
  if (commandResult.ok) return idle('command', 'idle', startedAt);
  const detail = (commandResult.stderr || commandResult.stdout || '').trim().split('\n')[0];
  return busyAnswer('command', detail || `${commandResult.command} exited ${commandResult.code}`, startedAt);
}

async function checkHttpBusy(probe, timeoutMs, startedAt) {
  const response = await httpGet({
    host: probe.host,
    port: probe.port,
    path: probe.path,
    timeoutMs,
    wantBody: true,
  });
  if (!response.ok || response.body === null) {
    const timedOut = typeof response.error === 'string' && response.error.includes('timed out');
    return unreadable(timedOut ? 'timed-out' : 'probe-error', response.error ?? `HTTP ${response.status}`, startedAt);
  }
  let parsed;
  try {
    parsed = JSON.parse(response.body);
  } catch {
    return unreadable('probe-error', 'the busy endpoint did not answer with JSON', startedAt);
  }
  const flag = dottedPath(parsed, probe.busyWhen);
  if (flag === undefined) {
    return unreadable('probe-error', `the busy endpoint has no "${probe.busyWhen}"`, startedAt);
  }
  return flag ? busyAnswer('http', `${probe.busyWhen} is set`, startedAt) : idle('http', 'idle', startedAt);
}

/**
 * The busy state of one service. Never throws.
 *
 * `liveness` carries what the caller already knows about the service's process:
 * whether it is running and when it started. That is the only evidence that can
 * retire a row a state database still calls running — a turn cannot outlive the
 * server that was executing it. Without that evidence a running row blocks,
 * whatever its age.
 */
export async function checkServiceBusy(service, { timeoutMs = null, liveness = null } = {}) {
  const startedAt = Date.now();
  const probe = service.busy ?? { type: 'unmonitored' };
  const budget = timeoutMs ?? (probe.timeoutSeconds ?? 8) * 1000;

  try {
    switch (probe.type) {
      case 't3-sqlite':
        return checkT3Sqlite(probe, { liveness, serviceName: service.name, startedAt });
      case 'command': {
        if (budget <= 0) {
          return unreadable('timed-out', `there was no time left in the budget to ask ${service.name}`, startedAt);
        }
        return readCommandBusy(await runArgvAsync(probe.command, { timeoutMs: budget }), startedAt);
      }
      case 'http':
        if (budget <= 0) {
          return unreadable('timed-out', `there was no time left in the budget to ask ${service.name}`, startedAt);
        }
        return await checkHttpBusy(probe, budget, startedAt);
      case 'none':
        return declaredIdle(startedAt);
      case 'unmonitored':
      default:
        return unmonitored(service.name, startedAt);
    }
  } catch (err) {
    return unreadable('probe-error', `the busy probe threw: ${err?.message ?? String(err)}`, startedAt);
  }
}

/**
 * Fold the per-service answers into the one object the gate consults: busy when
 * any service is busy, unknown when any probe could not be read. With more than
 * one service the reason names which service said what, because "1 turn running"
 * on its own does not tell anyone what to close.
 */
export function aggregateBusy(entries) {
  if (entries.length === 0) {
    return {
      busy: false,
      unknown: false,
      reason: 'no services are configured',
      monitoredServices: 0,
      unmonitoredServices: 0,
      blocking: [],
      checkedAt: new Date().toISOString(),
    };
  }

  const blocking = entries.filter((entry) => entry.busy.busy === true);
  const isUnknown = entries.some((entry) => entry.busy.unknown === true);
  const reason =
    entries.length === 1
      ? entries[0].busy.reason ?? 'idle'
      : entries.map((entry) => `${entry.service.name}: ${entry.busy.reason ?? 'idle'}`).join('; ');

  return {
    busy: blocking.length > 0,
    unknown: isUnknown,
    reason,
    monitoredServices: entries.filter((entry) => entry.busy.monitored === true).length,
    unmonitoredServices: entries.filter((entry) => entry.busy.monitored === false).length,
    blocking: blocking.map((entry) => entry.service.id),
    checkedAt: new Date().toISOString(),
  };
}

/** The reasonCode a blocked action reports for an aggregate. */
export function busyReasonCode(busy) {
  return busy.unknown ? REASON.busyUnknown : REASON.busy;
}

/**
 * Probe every service and return both the per-service answers and the aggregate.
 *
 * Bounded twice over: no more than BUSY_CONCURRENCY probes at a time, and the
 * whole set shares one deadline. A service whose probe does not fit in what is
 * left of that deadline comes back as timed-out — which blocks, which is the
 * safe direction — rather than pushing the reply past the point where the client
 * that asked has already given up on it.
 */
export async function checkAllBusy(config, { budgetMs = BUSY_BUDGET_MS, probeMs = null, liveness = null } = {}) {
  const services = config.services ?? [];
  const clock = makeDeadline(budgetMs);
  const answers = await mapWithLimit(services, BUSY_CONCURRENCY, (service) => {
    const own = probeMs ?? (service.busy?.timeoutSeconds ?? 8) * 1000;
    return checkServiceBusy(service, {
      timeoutMs: clock.slice(own),
      liveness: liveness?.[service.id] ?? null,
    });
  });
  const entries = services.map((service, index) => ({ service, busy: answers[index] }));
  return {
    entries,
    busy: aggregateBusy(entries),
    budgetMs,
    elapsedMs: budgetMs - clock.remaining(),
    partial: clock.expired(),
  };
}
