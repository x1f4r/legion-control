// Durable operations: what was asked for, what phase it reached, and what
// happened, kept on disk rather than in the process that was asked.
//
// The first version tied a mutation to the SSH session that started it. The
// result came back at the end or not at all, so a dropped link, a phone that
// went to sleep or a scheduler killing the run at its time limit all produced
// the same thing: an outcome nobody could ever find out. Worse, a client that
// retried after an ambiguous handoff had no way to know it was asking for
// something that had already started.
//
// So every mutation gets an id before it does anything — supplied by the client
// with --op, so the client can ask again about the same thing — records its
// phases as it goes, and writes a terminal result. A retry with the same id
// finds the original instead of starting a second one, and a run that was killed
// mid-flight is found by the next recovery pass and marked for what it is.
//
// AN ID BINDS AN INTENT. Reusing an id for a different kind, a different
// service, a different target or a different force setting is not a retry, it is
// a collision, and it answers `conflict`. Anything else would let a client that
// generates ids badly silently turn "restart t3" into "reboot the machine".

import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { AGENT_VERSION, operationsDir, runArgv } from './config.mjs';
import { CONTRACT_VERSION, DISRUPTED_PHASES, OPERATION_ID_PATTERN, REASON } from './contract.mjs';
import { acquireMutex, mutexPath } from './mutex.mjs';
import { processIsAlive, writeJsonAtomic } from './state.mjs';

/** How many finished records are kept, and for how long. */
export const HISTORY_LIMIT = 200;
export const HISTORY_MAX_AGE_MS = 30 * 24 * 60 * 60 * 1000;
/** A queued request with no expiry of its own. */
export const DEFAULT_QUEUE_TTL_MS = 4 * 60 * 60 * 1000;
export const MAX_QUEUE_TTL_MS = 7 * 24 * 60 * 60 * 1000;
/** Per-operation log lines kept in the record. */
export const LOG_LIMIT = 200;

/**
 * Record metadata has its own lock domain. It is deliberately separate from
 * the machine operation lock: phases and logs only hold this lock for one
 * synchronous read-modify-write, while the machine lock may span minutes.
 * When both are used, callers take the machine lock first and this lock inside
 * it; this module never tries to acquire the machine lock.
 */
export function operationsStoreMutexPath() {
  return mutexPath(path.join(operationsDir(), '.records.lock'));
}

export class OperationStoreError extends Error {
  constructor(message) {
    super(message);
    this.name = 'OperationStoreError';
    this.code = 'OPERATION_STORE_FAILED';
  }
}

function acquireOperationsStore() {
  const held = acquireMutex(operationsStoreMutexPath(), { waitMs: 20000 });
  if (held.ok) return held;
  const detail = held.busy ? 'did not become free within 20 seconds' : held.error;
  throw new OperationStoreError(`the operations store ${detail}`);
}

let heldOperationsStore = null;

function withOperationsStore(body) {
  // Metadata transactions are synchronous. Reusing the connection here makes
  // helper composition safe without weakening cross-process exclusion; no
  // other JavaScript can run between the nested calls.
  if (heldOperationsStore) {
    return body();
  }

  const held = acquireOperationsStore();
  heldOperationsStore = held;
  try {
    return body();
  } finally {
    heldOperationsStore = null;
    held.release();
  }
}

export function operationPath(id) {
  return path.join(operationsDir(), `${id}.json`);
}

/** A client may supply its own; otherwise a UUIDv4, which matches the same grammar. */
export function newOperationId() {
  return crypto.randomUUID();
}

function nowIso() {
  return new Date().toISOString();
}

function isPlainObject(value) {
  return Boolean(value) && typeof value === 'object' && !Array.isArray(value);
}

/**
 * Who asked. The restricted dispatcher sets LEGIONCTL_CLIENT from the ssh key
 * comment, which is the only identity the agent can actually attest to; anything
 * a client puts in an argument is a claim, not evidence.
 */
export function currentInitiator() {
  return {
    client: process.env.LEGIONCTL_CLIENT || 'cli',
    device: process.env.LEGIONCTL_DEVICE || null,
    user: process.env.LEGIONCTL_USER || os.userInfo?.().username || null,
  };
}

