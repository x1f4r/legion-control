// The wrapper every mutating command goes through.
//
// Updating, restarting, rebooting, suspending, running a configured action and
// replacing the agent all have the same three requirements: they must not run
// while another one is running, their outcome must survive the process that was
// asked for them, and asking twice with the same id must not do the work twice.
// This is the one place all three are arranged, so no verb can be added later
// that quietly skips any of them.
//
// The shape of every reply is the same, and clients depend on it:
//
//   ok, action, reasonCode, message, opId, op
//
// where `op` is the record summary and `action` is one of the values the
// contract lists for that kind.

import { REASON } from './contract.mjs';
import { acquireOperationLock, acquireServiceLock } from './lock.mjs';
import { log } from './log.mjs';
import {
  appendLog,
  beginOperation,
  claimOperation,
  deferQueuedOperation,
  finishOperation,
  newOperationId,
  readOperation,
  pendingPowerOperation,
  recordPhase,
  summarize,
  updateOperation,
} from './operations.mjs';

/**
 * Open a record, take the lock, run the body, close the record.
 *
 * The body returns `{ action, ok, reasonCode, message, ... }` — exactly what the
 * record's result should be — and this writes it. A body that throws is recorded
 * as a failure rather than losing the record, because an operation with no
 * ending is the thing the log exists to prevent.
 *
 * `existingLock` lets a cycle hold one exclusion across many services while each
 * service still gets its own record. `existingOpId` lets a queued record be
 * taken up by the cycle that runs it.
 */
export async function withOperation(options, body) {
  const {
    kind,
    service = null,
    target = null,
    actionId = null,
    serviceLockFile = null,
    mode = 'manual',
    force = false,
    noReboot = false,
    opId: requestedId = null,
    systemId = null,
    detached = false,
    existingLock = null,
    existingOpId = null,
  } = options;

  const id = existingOpId ?? requestedId ?? newOperationId();

  // Taking up a queued record: it changes hands, so recovery judges it against a
  // pid that is actually here. The claim is conditional and transactional:
  // another cycle or detached worker that got here first owns the operation.
  if (existingOpId) {
    const claimed = claimOperation(existingOpId);
    if (!claimed.ok) {
      const record = claimed.record ?? readOperation(existingOpId);
      return {
        ok: false,
        action: 'conflict',
        reasonCode: claimed.reasonCode ?? REASON.operationInProgress,
        opId: existingOpId,
        op: summarize(record),
        replayed: true,
        message: claimed.error,
      };
    }
    recordPhase(existingOpId, 'checking-busy');
  } else {
    const opened = beginOperation({
      id,
      kind,
      service,
      target,
      actionId,
      mode,
      force,
      noReboot,
      systemId,
      detached,
      queueRequested: false,
    });

    if (opened.conflict) {
      return {
        ok: false,
        action: 'conflict',
        reasonCode: opened.reasonCode ?? REASON.badArgument,
        opId: id,
        op: summarize(opened.record),
        message: opened.error,
      };
    }
    if (!opened.ok) {
      // Refusing here is deliberate. Running a mutation whose record cannot be
      // persisted means nobody can ever find out what it did.
      return {
        ok: false,
        action: 'failed',
        reasonCode: REASON.internal,
        opId: null,
        message: `the operation record could not be written (${opened.error}), so nothing was started; fix the state directory before retrying`,
      };
    }

    // The same id asked twice. This is the whole point of --op: a client whose
    // link dropped retries, and gets the first answer rather than a second run.
    if (opened.replay) {
      const record = opened.record;
      if (record.state === 'finished') {
        return {
          ...record.result,
          action: record.result?.action ?? 'failed',
          reasonCode: record.result?.reasonCode ?? null,
          opId: record.id,
          op: summarize(record),
          replayed: true,
        };
      }
      return {
        ok: true,
        action: 'accepted',
        reasonCode: REASON.alreadyRunning,
        opId: record.id,
        op: summarize(record),
        replayed: true,
        message: `operation ${record.id} is already ${record.state}${record.phase ? ` (${record.phase})` : ''}; it was not started a second time`,
      };
    }
  }

  let lock = existingLock;
  let serviceLock = null;
  const ownLock = !lock;
  if (ownLock) {
    recordPhase(id, 'locking');
    const acquired = acquireOperationLock({ kind, opId: id, service, serviceLockFile, target: target ?? actionId ?? kind });
    if (!acquired.ok) {
      const result = {
        ok: false,
        action: 'conflict',
        reasonCode: acquired.reasonCode ?? REASON.operationInProgress,
        message: acquired.message,
        conflict: acquired.conflictDetail ?? null,
      };
      finishOperation(id, result);
      return { ...result, opId: id, op: summarize(readOperation(id)) };
    }
    lock = acquired.lock;
    if (lock.compatWarning) appendLog(id, lock.compatWarning);
  }

  try {
    const transitioning = pendingPowerOperation({ excludeId: id });
    if (transitioning) {
      const result = { ok: false, action: 'conflict', reasonCode: REASON.operationInProgress, message: `operation ${transitioning.id} is awaiting a power transition` };
      const finished = finishOperation(id, result);
      return { ...result, opId: id, op: summarize(finished) };
    }
    if (!ownLock && serviceLockFile && lock.compatFile !== serviceLockFile) {
      serviceLock = acquireServiceLock(serviceLockFile, target ?? kind);
      if (!serviceLock.ok) {
        const result = { ok: false, action: 'conflict', reasonCode: serviceLock.reasonCode, message: serviceLock.message };
        const finished = finishOperation(id, result);
        return { ...result, opId: id, op: summarize(finished) };
      }
    }
    const { pendingTransition = false, ...result } = await body({ opId: id, lock });
    if (pendingTransition) {
      const pending = updateOperation(id, { dispatchAcknowledgement: result });
      return { ...result, opId: id, op: summarize(pending) };
    }
    if (existingOpId && readOperation(id)?.queueRequested === true && result.action === 'deferred' && [REASON.busy, REASON.busyUnknown].includes(result.reasonCode)) {
      const queued = deferQueuedOperation(id, result);
      return { ...result, ...(queued.state === 'queued' ? { action: 'queued' } : queued.result), opId: id, op: summarize(queued) };
    }
    const finished = finishOperation(id, result);
    return { ...result, opId: id, op: summarize(finished) };
  } catch (err) {
    const message = err?.stack ? String(err.stack).split('\n')[0] : String(err?.message ?? err);
    log(`${kind} threw: ${message}`, kind);
    const result = { ok: false, action: 'failed', reasonCode: REASON.internal, message };
    const finished = finishOperation(id, result);
    return { ...result, opId: id, op: summarize(finished) };
  } finally {
    serviceLock?.release();
    if (ownLock) lock.release();
  }
}
