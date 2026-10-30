// The one lock every mutating command takes.
//
// Updating, restarting, rebooting, suspending, running a configured action and
// replacing the agent all change the machine underneath whatever else is
// happening on it. The first version guarded only the update path, so a
// controller could restart a service while a scheduled cycle was replacing its
// files, or reboot the machine in the middle of an install. There is now exactly
// one exclusion, machine-wide, and every one of those verbs goes through it.
//
// The exclusion itself lives in mutex.mjs and is held by the operating system,
// which releases it when the holding process ends. Nothing here decides whether
// a lock is "stale": a lock is held exactly while its owner is alive, and that
// judgement belongs to the kernel rather than to a pid written in a file.
//
// Beside the mutex there is a TEXT SIDECAR, `op.lock`, which exists only so a
// person reading it over ssh, and `doctor`, can see what is running. It carries
// no authority at all — if it disappears, the lock is still held; if it is left
// behind, the lock is still free — and it is written only by whoever holds the
// mutex.
//
// Two rules that are easy to get wrong:
//
//   --force overrides a BUSY WARNING. It never breaks another operation's lock.
//   Interrupting a person's work is the user's call to make; interrupting an
//   installer half way through replacing files is not a call anyone gets to
//   make, because the result is a service that no longer starts.
//
//   THE ORDER IS ALWAYS op THEN state. Two locks taken in two orders is a
//   deadlock waiting for a slow disk, so the operation lock is always the outer
//   one and nothing ever takes it while holding the state lock.
//
// status, busy, doctor, op, history and logs do NOT take it. Reading the machine
// must never queue behind an install.

import fs from 'node:fs';
import path from 'node:path';
import { REASON } from './contract.mjs';
import { acquireMutex, mutexPath, probeMutex } from './mutex.mjs';
import { basePath } from './paths.mjs';
import { processIsAlive } from './state.mjs';

/**
 * How long a mutating command waits for the lock before answering `conflict`.
 *
 * Long enough to absorb the hand-off between a cycle finishing one service and
 * starting the next, short enough that a client never sits waiting on a machine
 * that is genuinely busy for the next ten minutes.
 */
export const LOCK_WAIT_MS = 250;
/** A watchdog lock file with no live owner is taken over after this. */
export const LEGACY_LOCK_STALE_MS = 20 * 60 * 1000;

export function operationLockPath() {
  return path.join(basePath(), 'op.lock');
}

/**
 * The sidecar body is `key=value` lines rather than JSON.
 *
 * A human reading it over ssh gets an answer without a parser, and the Windows
 * watchdog that honours the per-service lock file already expects this shape.
 */
function serializeHolder(holder) {
  return `${Object.entries(holder)
    .filter(([, value]) => value !== null && value !== undefined)
    .map(([key, value]) => `${key}=${value}`)
    .join('\n')}\n`;
}

function parseHolder(text) {
  const holder = {};
  for (const line of String(text ?? '').split('\n')) {
    const match = /^([a-zA-Z]+)=(.*)$/.exec(line.trim());
    if (!match) continue;
    holder[match[1]] = match[2];
  }
  if (holder.pid !== undefined) holder.pid = Number.parseInt(holder.pid, 10);
  return holder;
}

function readHolder(file) {
  let text;
  let mtimeMs;
  try {
    text = fs.readFileSync(file, 'utf8');
    mtimeMs = fs.statSync(file).mtimeMs;
  } catch {
    return null;
  }
  return { ...parseHolder(text), ageMs: Date.now() - mtimeMs };
}

/** Replace the sidecar without ever leaving a half-written one behind. */
function writeSidecar(file, holder) {
  const tmp = `${file}.tmp-${process.pid}`;
  try {
    fs.writeFileSync(tmp, serializeHolder(holder), 'utf8');
    fs.renameSync(tmp, file);
  } catch {
    // The sidecar is a convenience. Failing to write it must never fail an
    // operation whose lock is genuinely held.
    try {
      fs.rmSync(tmp, { force: true });
    } catch {
      /* nothing useful to do */
    }
  }
}

function describeHolder(holder) {
  if (!holder) return 'another operation';
  const who = holder.op ? `operation ${holder.op}` : holder.pid ? `pid ${holder.pid}` : 'another process';
  const what = holder.kind && holder.kind !== 'unknown' ? holder.kind : 'an operation';
  const on = holder.service ? ` on ${holder.service}` : '';
  const phase = holder.phase ? `, phase ${holder.phase}` : '';
  return `${what}${on} (${who}${phase})`;
}

/** An ISO 8601 date and time ending in `Z` or an offset from UTC. */
const ZONED_TIME = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(:\d{2}(\.\d+)?)?(Z|[+-]\d{2}:?\d{2})$/i;