/**
 * This machine's boot, identified as precisely as the platform allows.
 *
 * Used as the evidence that a reboot actually happened. On Linux the kernel
 * hands out a fresh random boot id every boot. Other supported platforms expose
 * their actual boot time. An unavailable identity stays unknown; a rounded
 * uptime estimate could cross a rounding boundary without a reboot.
 */
let cachedBootIdentity;

export function bootIdentity() {
  if (cachedBootIdentity !== undefined) return cachedBootIdentity;
  try {
    if (process.platform === 'linux') {
      const id = fs.readFileSync('/proc/sys/kernel/random/boot_id', 'utf8').trim();
      if (id) return (cachedBootIdentity = id);
    }
    if (process.platform === 'darwin') {
      const result = runArgv(['/usr/sbin/sysctl', '-n', 'kern.boottime'], { timeoutMs: 5000 });
      const match = /sec\s*=\s*(\d+)\s*,\s*usec\s*=\s*(\d+)/.exec(result.stdout ?? '');
      if (result.ok && match) return (cachedBootIdentity = `boot-${match[1]}-${match[2]}`);
    }
    if (process.platform === 'win32') {
      const result = runArgv(['powershell.exe', '-NoProfile', '-NonInteractive', '-Command', '(Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime().Ticks'], { timeoutMs: 10000 });
      const ticks = String(result.stdout ?? '').trim();
      if (result.ok && /^\d+$/.test(ticks)) return (cachedBootIdentity = `boot-${ticks}`);
    }
  } catch {
    /* an unavailable identity cannot prove a transition */
  }
  return (cachedBootIdentity = null);
}

// ---------------------------------------------------------------------------
// Reading and writing one record
// ---------------------------------------------------------------------------

export function readOperation(id) {
  try {
    const parsed = JSON.parse(fs.readFileSync(operationPath(id), 'utf8'));
    return isPlainObject(parsed) ? parsed : null;
  } catch {
    return null;
  }
}

/**
 * Write one record atomically.
 * Readers see one whole version or the other because the write lands through a
 * rename. Writers also take the operations-store mutex because detached-worker
 * handoff, recovery and status-independent queue work can touch the same record
 * from different processes.
 */
function writeOperationUnlocked(record) {
  return writeJsonAtomic(operationPath(record.id), { ...record, updatedAt: nowIso() });
}

export function writeOperation(record) {
  try {
    return withOperationsStore(() => writeOperationUnlocked(record));
  } catch (err) {
    return { ok: false, error: err?.message ?? String(err) };
  }
}

/** Every record on disk, newest first. Never throws. */
export function listOperations({ limit = 30, service = null, kind = null, state = null, recoveryPending = false } = {}) {
  let names = [];
  try {
    names = fs.readdirSync(operationsDir());
  } catch {
    return [];
  }
  const records = [];
  for (const name of names) {
    if (!name.endsWith('.json')) continue;
    const record = readOperation(name.slice(0, -5));
    if (!record) continue;
    if (service && record.service !== service) continue;
    if (kind && record.kind !== kind) continue;
    if (state && record.state !== state) continue;
    if (recoveryPending && record.recoveryPending !== true) continue;
    records.push(record);
  }
  records.sort((a, b) => String(b.requestedAt ?? '').localeCompare(String(a.requestedAt ?? '')));
  return records.slice(0, Math.min(limit, HISTORY_LIMIT));
}

/**
 * The record as clients see it.
 *
 * The file on disk carries a few things the wire contract does not name — the
 * boot identity a reboot is judged against, whether the request asked to be
 * queued — and those stay internal. Projecting explicitly rather than deleting
 * keys means a field added to the file later cannot leak onto the wire by
 * accident.
 */
