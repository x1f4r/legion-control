// When the agent is allowed to update something on its own.
//
// The first version had one switch, autoUpdate, and it answered two completely
// different questions with it. "Should the timer install things unattended?" and
// "may this person press Update?" are not the same question, and wiring them
// together meant that turning the schedule off also removed the safe manual
// path — leaving --force, which skips the busy gate, as the only way to update
// at all. That is exactly backwards: the setting that exists to make the machine
// more careful made the only available action less careful.
//
// So there are three separate things here:
//
//   ELIGIBILITY   may the SCHEDULER act unattended right now? System automatic,
//                 per-service automatic, pause-until, maintenance windows.
//   REQUEST       a person asked. Eligibility is not consulted at all.
//   OVERRIDE      may this run interrupt work in progress? That is --force, and
//                 overriding the busy gate is the ONLY thing --force means. It
//                 never breaks the operation lock, never accepts an invalid
//                 config, and never turns an unverifiable update into a success.
//
// Everything the scheduler declines is declined with a reasonCode, so a client
// can say "paused until Tuesday" rather than "nothing happened".

import { WEEKDAYS } from './config.mjs';
import { REASON } from './contract.mjs';
import { serviceState } from './state.mjs';

function minutesOfDay(hhmm) {
  const [hours, minutes] = String(hhmm).split(':').map((part) => Number.parseInt(part, 10));
  return hours * 60 + minutes;
}

/**
 * Is `now` inside one window? Local time, deliberately: a maintenance window is
 * a statement about when the person who owns the machine is asleep, and that is
 * a wall-clock fact about where the machine is, not a UTC offset.
 *
 * A window whose end is before its start wraps past midnight, which is what
 * almost every real maintenance window does. It belongs to the day it STARTED
 * on, so 22:00-04:00 on "fri" covers Friday evening and the early hours of
 * Saturday, and not Friday's own small hours.
 */
export function inWindow(window, now = new Date()) {
  if (!window) return true;
  const day = WEEKDAYS[now.getDay()];
  const from = minutesOfDay(window.from);
  const to = minutesOfDay(window.to);
  const minute = now.getHours() * 60 + now.getMinutes();

  if (from <= to) return window.days.includes(day) && minute >= from && minute <= to;
  if (minute >= from) return window.days.includes(day);
  const yesterday = WEEKDAYS[(now.getDay() + 6) % 7];
  return window.days.includes(yesterday) && minute <= to;
}

/** An empty list means "any time". Otherwise any one window is enough. */
export function inAnyWindow(windows, now = new Date()) {
  if (!Array.isArray(windows) || windows.length === 0) return true;
  return windows.some((window) => inWindow(window, now));
}

/**
 * When the next window opens, as an ISO timestamp, or null when there is always
 * one open. Searched minute by minute over a week, which is cheap and avoids a
 * calendar library for a question asked once per status.
 */
export function nextWindowStart(windows, now = new Date()) {
  if (!Array.isArray(windows) || windows.length === 0) return null;
  if (inAnyWindow(windows, now)) return null;
  const cursor = new Date(now.getTime());
  cursor.setSeconds(0, 0);
  for (let step = 1; step <= 7 * 24 * 60; step += 1) {
    cursor.setMinutes(cursor.getMinutes() + 1);
    if (inAnyWindow(windows, cursor)) return cursor.toISOString();
  }
  return null;
}

/**
 * The policy actually in force for one service, with every inherited value
 * resolved and the runtime pause folded in.
 *
 * A pause can come from the config file or from `auto-update pause`, which
 * writes to the service's state rather than rewriting the config. Whichever
 * lasts longer wins: a person who paused for four hours and then edited the
 * config to pause for a day meant the day.
 */
export function effectivePolicy(config, service, { state = null } = {}) {
  const system = config.updates ?? { automatic: true, pauseUntil: null, maintenanceWindows: [] };
  const own = service?.updates ?? {};
  const stored = state ?? (service ? serviceState(service) : {});

  const automatic = own.automatic === null || own.automatic === undefined ? system.automatic === true : own.automatic === true;
  const inherited = own.automatic === null || own.automatic === undefined;

  const pauses = [own.pauseUntil, stored.pauseUntil, system.pauseUntil].filter(
    (value) => typeof value === 'string' && !Number.isNaN(Date.parse(value)),
  );
  const pauseUntil = pauses.length > 0 ? pauses.sort((a, b) => Date.parse(b) - Date.parse(a))[0] : null;

  const maintenanceWindows =
    own.maintenanceWindows === null || own.maintenanceWindows === undefined
      ? system.maintenanceWindows ?? []
      : own.maintenanceWindows;

  return {
    automatic,
    inherited,
    pauseUntil,
    // Which side the pause came from matters to a UI offering "resume".
    pauseSource: pauses.length === 0 ? null : pauseUntil === stored.pauseUntil ? 'runtime' : pauseUntil === own.pauseUntil ? 'service' : 'system',
    maintenanceWindows,
    windowsInherited: own.maintenanceWindows === null || own.maintenanceWindows === undefined,
    order: own.order ?? 0,
    after: own.after ?? [],
  };
}