/**
 * Whether a WATCHDOG lock file may be taken over.
 *
 * This applies only to the per-service compatibility file, which is written by
 * something outside this agent and identifies its owner by pid. The agent's own
 * locks are not judged this way at all — the operating system holds those.
 *
 * A pid is checked against the time the file says it was taken, where it says
 * one: a process that holds that id now and started after it did not take it,
 * and a file it named would otherwise stay held for as long as the id was in
 * use by somebody else. Only a time that names its zone counts, the shape this
 * agent writes. Nothing pins down what the watchdog writes, and a time with no
 * zone is read as local time: written in UTC on a machine east of it, it would
 * date a live watchdog's file before its own process started, and the agent
 * would take the lock from under it. Any other time leaves the pid check alone,
 * as before there was one.
 */
export function holderIsStale(holder) {
  if (!holder) return true;
  if (Number.isInteger(holder.pid)) return !processIsAlive(holder.pid, { startedAt: ZONED_TIME.test(holder.started ?? '') ? holder.started : null });
  return holder.ageMs > LEGACY_LOCK_STALE_MS;
}

/**
 * The compatibility lock for one service.
 *
 * A Windows install has a watchdog task that honours
 * %LOCALAPPDATA%\T3Code\update.lock, and it only knows how to look for that file
 * with a version on the first line. Holding the machine-wide mutex does nothing
 * for it, so the service lock is written alongside, in exactly the shape that
 * watchdog expects.
 *
 * It is taken strictly INSIDE the operation mutex, so the only thing that can
 * ever contend for it is the watchdog itself — never a second copy of this
 * agent, which is what made its check-then-create protocol unsafe before.
 */
function acquireCompatLock(file, target) {
  if (!file) return { held: false, file: null, message: null, blocked: false };
  try {
    fs.mkdirSync(path.dirname(file), { recursive: true });
  } catch (err) {
    return { held: false, file, message: `cannot create ${path.dirname(file)}: ${err.message}`, blocked: false };
  }

  for (let attempt = 0; attempt < 2; attempt += 1) {
    try {
      const fd = fs.openSync(file, 'wx');
      try {
        // The first line stays the target version: the watchdog that honours
        // this file only knows how to read that shape.
        fs.writeFileSync(fd, `${target}\npid=${process.pid}\nstarted=${new Date().toISOString()}\n`, 'utf8');
      } finally {
        fs.closeSync(fd);
      }
      return { held: true, file, message: null, blocked: false };
    } catch (err) {
      if (err.code !== 'EEXIST') return { held: false, file, message: `cannot write ${file}: ${err.message}`, blocked: false };
      const holder = readHolder(file);
      if (!holderIsStale(holder)) {
        return { held: false, file, message: `${describeHolder(holder)} holds ${file}`, blocked: true, holder };
      }
      try {
        fs.rmSync(file, { force: true });
      } catch (removeError) {
        return { held: false, file, message: `cannot remove the stale lock: ${removeError.message}`, blocked: false };
      }
    }
  }
  return { held: false, file, message: `could not take ${file}`, blocked: false };
}

/** A cycle already owns the machine mutex; each child still needs its watchdog lock. */
export function acquireServiceLock(file, target) {
  const compat = acquireCompatLock(file, target);
  let released = false;
  return {
    ok: !file || compat.held,
    reasonCode: compat.blocked ? REASON.lockHeld : REASON.internal,
    message: compat.message,
    release() {
      if (released || !compat.held) return;
      released = true;
      // Do not remove a foreign watchdog's replacement if the file changed.
      if (readHolder(compat.file)?.pid !== process.pid) return;
      try { fs.rmSync(compat.file, { force: true }); } catch { /* a dead owner can be recovered */ }
    },
  };
}

/** How many holds on the machine lock this process has, which is never more than one at a time. */
let heldHere = 0;

/**
 * Whether this process holds the machine lock. The kernel lets one process
 * hold it at a time, so while this one does, no other can: what recovery
 * knows, from inside the cycle that holds it, about every other process.
 */
export function holdsOperationLock() {
  return heldHere > 0;
}

/**
 * Take the machine-wide operation lock.
 *
 * Returns { ok, lock } or { ok: false, conflict, reasonCode, holder, message }.
 * The caller must release() in a finally, and should call lock.phase(name) as it
 * moves through the cycle so a later reader can say what is running.
 */