export function wireRecord(record) {
  if (!record) return null;
  const wire = {
    id: record.id,
    kind: record.kind,
    service: record.service ?? null,
    target: record.target ?? null,
    actionId: record.actionId ?? null,
    mode: record.mode ?? 'manual',
    initiator: {
      client: record.initiator?.client ?? 'cli',
      device: record.initiator?.device ?? null,
      user: record.initiator?.user ?? null,
    },
    state: record.state,
    phase: record.state === 'finished' ? 'done' : record.phase ?? null,
    progress: record.progress ?? null,
    requestedAt: record.requestedAt ?? null,
    startedAt: record.startedAt ?? null,
    updatedAt: record.updatedAt ?? null,
    finishedAt: record.finishedAt ?? null,
    expiresAt: record.expiresAt ?? null,
    pid: record.pid ?? null,
    detached: record.detached === true,
    result: record.result ?? null,
    log: Array.isArray(record.log) ? record.log : [],
    agentVersion: record.agentVersion ?? AGENT_VERSION,
    systemId: record.systemId ?? null,
  };
  if (record.from !== undefined && record.from !== null) wire.from = record.from;
  if (record.to !== undefined && record.to !== null) wire.to = record.to;
  if (Array.isArray(record.children)) wire.children = record.children;
  if (record.replaced) wire.replaced = record.replaced;
  return wire;
}

/** The compact form status and history carry. */
export function summarize(record) {
  if (!record) return null;
  return {
    id: record.id,
    kind: record.kind,
    service: record.service ?? null,
    target: record.target ?? null,
    actionId: record.actionId ?? null,
    mode: record.mode ?? null,
    state: record.state,
    phase: record.phase ?? null,
    action: record.result?.action ?? null,
    reasonCode: record.result?.reasonCode ?? null,
    requestedAt: record.requestedAt ?? null,
    updatedAt: record.updatedAt ?? null,
    expiresAt: record.expiresAt ?? null,
  };
}

// ---------------------------------------------------------------------------
// Intent
// ---------------------------------------------------------------------------

/**
 * The part of a request an id is bound to.
 *
 * Force is in here because "restart, interrupting whatever is running" is a
 * materially different request from "restart when it is safe", and a client that
 * reuses an id across that boundary must be told so rather than quietly getting
 * whichever one happened to be recorded first.
 */
export function intentOf(record) {
  return {
    kind: record.kind ?? null,
    service: record.service ?? null,
    target: record.target ?? null,
    actionId: record.actionId ?? null,
    force: record.force === true,
    queued: record.queueRequested === true,
    noReboot: record.noReboot === true,
    services: record.services ? [...new Set(record.services)].sort() : null,
  };
}

function intentMatches(a, b) {
  return (
    a.kind === b.kind &&
    a.service === b.service &&
    a.target === b.target &&
    a.actionId === b.actionId &&
    a.force === b.force &&
    a.queued === b.queued &&
    a.noReboot === b.noReboot &&
    JSON.stringify(a.services) === JSON.stringify(b.services)
  );
}

function describeIntent(intent) {
  const parts = [intent.kind];
  if (intent.service) parts.push(`service ${intent.service}`);
  if (intent.target) parts.push(`target ${intent.target}`);
  if (intent.actionId) parts.push(`action ${intent.actionId}`);
  if (intent.force) parts.push('forced');
  if (intent.queued) parts.push('when idle');
  if (intent.noReboot) parts.push('arm only');
  if (intent.services) parts.push(`services ${intent.services.join(', ')}`);
  return parts.join(', ');
}

// ---------------------------------------------------------------------------
// Opening a record
// ---------------------------------------------------------------------------

function baseRecord(input) {
  const at = nowIso();
  return {
    id: input.id,
    kind: input.kind,
    service: input.service ?? null,
    target: input.target ?? null,
    actionId: input.actionId ?? null,
    mode: input.mode ?? 'manual',
    force: input.force === true,
    queueRequested: input.queueRequested === true,
    noReboot: input.noReboot === true,
    services: input.services ? [...new Set(input.services)].sort() : null,
    initiator: input.initiator ?? currentInitiator(),
    state: input.state ?? 'running',
    phase: input.phase ?? null,
    progress: null,
    requestedAt: at,
    startedAt: input.state === 'queued' ? null : at,
    updatedAt: at,
    finishedAt: null,
    expiresAt: input.expiresAt ?? null,
    pid: input.state === 'queued' ? null : process.pid,
    detached: input.detached === true,
    workerClaimed: input.state !== 'queued' && input.detached !== true,
    bootId: bootIdentity(),
    systemId: input.systemId ?? null,
    from: null,
    to: null,
    result: null,
    log: [],
    agentVersion: AGENT_VERSION,
    contract: CONTRACT_VERSION,
  };
}