/** Whether the service can be updated at all, before any policy question. */
export function hasUpdatePath(service) {
  if (service.kind === 'command') return Boolean(service.update);
  return true;
}

/**
 * May the SCHEDULER update this service unattended right now?
 *
 * Returns { eligible, reasonCode, reason }. Never consulted for a manual
 * request. The busy gate is not asked here: it is asked later, by the cycle,
 * because it costs a probe and this answer often makes it unnecessary.
 */
export function scheduledEligibility(config, service, { now = new Date(), state = null } = {}) {
  if (!hasUpdatePath(service)) {
    return {
      eligible: false,
      reasonCode: REASON.noUpdate,
      reason: `${service.name} has no update command configured, so there is nothing for the scheduler to run`,
    };
  }

  const policy = effectivePolicy(config, service, { state });

  // The system switch is a master stop for the scheduler. A service that turns
  // its own automatic on cannot re-enable a schedule the machine has turned off,
  // because "off" on the machine has to mean off.
  if (config.updates?.automatic !== true) {
    return {
      eligible: false,
      reasonCode: REASON.policyOff,
      reason: 'automatic updates are turned off on this machine; services can still be updated on request',
      policy,
    };
  }
  if (!policy.automatic) {
    return {
      eligible: false,
      reasonCode: REASON.policyOff,
      reason: `automatic updates are turned off for ${service.name}; it can still be updated on request`,
      policy,
    };
  }

  if (policy.pauseUntil && Date.parse(policy.pauseUntil) > now.getTime()) {
    return {
      eligible: false,
      reasonCode: REASON.policyPaused,
      reason: `${service.name} is paused until ${policy.pauseUntil}`,
      until: policy.pauseUntil,
      policy,
    };
  }

  if (!inAnyWindow(policy.maintenanceWindows, now)) {
    const described = policy.maintenanceWindows
      .map((window) => `${window.days.join('/')} ${window.from}-${window.to}`)
      .join(', ');
    return {
      eligible: false,
      reasonCode: REASON.outsideWindow,
      reason: `${service.name} only updates inside its maintenance window (${described}, local time)`,
      nextWindow: nextWindowStart(policy.maintenanceWindows, now),
      policy,
    };
  }

  return { eligible: true, reasonCode: null, reason: 'eligible', policy };
}

/**
 * The order a scheduled cycle walks the services in.
 *
 * `after` is a real dependency, not a hint: a service that fronts another one
 * should come down after it and come back up before it is asked for. A cycle in
 * the graph is reported rather than resolved, because there is no correct order
 * and picking one silently would be worse than saying so.
 *
 * Both `order` and `after` are optional additions outside the frozen contract.
 * They only decide the sequence of services a cycle was going to walk anyway, so
 * a client that knows nothing about them loses nothing.
 */
export function orderServices(services) {
  const byId = new Map(services.map((service) => [service.id, service]));
  const visited = new Set();
  const visiting = new Set();
  const ordered = [];
  const cycles = [];

  const visit = (service, trail) => {
    if (visited.has(service.id)) return;
    if (visiting.has(service.id)) {
      cycles.push([...trail, service.id]);
      return;
    }
    visiting.add(service.id);
    const dependencies = (service.updates?.after ?? [])
      .map((id) => byId.get(id))
      .filter(Boolean)
      .sort((a, b) => (a.updates?.order ?? 0) - (b.updates?.order ?? 0));
    for (const dependency of dependencies) visit(dependency, [...trail, service.id]);
    visiting.delete(service.id);
    visited.add(service.id);
    ordered.push(service);
  };

  for (const service of [...services].sort((a, b) => (a.updates?.order ?? 0) - (b.updates?.order ?? 0))) {
    visit(service, []);
  }
  return { ordered, cycles };
}

/** The system-level `updates` block as status reports it. */
export function describeSystemUpdates(config, { now = new Date(), lastCycle = null } = {}) {
  const updates = config.updates ?? { automatic: true, pauseUntil: null, maintenanceWindows: [] };
  return {
    automatic: updates.automatic === true,
    pauseUntil: updates.pauseUntil ?? null,
    maintenanceWindows: updates.maintenanceWindows ?? [],
    inWindowNow: inAnyWindow(updates.maintenanceWindows, now),
    nextWindow: nextWindowStart(updates.maintenanceWindows, now),
    lastCycle,
  };
}