export function acquireOperationLock({
  kind,
  opId = null,
  service = null,
  serviceLockFile = null,
  target = null,
  waitMs = LOCK_WAIT_MS,
} = {}) {
  const file = operationLockPath();
  const taken = acquireMutex(mutexPath(file), { waitMs });

  if (!taken.ok) {
    if (taken.busy) {
      // The sidecar tells us WHAT is running. It has no authority over whether
      // anything is — the mutex already answered that — so an absent or stale
      // sidecar just means a less specific message.
      const holder = readHolder(file);
      return {
        ok: false,
        conflict: true,
        reasonCode: REASON.operationInProgress,
        holder,
        conflictDetail: {
          opId: holder?.op ?? null,
          kind: holder?.kind ?? null,
          service: holder?.service ?? null,
          phase: holder?.phase ?? null,
          startedAt: holder?.started ?? null,
        },
        message: `${describeHolder(holder)} is already running; --force does not break another operation's lock`,
      };
    }
    return {
      ok: false,
      conflict: false,
      reasonCode: REASON.internal,
      message: taken.hint ? `${taken.error}: ${taken.hint}` : taken.error,
    };
  }

  const holder = {
    pid: process.pid,
    op: opId,
    kind,
    service,
    target,
    phase: 'starting',
    started: new Date().toISOString(),
  };
  writeSidecar(file, holder);

  const compat = acquireCompatLock(serviceLockFile, target ?? kind);
  if (compat.blocked || (serviceLockFile && !compat.held)) {
    // The watchdog owns the service file. Give the machine lock straight back
    // rather than sitting on it, and take the sidecar with it.
    try {
      fs.rmSync(file, { force: true });
    } catch {
      /* the next holder overwrites it anyway */
    }
    taken.release();
    return {
      ok: false,
      conflict: true,
      reasonCode: compat.blocked ? REASON.lockHeld : REASON.internal,
      holder: compat.holder ?? null,
      message: compat.message,
    };
  }

  let released = false;
  heldHere += 1;
  return {
    ok: true,
    conflict: false,
    message: null,
    lock: {
      file,
      /** The operation the sidecar names as holding the lock, which a record notes as its evidence of life. */
      opId,
      compatFile: compat.held ? compat.file : null,
      compatWarning: compat.held || !compat.message ? null : compat.message,
      /** Record the phase in the sidecar so a reader can name what is running. */
      phase(name) {
        holder.phase = name;
        writeSidecar(file, holder);
      },
      release() {
        if (released) return;
        released = true;
        // Innermost first: the watchdog file, then the sidecar, then the mutex
        // last, so nothing can take the lock and find another holder's sidecar.
        if (compat.held) {
          try {
            fs.rmSync(compat.file, { force: true });
          } catch {
            /* a watchdog file with a dead pid is reclaimed by the next caller */
          }
        }
        try {
          fs.rmSync(file, { force: true });
        } catch {
          /* the sidecar carries no authority; a leftover is harmless */
        }
        heldHere -= 1;
        taken.release();
      },
    },
  };
}

/**
 * What the mutex says about the machine lock, `held`, `free` or `unknown`,
 * with the sidecar as it stands, or null where there is none.
 *
 * Read-only in the strictest sense: it never creates the lock database, so a
 * status poll on a machine that has never run an operation leaves no trace.
 */
export function probeOperationLock() {
  const file = operationLockPath();
  const probe = probeMutex(mutexPath(file));
  return { state: probe.state, sidecar: readHolder(file), error: probe.error ?? null, hint: probe.hint ?? null };
}

/**
 * Who holds the machine lock right now, or null.
 *
 * `held` comes from the mutex, which is the truth; the sidecar only supplies the
 * detail. A sidecar with no lock behind it is a leftover, and says so.
 */
export function currentHolder() {
  const { state, sidecar, error, hint } = probeOperationLock();
  if (!sidecar && state !== 'held') return null;
  return {
    ...(sidecar ?? {}),
    held: state === 'held',
    stale: Boolean(sidecar) && state === 'free',
    lockError: error,
    lockHint: hint,
  };
}

/**
 * Drop leftovers nobody owns.
 *
 * Only two things can be cleared: a watchdog lock file whose pid is gone, and
 * the sidecar of an operation lock the mutex reports as free. THE LOCK DATABASES
 * ARE NEVER TOUCHED — unlinking one while it is held creates a second lock
 * domain, which is the precise failure this whole design exists to remove.
 */
export function clearStaleLocks(files = []) {
  const cleared = [];
  const sidecar = operationLockPath();

  if (probeMutex(mutexPath(sidecar)).state === 'free') {
    const holder = readHolder(sidecar);
    if (holder) {
      try {
        fs.rmSync(sidecar, { force: true });
        cleared.push({ file: sidecar, holder });
      } catch {
        /* the next holder overwrites it */
      }
    }
  }

  for (const file of files.filter(Boolean)) {
    if (file.endsWith('.sqlite')) continue;
    let holder;
    try {
      if (!fs.existsSync(file)) continue;
      holder = readHolder(file);
    } catch {
      continue;
    }
    if (!holderIsStale(holder)) continue;
    try {
      fs.rmSync(file, { force: true });
      cleared.push({ file, holder });
    } catch {
      /* it will be taken over by the next acquirer instead */
    }
  }
  return cleared;
}