/**
 * Open a record, or hand back the one this id already names.
 *
 * Returns one of:
 *   { ok: true, record, replay: false }   a new record; do the work
 *   { ok: true, record, replay: true }    this id already ran; report on it
 *   { ok: false, conflict: true, ... }    this id names a different request
 *   { ok: false, error }                  the record could not be written
 *
 * Refusing on a write failure is deliberate: running a mutation whose record
 * cannot be persisted means nobody can ever find out what it did, which is the
 * exact failure the operation log exists to prevent.
 */
function beginOperationUnlocked(input) {
  const id = input.id ?? newOperationId();
  if (!OPERATION_ID_PATTERN.test(id)) {
    return { ok: false, error: `operation id ${JSON.stringify(id)} is not 8 to 64 lowercase characters, digits or dashes` };
  }

  const existing = readOperation(id);
  if (existing) {
    const wanted = intentOf({ ...input, queueRequested: input.queueRequested === true });
    const held = intentOf(existing);
    if (!intentMatches(wanted, held)) {
      return {
        ok: false,
        conflict: true,
        record: existing,
        reasonCode: REASON.badArgument,
        error:
          `operation ${id} already names a different request (${describeIntent(held)}); ` +
          `this one is ${describeIntent(wanted)}. An operation id binds one intent, so reusing it for another is refused rather than replayed`,
      };
    }
    return { ok: true, record: existing, replay: true };
  }
  if (fs.existsSync(operationPath(id))) {
    return {
      ok: false,
      error: `operation ${id} already has a record that cannot be read as an object; it was left untouched`,
    };
  }

  const record = baseRecord({ ...input, id });
  const written = writeOperationUnlocked(record);
  if (!written.ok) return { ok: false, error: written.error };
  return { ok: true, record, replay: false };
}

export function beginOperation(input) {
  try {
    return withOperationsStore(() => beginOperationUnlocked(input));
  } catch (err) {
    return { ok: false, error: err?.message ?? String(err) };
  }
}

// ---------------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------------

/** Apply a patch to a record on disk. Returns the new record, or null. */
export function updateOperation(id, patch) {
  return withOperationsStore(() => updateOperationUnlocked(id, patch));
}

function updateOperationUnlocked(id, patch) {
  const record = readOperation(id);
  if (!record) return null;
  const value = typeof patch === 'function' ? patch(record) : patch;
  const next = { ...record, ...value, updatedAt: nowIso() };
  const written = writeOperationUnlocked(next);
  if (!written.ok) throw new OperationStoreError(written.error);
  return next;
}

/** Move to a phase, optionally with a step counter and a note for the UI. */
export function recordPhase(id, phase, progress = null) {
  // The contract's progress is a step counter with a note, all three together or
  // nothing. A note on its own would be a shape no client is required to decode.
  const next =
    progress && Number.isInteger(progress.step) && Number.isInteger(progress.of)
      ? { step: progress.step, of: progress.of, note: progress.note ?? '' }
      : null;
  return updateOperation(id, (record) => ({
    phase,
    progress: next ?? record.progress ?? null,
    log: appendLine(record.log, `phase: ${phase}${progress?.note ? ` — ${progress.note}` : ''}`),
  }));
}

function appendLine(log, line) {
  const lines = Array.isArray(log) ? log : [];
  return [...lines, { at: nowIso(), line: String(line).slice(0, 2000) }].slice(-LOG_LIMIT);
}

export function appendLog(id, line) {
  return updateOperation(id, (record) => ({ log: appendLine(record.log, line) }));
}

/**
 * Write the terminal record. After this the operation is answerable until it is
 * pruned, and `op ID` returns the same answer however many times it is asked.
 */
export function finishOperation(id, result, { keepPhase = false } = {}) {
  return withOperationsStore(() => finishOperationUnlocked(id, result, { keepPhase }));
}

