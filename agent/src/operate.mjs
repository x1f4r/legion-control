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
// contract lists for that kind. The one exception is an --op id that already
// names a different request: nothing was attempted, so there is no action, and
// the reply carries `refusal` for the caller to answer in the refusal shape.
//
// It is also where the command ledger is honoured (process-runner.mjs). A
// command an earlier operation could not see stop may still be changing the
// machine, and the next operation waits for it exactly as it waits for a lock.

import { MUTATING_PHASES, producedOutcome, REASON } from './contract.mjs';
import { acquireOperationLock, acquireServiceLock } from './lock.mjs';
import { log } from './log.mjs';
import { unsettledWork, withOperationContext } from './process-runner.mjs';
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
 * What an operation concluded, once one of its own commands ran out of time
 * and could not be seen to stop. That command may still be doing its work, so
 * the outcome is unknown and no client sends the operation again.
 *
 * Section 5.6 lets a producer state unknown only where the result says it
 * failed, which is what keeps a reply and its record, each reduced from its
 * own fields, saying the same. A failure is stated as unknown as it stands.
 * Anything else the body came to after that command, a deferral or a no-op, is
 * not something the operation can stand behind while the command may still be
 * at work, so it becomes the command's timeout. A result that is still pending
 * is left alone: the work is still owned, and its ending is judged when it
 * comes.
 */
export function stillAtWork(result, unsettled) {
  const outcome = producedOutcome(result);
  if (outcome === 'failed') return { ...result, outcome: 'unknown' };
  if (outcome === 'unknown' || outcome === 'pending') return result;
  const { conflict, ...rest } = result;
  return {
    ...rest,
    ok: false,
    action: 'failed',
    reasonCode: REASON.timedOut,
    message: result.message ? `${result.message}, but ${unsettled.message}` : unsettled.message,
    outcome: 'unknown',
  };
}

/**
 * Finish a record whose request can no longer be run at all, because the
 * service or action it names was removed from the setup after it was asked
 * for. Whoever takes it up, a cycle or a detached worker, finishes it with
 * why; otherwise nobody ever would, and a client following it would wait for
 * an ending that never comes. It is taken up the way withOperation takes up a
 * record, so one another process already took is left to that process.
 */
export function finishUnrunnable(id, result) {
  const claimed = claimOperation(id);
  if (!claimed.ok) {
    return {
      ok: false,
      action: 'conflict',
      reasonCode: claimed.reasonCode ?? REASON.operationInProgress,
      opId: id,
      op: summarize(claimed.record ?? readOperation(id)),
      replayed: true,
      message: claimed.error,
    };
  }
  const finished = finishOperation(id, result);
  return { ...result, opId: id, op: summarize(finished) };
}

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

    // An id bound to another intent is a bad --op argument. It is never a
    // replay, and never a lock conflict either: `conflict` tells a client to
    // try the same request later, and this request can never succeed.
    if (opened.conflict) {
      return {
        ok: false,
        reasonCode: REASON.badArgument,
        opId: null,
        message: opened.error,
        refusal: { command: kind, argument: '--op' },
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
      // Still running: the client follows it exactly as it follows `accepted`,
      // but `accepted` would claim this request started something.
      return {
        ok: true,
        action: 'already-running',
        reasonCode: REASON.alreadyRunning,
        opId: record.id,
        op: summarize(record),
        replayed: true,
        detached: record.detached === true,
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
    // From here the record lives exactly as long as the operation holding the
    // lock for it, which is what recovery and every read judge it by.
    if (lock.opId) updateOperation(id, { lockOwner: lock.opId });
    const transitioning = pendingPowerOperation({ excludeId: id });
    if (transitioning) {
      const result = { ok: false, action: 'conflict', reasonCode: REASON.operationInProgress, message: `operation ${transitioning.id} is awaiting a power transition` };
      const finished = finishOperation(id, result);
      return { ...result, opId: id, op: summarize(finished) };
    }
    // Replacing the agent touches no service, and is how a machine gets the
    // fix for whatever is holding it, so only it goes ahead regardless. A
    // reboot would clear the ledger too, and is still held: it would cut off
    // whatever the command is in the middle of, which is the very overlap the
    // ledger is there to prevent, and arming one runs commands of its own. A
    // person who judges that worth it can stop the command, or restart the
    // machine, themselves; the next mutation then finds the ledger clear.
    const unsettled = kind === 'self-update' ? null : await unsettledWork();
    if (unsettled) {
      const result = unsettled.conflict
        ? { ok: false, action: 'conflict', reasonCode: REASON.operationInProgress, message: unsettled.message, conflict: unsettled.conflict }
        : { ok: false, action: 'failed', reasonCode: REASON.internal, message: unsettled.message };
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
    const { pendingTransition = false, ...concluded } = await withOperationContext(id, () => body({ opId: id, lock }));
    // A command of this operation that ran out of time and could not be seen
    // to stop may still be doing its work, whatever the body concluded.
    const ownUnsettled = await unsettledWork({ opId: id });
    const result = ownUnsettled ? stillAtWork(concluded, ownUnsettled) : concluded;
    if (pendingTransition) {
      // The lock is given back while the record stays open for the transition,
      // so the lock is no longer what says whether it is anybody's.
      const pending = updateOperation(id, { dispatchAcknowledgement: result, lockOwner: null });
      return { ...result, opId: id, op: summarize(pending) };
    }
    // A queued request the machine was too busy for, or a cycle had too little
    // time left for, goes back to wait for the next one.
    if (existingOpId && readOperation(id)?.queueRequested === true && result.action === 'deferred' && [REASON.busy, REASON.busyUnknown, REASON.timedOut].includes(result.reasonCode)) {
      const queued = deferQueuedOperation(id, result);
      return { ...result, ...(queued.state === 'queued' ? { action: 'queued' } : queued.result), opId: id, op: summarize(queued) };
    }
    const finished = finishOperation(id, result);
    return { ...result, opId: id, op: summarize(finished) };
  } catch (err) {
    const message = err?.stack ? String(err.stack).split('\n')[0] : String(err?.message ?? err);
    log(`${kind} threw: ${message}`, kind);
    // A body that threw once it was changing the machine left it in a state
    // nobody observed. Its failure is reported as unknown, so a client checks
    // again rather than sending the same work a second time.
    const changing = MUTATING_PHASES.has(readOperation(id)?.phase) || Boolean(await unsettledWork({ opId: id }));
    const result = { ok: false, action: 'failed', reasonCode: REASON.internal, message, ...(changing ? { outcome: 'unknown' } : {}) };
    const finished = finishOperation(id, result);
    return { ...result, opId: id, op: summarize(finished) };
  } finally {
    serviceLock?.release();
    if (ownLock) lock.release();
  }
}
