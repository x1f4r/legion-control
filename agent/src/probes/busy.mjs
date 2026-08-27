// The busy gate.
//
// "Busy" means something would be lost by stopping the service, and this gate
// sits in front of every disruptive action: update, restart, boot, sleep. IT
// FAILS CLOSED EVERYWHERE. A probe that cannot be read counts as busy, because
// deferring costs a delay and the next cycle tries again, while acting on a
// misread costs the user their work.

import { describeFailure, runArgv } from '../config.mjs';
import { httpGet } from '../http.mjs';
import { checkT3Sqlite } from './t3-sqlite.mjs';

function idle(reason = 'idle') {
  return { busy: false, reason, unknown: false };
}

function unknown(detail) {
  return { busy: true, reason: 'busy state unknown', unknown: true, error: detail };
}

/**
 * Follow a dotted path into a parsed JSON body. A path that does not resolve is
 * undefined, which is falsy, which reads as idle — so the caller has to have
 * decided already that the body itself was readable.
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
export function readCommandBusy(result) {
  if (result.timedOut || result.code === null) return unknown(describeFailure(result));

  let parsed = null;
  const text = (result.stdout || '').trim();
  if (text.startsWith('{')) {
    try {
      parsed = JSON.parse(text);
    } catch {
      /* not JSON after all; the exit code still answers the question */
    }
  }
  if (parsed && typeof parsed === 'object' && typeof parsed.busy === 'boolean') {
    return {
      busy: parsed.busy,
      reason: typeof parsed.reason === 'string' && parsed.reason.length > 0 ? parsed.reason : parsed.busy ? 'busy' : 'idle',
      unknown: false,
    };
  }
  if (result.ok) return idle();
  const detail = (result.stderr || result.stdout || '').trim().split('\n')[0];
  return { busy: true, reason: detail || `${result.command} exited ${result.code}`, unknown: false };
}

async function checkHttpBusy(probe) {
  const response = await httpGet({
    host: probe.host,
    port: probe.port,
    path: probe.path,
    timeoutMs: 5000,
    wantBody: true,
  });
  if (!response.ok || response.body === null) {
    return unknown(response.error ?? `HTTP ${response.status}`);
  }
  let parsed;
  try {
    parsed = JSON.parse(response.body);
  } catch {
    return unknown('the busy endpoint did not answer with JSON');
  }
  const flag = dottedPath(parsed, probe.busyWhen);
  if (flag === undefined) return unknown(`the busy endpoint has no "${probe.busyWhen}"`);
  return flag ? { busy: true, reason: `${probe.busyWhen} is set`, unknown: false } : idle();
}

/** The busy state of one service. Never throws. */
export async function checkServiceBusy(service) {
  const probe = service.busy ?? { type: 'none' };
  try {
    switch (probe.type) {
      case 't3-sqlite':
        return checkT3Sqlite(probe);
      case 'command':
        return readCommandBusy(runArgv(probe.command, { timeoutMs: 20000 }));
      case 'http':
        return await checkHttpBusy(probe);
      default:
        return idle();
    }
  } catch (err) {
    return unknown(`the busy probe threw: ${err?.message ?? String(err)}`);
  }
}

/**
 * Fold the per-service answers into the one object the gate consults: busy when
 * any service is busy, unknown when any probe could not be read. With more than
 * one service the reason names which service said what, because "1 turn running"
 * on its own does not tell anyone what to close.
 */
export function aggregateBusy(entries) {
  if (entries.length === 0) return idle();
  if (entries.length === 1) {
    const only = entries[0].busy;
    return { busy: only.busy === true, reason: only.reason ?? 'idle', unknown: only.unknown === true };
  }

  const busy = entries.some((entry) => entry.busy.busy === true);
  const isUnknown = entries.some((entry) => entry.busy.unknown === true);
  const reason = entries.map((entry) => `${entry.service.name}: ${entry.busy.reason ?? 'idle'}`).join('; ');
  return { busy, reason, unknown: isUnknown };
}

/** Probe every service and return both the per-service answers and the aggregate. */
export async function checkAllBusy(config) {
  const entries = [];
  for (const service of config.services) {
    entries.push({ service, busy: await checkServiceBusy(service) });
  }
  return { entries, busy: aggregateBusy(entries) };
}