function finishOperationUnlocked(id, result, { keepPhase = false } = {}) {
  return updateOperationUnlocked(id, (record) => ({
    state: 'finished',
    // An interrupted operation keeps the phase it actually reached. Overwriting
    // it with "done" would lose the one fact recovery needs: whether the run got
    // far enough to leave a service stopped.
    phase: keepPhase ? (record.phase ?? 'done') : 'done',
    pid: null,
    launchPendingUntil: null,
    // Persist the recovery obligation in the same transaction that retires an
    // interrupted run, so a crash or a live watchdog cannot lose the retry.
    recoveryPending: record.recoveryPending === true || Boolean(keepPhase && result.action === 'interrupted' && record.service && DISRUPTED_PHASES.has(record.phase)),
    finishedAt: nowIso(),
    from: result.from ?? record.from ?? null,
    to: result.to ?? record.to ?? null,
    result: {
      ok: result.ok === true,
      action: result.action,
      reasonCode: result.reasonCode ?? null,
      message: result.message ?? '',
      from: result.from ?? record.from ?? null,
      to: result.to ?? record.to ?? null,
      exitCode: result.exitCode ?? null,
      output: result.output ?? null,
    },
    log: appendLine(record.log, `finished: ${result.action}${result.reasonCode ? ` (${result.reasonCode})` : ''}`),
  }));
}

/**
 * Transfer a queued request, or a newly launched detached request, to exactly
 * one worker. The claim and owner-pid update are one metadata transaction, so
 * two `op-run` processes cannot both pass the state check and execute the body.
 */
export function claimOperation(id, { pid = process.pid, parentPid = process.ppid, now = Date.now() } = {}) {
  try {
    return withOperationsStore(() => {
      const record = readOperation(id);
      if (!record) return { ok: false, reasonCode: REASON.badArgument, error: `there is no operation ${id}` };
      if (record.state === 'finished') {
        return {
          ok: false,
          reasonCode: REASON.alreadyRunning,
          error: `operation ${id} already finished as ${record.result?.action ?? 'unknown'}`,
          record,
        };
      }
      if (record.workerClaimed === true) {
        return {
          ok: false,
          reasonCode: REASON.alreadyRunning,
          error: `operation ${id} is already claimed by worker pid ${record.pid ?? 'unknown'}`,
          record,
        };
      }

      if (record.state === 'queued') {
        const expires = Date.parse(record.expiresAt ?? '');
        if (!Number.isNaN(expires) && expires <= now) {
          return { ok: false, reasonCode: REASON.expired, error: `operation ${id} expired before a worker claimed it`, record };
        }
      } else if (record.state === 'running') {
        const launchPending = Date.parse(record.launchPendingUntil ?? '') > now;
        // A definite detach failure falls back to withOperation in the launcher
        // process itself, so an unclaimed record already naming this pid is also
        // a valid handoff even after detached has been cleared.
        const belongsToLaunch = record.pid === pid ||
          (record.detached === true && (launchPending || record.pid === parentPid));
        if (!belongsToLaunch) {
          return {
            ok: false,
            reasonCode: REASON.operationInProgress,
            error: `operation ${id} is running and is not awaiting this detached worker`,
            record,
          };
        }
      } else {
        return { ok: false, reasonCode: REASON.badArgument, error: `operation ${id} has invalid state ${record.state}`, record };
      }

      const claimed = updateOperationUnlocked(id, {
        state: 'running',
        startedAt: record.startedAt ?? nowIso(),
        updatedAt: nowIso(),
        pid,
        workerClaimed: true,
        launchPendingUntil: null,
        mode: record.state === 'queued' ? 'queued' : record.mode,
      });
      return { ok: true, claimed: true, record: claimed };
    });
  } catch (err) {
    return { ok: false, reasonCode: REASON.internal, error: err?.message ?? String(err) };
  }
}

// ---------------------------------------------------------------------------
// Queue
// ---------------------------------------------------------------------------

/**
 * Park a request until the machine is idle.
 *
 * The expiry is not optional. A "restart when idle" that is still waiting three
 * days later is not a request anybody remembers making, and applying it then is
 * worse than dropping it.
 *
 * At most one queued request per (kind, service|target|actionId): a person who
 * presses "update when idle" twice meant it once, and two records would run the
 * same update twice in a row.
 */