/** The per-service `updates` block as status reports it. */
export function describeServiceUpdates(config, service, { now = new Date(), state = null, busy = null } = {}) {
  const eligibility = scheduledEligibility(config, service, { now, state });
  const policy = eligibility.policy ?? effectivePolicy(config, service, { state });

  // The busy gate is the last thing a cycle checks, so a service that is
  // otherwise eligible is only "eligible now" when it is also not busy.
  let eligibleNow = eligibility.eligible;
  let deferredReason = eligibility.reasonCode;
  if (eligibleNow && busy && busy.busy === true) {
    eligibleNow = false;
    deferredReason = busy.unknown ? REASON.busyUnknown : REASON.busy;
  }

  return {
    automatic: policy.automatic,
    inherited: policy.inherited,
    pauseUntil: policy.pauseUntil,
    pauseSource: policy.pauseSource,
    maintenanceWindows: policy.maintenanceWindows,
    windowsInherited: policy.windowsInherited,
    order: policy.order,
    after: policy.after,
    eligibleNow,
    deferredReason: eligibleNow ? null : deferredReason,
    nextWindow: eligibility.nextWindow ?? null,
    // Manual updates are always offered where there is an update path at all.
    // "Automatic updates are off" has never been a reason to refuse a person.
    manualAvailable: hasUpdatePath(service),
  };
}

/**
 * Validate a policy patch from `policy set` before anything is written.
 *
 * Returns { ok, patch, errors }. The patch is the subset of keys the caller
 * actually named, so setting a pause does not silently rewrite the windows.
 */
export function validatePolicyPatch(raw, { perService }) {
  const errors = [];
  const patch = {};
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) {
    return { ok: false, patch: null, errors: [{ path: '', message: 'expected a JSON object' }] };
  }

  const known = perService ? ['automatic', 'pauseUntil', 'maintenanceWindows'] : ['automatic', 'pauseUntil', 'maintenanceWindows'];
  for (const key of Object.keys(raw)) {
    if (!known.includes(key)) errors.push({ path: key, message: `unknown setting "${key}"; this patch accepts: ${known.join(', ')}` });
  }

  if ('automatic' in raw) {
    if (raw.automatic === null) {
      if (!perService) errors.push({ path: 'automatic', message: 'the system-wide automatic switch cannot inherit from anything, so it cannot be null' });
      else patch.automatic = null;
    } else if (typeof raw.automatic !== 'boolean') {
      errors.push({ path: 'automatic', message: `expected true, false${perService ? ' or null to inherit' : ''}, got ${JSON.stringify(raw.automatic)}` });
    } else {
      patch.automatic = raw.automatic;
    }
  }

  if ('pauseUntil' in raw) {
    if (raw.pauseUntil === null) patch.pauseUntil = null;
    else if (typeof raw.pauseUntil !== 'string' || Number.isNaN(Date.parse(raw.pauseUntil))) {
      errors.push({ path: 'pauseUntil', message: `expected an ISO timestamp or null, got ${JSON.stringify(raw.pauseUntil)}` });
    } else {
      patch.pauseUntil = new Date(raw.pauseUntil).toISOString();
    }
  }

  if ('maintenanceWindows' in raw) {
    if (raw.maintenanceWindows === null) {
      if (!perService) errors.push({ path: 'maintenanceWindows', message: 'the system-wide window list cannot inherit; use [] for "any time"' });
      else patch.maintenanceWindows = null;
    } else if (!Array.isArray(raw.maintenanceWindows)) {
      errors.push({ path: 'maintenanceWindows', message: `expected an array of windows, got ${JSON.stringify(raw.maintenanceWindows)}` });
    } else {
      const windows = [];
      raw.maintenanceWindows.forEach((entry, index) => {
        const where = `maintenanceWindows[${index}]`;
        if (!entry || typeof entry !== 'object' || Array.isArray(entry)) {
          errors.push({ path: where, message: 'expected an object with days, from and to' });
          return;
        }
        for (const key of Object.keys(entry)) {
          if (!['days', 'from', 'to'].includes(key)) errors.push({ path: `${where}.${key}`, message: `unknown key "${key}"` });
        }
        const days = Array.isArray(entry.days) ? entry.days.map((day) => String(day).slice(0, 3).toLowerCase()) : [...WEEKDAYS];
        const unknown = days.find((day) => !WEEKDAYS.includes(day));
        if (unknown) errors.push({ path: `${where}.days`, message: `unknown day ${JSON.stringify(unknown)}; one of ${WEEKDAYS.join(', ')}` });
        for (const key of ['from', 'to']) {
          if (entry[key] === undefined || entry[key] === null) continue;
          if (typeof entry[key] !== 'string' || !/^([01]\d|2[0-3]):[0-5]\d$/.test(entry[key])) {
            errors.push({ path: `${where}.${key}`, message: `expected a 24-hour local time like "02:30", got ${JSON.stringify(entry[key])}` });
          }
        }
        windows.push({
          days: days.length > 0 ? days : [...WEEKDAYS],
          from: typeof entry.from === 'string' ? entry.from : '00:00',
          to: typeof entry.to === 'string' ? entry.to : '23:59',
        });
      });
      patch.maintenanceWindows = windows;
    }
  }

  return { ok: errors.length === 0, patch: errors.length === 0 ? patch : null, errors };
}