export function queueOperation(input) {
  const ttlMs = Math.min(
    Number.isFinite(input.ttlMs) && input.ttlMs > 0 ? input.ttlMs : DEFAULT_QUEUE_TTL_MS,
    MAX_QUEUE_TTL_MS,
  );
  const expiresAt = input.expiresAt ?? new Date(Date.now() + ttlMs).toISOString();

  try {
    return withOperationsStore(() => {
      // Bind or validate the incoming id before changing the existing slot. In
      // particular, replaying an older replaced id must not cancel the request
      // that replaced it.
      const opened = beginOperationUnlocked({
        ...input,
        state: 'queued',
        phase: 'queued',
        mode: 'queued',
        queueRequested: true,
        expiresAt,
      });
      if (!opened.ok || opened.replay) return { ...opened, replaced: null };

      const slot = `${input.kind}:${input.service ?? input.target ?? input.actionId ?? '-'}`;
      let replaced = null;
      try {
        for (const record of listOperations({ limit: HISTORY_LIMIT, state: 'queued' })) {
          if (record.id === opened.record.id) continue;
          if (`${record.kind}:${record.service ?? record.target ?? record.actionId ?? '-'}` !== slot) continue;
          finishOperationUnlocked(record.id, {
            ok: true,
            action: 'cancelled',
            reasonCode: REASON.cancelled,
            message: 'replaced by a newer request for the same thing',
          });
          replaced = record.id;
        }
      } catch (err) {
        // Keep the older request authoritative if its retirement could not be
        // persisted. This compensating write is itself checked and will throw
        // explicitly if the store has become unusable.
        finishOperationUnlocked(opened.record.id, {
          ok: false,
          action: 'cancelled',
          reasonCode: REASON.internal,
          message: 'the previous queued request could not be replaced safely',
        });
        throw err;
      }
      return { ...opened, replaced };
    });
  } catch (err) {
    return { ok: false, error: err?.message ?? String(err), replaced: null };
  }
}

/** Queued records that have not expired, oldest first: a queue, not a stack. */
export function dueQueue({ now = Date.now() } = {}) {
  return listOperations({ limit: HISTORY_LIMIT, state: 'queued' })
    .filter((record) => {
      const expires = Date.parse(record.expiresAt ?? '');
      return Number.isNaN(expires) || expires > now;
    })
    .sort((a, b) => String(a.requestedAt ?? '').localeCompare(String(b.requestedAt ?? '')));
}

/** Retire queued requests nobody is waiting for any more. Returns what expired. */
export function expireQueue({ now = Date.now() } = {}) {
  return withOperationsStore(() => {
    const expired = [];
    for (const record of listOperations({ limit: HISTORY_LIMIT, state: 'queued' })) {
      const expires = Date.parse(record.expiresAt ?? '');
      if (Number.isNaN(expires) || expires > now) continue;
      const finished = finishOperationUnlocked(record.id, {
        ok: false,
        action: 'expired',
        reasonCode: REASON.expired,
        message: `the queued ${record.kind} expired at ${record.expiresAt} without the machine becoming idle`,
      });
      if (finished) expired.push(finished);
    }
    return expired;
  });
}

/**
 * Cancel an operation.
 *
 * A queued request is simply retired. A running one is NOT killed: there is no
 * safe way to abort an installer from outside it, and pretending otherwise would
 * be the more dangerous lie. It answers `conflict` and says so.
 */
export function cancelOperation(id) {
  return withOperationsStore(() => cancelOperationUnlocked(id));
}

function cancelOperationUnlocked(id) {
  const record = readOperation(id);
  if (!record) {
    return { ok: false, action: 'failed', reasonCode: REASON.badArgument, message: `there is no operation ${id}` };
  }
  if (record.state === 'finished') {
    return {
      ok: false,
      action: 'conflict',
      reasonCode: REASON.alreadyRunning,
      message: `operation ${id} already finished as ${record.result?.action ?? 'unknown'}`,
      op: summarize(record),
    };
  }
  if (record.state === 'queued') {
    const finished = finishOperationUnlocked(id, {
      ok: true,
      action: 'cancelled',
      reasonCode: REASON.cancelled,
      message: `the queued ${record.kind} was cancelled before it ran`,
    });
    return { ok: true, action: 'cancelled', reasonCode: null, message: `cancelled the queued ${record.kind}`, op: summarize(finished) };
  }
  return {
    ok: false,
    action: 'conflict',
    reasonCode: REASON.operationInProgress,
    message: `operation ${id} is running (${record.phase ?? 'no phase'}) and cannot be cancelled; a running install is never aborted from outside`,
    op: summarize(record),
  };
}

// ---------------------------------------------------------------------------
// Recovery
// ---------------------------------------------------------------------------

/** Return a claimed request to its original queue when a fresh busy check changes. */
export function deferQueuedOperation(id, result) {
  return withOperationsStore(() => {
    const record = readOperation(id);
    if (!record || record.state !== 'running' || record.queueRequested !== true || record.pid !== process.pid) {
      throw new OperationStoreError(`operation ${id} is not this process's claimed queue request`);
    }
    if (Date.parse(record.expiresAt ?? '') <= Date.now()) {
      return finishOperationUnlocked(id, { ok: true, action: 'expired', reasonCode: REASON.expired, message: 'the queued request expired while waiting for an idle window' });
    }
    return updateOperationUnlocked(id, {
      state: 'queued', phase: 'queued', pid: null, startedAt: null, workerClaimed: false, result: null,
      log: appendLine(record.log, `waiting for idle: ${result.message ?? result.reasonCode ?? 'busy'}`),
    });
  });
}

export const POWER_TRANSITION_GRACE_MS = 120000;

/** Dispatch acknowledgement is not completion; allow the OS time to transition. */
export function markPowerDispatch(id, phase) {
  return updateOperation(id, { phase, transitionPendingUntil: new Date(Date.now() + POWER_TRANSITION_GRACE_MS).toISOString() });
}

function powerTransitionPending(record, bootId, now = Date.now()) {
  return record.state === 'running' && ['boot', 'sleep'].includes(record.kind) &&
    ['rebooting', 'suspending'].includes(record.phase) && record.bootId === bootId &&
    Date.parse(record.transitionPendingUntil ?? '') > now;
}

export function pendingPowerOperation({ excludeId = null } = {}) {
  const bootId = bootIdentity();
  return listOperations({ state: 'running' }).find((record) => record.id !== excludeId && powerTransitionPending(record, bootId)) ?? null;
}

/**
 * Judge one record whose owner process is gone.
 *
 * Boot and sleep are the interesting cases, and the rule is deliberately strict.
 * A shorter uptime is NOT on its own evidence that the reboot we asked for is
 * the reboot that happened: the machine could have been power cycled, or told to
 * reboot by something else, or come back into the system it was already in
 * because the arming silently failed. So a boot only reads as `rebooted` when
 * BOTH the boot identity changed AND the system now running is the one that was
 * asked for. Anything less is an outcome nobody observed, and it says so.
 *
 * A suspend leaves no trace at all once the machine is awake again, so an
 * interrupted sleep is always reported as unknown rather than guessed at.
 */
export function judgeInterrupted(record, { systemId = null, bootId = bootIdentity() } = {}) {
  const bootChanged = Boolean(record.bootId && bootId) && record.bootId !== bootId;

  if (record.kind === 'boot') {
    if (record.noReboot === true || record.phase !== 'rebooting') {
      return { ok: false, action: 'interrupted', reasonCode: REASON.interrupted, message: 'the operation stopped before a reboot was dispatched; its outcome is unknown' };
    }
    if (!bootChanged) {
      return {
        ok: false,
        action: 'interrupted',
        reasonCode: REASON.interrupted,
        message: `the reboot into ${record.target} was dispatched but this machine has not rebooted since, so the outcome is not known`,
      };
    }
    if (record.target && systemId && record.target === systemId) {
      return {
        ok: true,
        action: 'rebooted',
        reasonCode: null,
        message: `the machine rebooted and is now running ${systemId}, which is the target that was asked for`,
      };
    }
    return {
      ok: false,
      action: 'failed',
      reasonCode: REASON.interrupted,
      message: `the machine rebooted but is running ${systemId ?? 'an unknown system'}, not the requested ${record.target}`,
    };
  }

  if (record.kind === 'sleep') {
    return {
      ok: false,
      action: 'interrupted',
      reasonCode: REASON.interrupted,
      message:
        'the suspend was dispatched and the process that dispatched it is gone; whether the machine actually slept cannot be observed after the fact',
    };
  }

  return {
    ok: false,
    action: 'interrupted',
    reasonCode: REASON.interrupted,
    message: `the ${record.kind}${record.service ? ` of ${record.service}` : ''} stopped during "${
      record.phase ?? 'an unrecorded phase'
    }": the process running it (pid ${record.pid ?? 'unknown'}) is gone`,
  };
}

/**
 * Finish every record whose process is gone.
 *
 * Only records this host owns are judged. A record written on another host (a
 * restored home directory, a shared profile) is left alone rather than declared
 * dead on the strength of a pid that means nothing here.
 */
export function recoverOperations({ systemId = null, bootId = bootIdentity(), now = Date.now() } = {}) {
  return withOperationsStore(() => {
    const recovered = [];
    for (const record of listOperations({ limit: HISTORY_LIMIT })) {
      if (record.state !== 'running') continue;
      if (powerTransitionPending(record, bootId, now)) continue;
      const sameBoot = !record.bootId || !bootId || record.bootId === bootId;
      // A detached launcher gets a bounded grace period in which to create the
      // worker and let it claim the record. The launcher pid may disappear
      // before the child reaches Node, especially through Windows CIM.
      if (sameBoot && record.workerClaimed !== true && Date.parse(record.launchPendingUntil ?? '') > now) continue;
      if (sameBoot && record.pid === process.pid) continue;
      if (sameBoot && Number.isInteger(record.pid) && processIsAlive(record.pid)) continue;
      const verdict = judgeInterrupted(record, { systemId, bootId });
      const finished = finishOperationUnlocked(record.id, { ...verdict, from: record.from, to: record.to }, { keepPhase: true });
      if (finished) recovered.push(finished);
    }
    return recovered;
  });
}

/**
 * The same judgement, without writing anything.
 *
 * `status` must never mutate, but it must also never report an operation as
 * still running when its process is plainly gone. So it derives the answer at
 * read time and leaves the file for a mutating command to fix.
 */
export function deriveState(record, { systemId = null, bootId = bootIdentity(), now = Date.now() } = {}) {
  if (record.state !== 'running') return record;
  if (powerTransitionPending(record, bootId, now)) return record;
  const sameBoot = !record.bootId || !bootId || record.bootId === bootId;
  if (sameBoot && record.workerClaimed !== true && Date.parse(record.launchPendingUntil ?? '') > now) return record;
  if (sameBoot && record.pid === process.pid) return record;
  if (sameBoot && Number.isInteger(record.pid) && processIsAlive(record.pid)) return record;
  const verdict = judgeInterrupted(record, { systemId, bootId });
  return { ...record, state: 'finished', derived: true, pid: null, finishedAt: new Date(now).toISOString(), result: { ...verdict, from: record.from, to: record.to } };
}

/**
 * Services an interrupted operation may have left stopped, with the phase it
 * reached, so the caller can decide what putting them back means.
 */
export function servicesNeedingRecovery(records) {
  const needing = new Map();
  for (const record of records) {
    if (!record.service) continue;
    const phase = record.phase ?? '';
    if (!DISRUPTED_PHASES.has(phase)) continue;
    needing.set(record.service, record);
  }
  return [...needing.values()];
}

// ---------------------------------------------------------------------------
// Retention
// ---------------------------------------------------------------------------

/** Keep the newest HISTORY_LIMIT finished records, and nothing older than 30 days. */
export function pruneOperations({ now = Date.now() } = {}) {
  return withOperationsStore(() => pruneOperationsUnlocked({ now }));
}

function pruneOperationsUnlocked({ now = Date.now() } = {}) {
  let names = [];
  try {
    names = fs.readdirSync(operationsDir());
  } catch {
    return { removed: [] };
  }

  const finished = [];
  for (const name of names) {
    if (!name.endsWith('.json')) continue;
    const record = readOperation(name.slice(0, -5));
    if (!record || record.state !== 'finished' || record.recoveryPending === true) continue;
    finished.push(record);
  }
  finished.sort((a, b) => String(b.finishedAt ?? b.requestedAt ?? '').localeCompare(String(a.finishedAt ?? a.requestedAt ?? '')));

  const removed = [];
  finished.forEach((record, index) => {
    const at = Date.parse(record.finishedAt ?? record.requestedAt ?? '');
    const tooOld = !Number.isNaN(at) && now - at > HISTORY_MAX_AGE_MS;
    if (index < HISTORY_LIMIT && !tooOld) return;
    try {
      fs.rmSync(operationPath(record.id), { force: true });
      removed.push(record.id);
    } catch {
      /* it will be reconsidered next time */
    }
  });
  return { removed };
}
