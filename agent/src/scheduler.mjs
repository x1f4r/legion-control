// One scheduled cycle: what the timer on each machine actually runs.
//
// The first version pointed its timer at `legionctl update`, which with no
// --service picks the FIRST service and only that one. A machine with two
// services therefore had automatic updates that reached one of them, while the
// UI reported that automatic updates were on for the system. The second service
// could sit out of date indefinitely and nothing anywhere said so.
//
// A cycle is four things, in this order, and the first two matter more than the
// third:
//
//   1. RECOVER. Finish operations whose process is gone, put back services a
//      killed run left stopped, drop locks nobody owns, prune old records. This
//      comes first because everything after it assumes a machine in a known
//      state — and because the run that needs recovering is exactly the run the
//      same timer's time limit killed.
//   2. DRAIN THE QUEUE. "Restart when idle" is a promise, and something has to
//      keep it. That something is this, on every tick.
//   3. UPDATE. Every eligible service, in order, each with its own policy, its
//      own child operation record and its own outcome.
//   4. EXPIRE. Retire queued requests nobody is waiting for any more.
//
// The whole cycle holds ONE machine lock, so a controller pressing restart while
// it runs is refused with the reason rather than racing an installer. One
// deferred or failed service never stops the others, and a failed install for
// one service never rolls back another.

import { REASON } from './contract.mjs';
import { acquireOperationLock, acquireServiceLock, clearStaleLocks } from './lock.mjs';
import { log, note } from './log.mjs';
import {
  appendLog,
  beginOperation,
  cancelOperation,
  dueQueue,
  expireQueue,
  finishOperation,
  listOperations,
  newOperationId,
  pruneOperations,
  pendingPowerOperation,
  readOperation,
  recordPhase,
  recoverOperations,
  servicesNeedingRecovery,
  summarize,
  updateOperation,
} from './operations.mjs';
import { stillAtWork } from './operate.mjs';
import { orderServices, scheduledEligibility } from './policy.mjs';
import { unsettledWork, withOperationContext } from './process-runner.mjs';
import { busyReasonCode, checkAllBusy } from './probes/busy.mjs';
import { describeHealth } from './probes/health.mjs';
import { livenessOf, recoveryEvidence, startAndSettle, waitForServiceHealth } from './service.mjs';
import { rollBackInterrupted, rollbackOwed, runRestart, runUpdate } from './update.mjs';
import { runAction } from './actions.mjs';
import { ensureServiceStateWritable, serviceState } from './state.mjs';

/**
 * Put back anything an interrupted run left in a state nobody chose.
 *
 * Only services an interrupted operation actually reached a disruptive phase for
 * are touched. A cycle that died while resolving a version never stopped
 * anything, and starting a service somebody deliberately stopped would be its
 * own kind of damage. Putting one back means what contract section 5.5 says:
 * up again, and, where an update left a version that is neither the one it
 * started from nor the one it was going to, rolled back to the first.
 */
export async function recoverMachine(config, { lockFiles = [] } = {}) {
  const interrupted = recoverOperations({ systemId: config.system.id });
  const cleared = clearStaleLocks(lockFiles);
  const restored = [];
  const pending = listOperations({ recoveryPending: true, limit: 200 });
  const completed = (owing) => {
    for (const record of owing) updateOperation(record.id, { recoveryPending: false });
  };
  // How an obligation ended, or why it stays, is written on every record that
  // owed it, so `op ID` answers the question a cycle cannot: what recovery did
  // about that run, or what is still wrong and what it is waiting for. A
  // reason to wait is written once, and again only when it changes. `record`
  // is the run the answer is about.
  const settled = (owing, record, service, action, message) => {
    restored.push({ service: service.id, action, settled: true, message, opId: record.id, phase: record.phase ?? null });
    log(message, 'recover');
    for (const entry of owing) appendLog(entry.id, `recovered: ${message}`);
    completed(owing);
  };
  const owed = (owing, record, service, action, message, reasonCode = null) => {
    restored.push({ service: service.id, action, settled: false, reasonCode, message, opId: record.id, phase: record.phase ?? null });
    log(message, 'recover');
    const line = `recovery is still owed: ${message}`;
    for (const entry of owing) {
      if (readOperation(entry.id)?.log?.at(-1)?.line !== line) appendLog(entry.id, line);
    }
  };

  for (const { service: id, records: owing } of servicesNeedingRecovery(pending)) {
    const service = config.services.find((entry) => entry.id === id);
    if (!service) {
      completed(owing);
      continue;
    }
    // Only a positive reading settles anything (contract section 5.5). The stop
    // loop's isRunning() fails closed, and recovery used to read that as "it is
    // up": a process probe that could not run dropped the obligation, and the
    // service an interrupted update had stopped stayed off for good.
    const evidence = await recoveryEvidence(service);
    if (evidence.state === 'unknown') {
      owed(owing, owing[0], service, 'unknown', `${service.name} was neither started nor taken for running, because nothing can tell whether it is: ${evidence.error}`);
      continue;
    }
    const rollback = rollbackOwed(service, owing);
    const record = rollback.record ?? owing[0];
    if (evidence.state === 'running' && rollback.state !== 'owed') {
      settled(owing, record, service, 'already-running', `${service.name} was already running after an interrupted ${record.kind} (${record.phase})${unrolled(rollback)}`);
      continue;
    }
    // The cycle owns the machine mutex, but the external watchdog understands
    // only this service lock. Recovery must obey it just like an update does.
    const serviceLock = acquireServiceLock(service.lockFile, 'recovery');
    if (!serviceLock.ok) {
      owed(owing, record, service, 'conflict', `${service.name} was not put back: ${serviceLock.message}`, serviceLock.reasonCode);
      continue;
    }
    try {
      ensureServiceStateWritable(service);
      const ending = await putBack(config, service, owing, { evidence, rollback });
      if (ending.settled) settled(owing, ending.record, service, ending.action, ending.message);
      else owed(owing, ending.record, service, ending.action, ending.message, ending.reasonCode ?? null);
    } catch (error) {
      owed(owing, record, service, 'start-failed', `${service.name} could not be put back: ${error?.message ?? String(error)}`, REASON.internal);
    } finally {
      serviceLock.release();
    }
  }

  const pruned = pruneOperations();
  return {
    interrupted: interrupted.map(summarize),
    locksCleared: cleared.map((entry) => entry.file),
    restored,
    pruned: pruned.removed.length,
  };
}

/** Why nothing was rolled back, where the version could not say whether anything was owed. */
function unrolled(rollback) {
  return rollback.state === 'unknown' ? `; nothing was rolled back, because ${rollback.error}` : '';
}

/**
 * Put one service back, with its lock held: roll it back where its version
 * says an interrupted update left it half installed, and otherwise start it.
 * Returns how that ended, { settled, record, action, reasonCode, message },
 * for recoverMachine to note on the records that owed it.
 */
async function putBack(config, service, owing, { evidence, rollback }) {
  if (rollback.state === 'owed') {
    return { record: rollback.record, ...(await rollBackInterrupted(config, service, rollback, { running: evidence.state === 'running' })) };
  }

  const record = rollback.record ?? owing[0];
  const after = `after an interrupted ${record.kind} (${record.phase})`;
  note(`${service.name} was left stopped by an interrupted ${record.kind}; starting it`, 'recover');
  await startAndSettle(service);
  const health = await waitForServiceHealth(service, 60000);
  if (!health.ok) return { settled: false, record, action: 'start-failed', message: `${service.name} could not be restarted ${after}: ${describeHealth(health)}` };

  // A version command can need the service it asks about, so a version that
  // could not be read with the service down is read again now that it is up.
  // What that reading owes goes through the gate any running service gets.
  const again = rollback.state === 'unknown' ? rollbackOwed(service, owing) : rollback;
  if (again.state !== 'owed') return { settled: true, record, action: 'started', message: `${service.name} was restarted ${after}${unrolled(again)}` };
  return { record: again.record, ...(await rollBackInterrupted(config, service, again, { running: true })) };
}

/** Run one queued request now, reusing the cycle's lock and the record's own id. */
async function runQueued(config, record, { lock }) {
  const service = record.service ? config.services.find((entry) => entry.id === record.service) : null;
  const shared = { existingOpId: record.id, existingLock: lock, force: record.force === true, noReboot: record.noReboot === true, mode: 'queued' };

  if (record.kind === 'restart' || record.kind === 'update') {
    if (!service) {
      finishOperation(record.id, {
        ok: false,
        action: 'failed',
        reasonCode: REASON.unknownService,
        message: `the queued ${record.kind} names ${record.service}, which this machine no longer has`,
      });
      return { opId: record.id, kind: record.kind, ok: false, action: 'failed' };
    }
    const result =
      record.kind === 'restart' ? await runRestart(config, service, shared) : await runUpdate(config, service, shared);
    return { opId: record.id, kind: record.kind, service: service.id, ...result };
  }

  if (record.kind === 'run') {
    return { opId: record.id, kind: record.kind, ...(await runAction(config, record.actionId, shared)) };
  }

  // boot and sleep are queued too, and they are the reason a queue needs an
  // expiry: a machine that reboots itself hours later, with nobody watching, is
  // a surprise. They are imported lazily so the cycle does not drag the boot
  // adapters into every process that only wanted to update something.
  if (record.kind === 'boot' || record.kind === 'sleep') {
    const { runBoot, runSleep } = await import('./power.mjs');
    const result =
      record.kind === 'boot'
        ? await runBoot(config, record.target, shared)
        : await runSleep(config, shared);
    return { opId: record.id, kind: record.kind, target: record.target ?? null, ...result };
  }

  finishOperation(record.id, {
    ok: false,
    action: 'failed',
    reasonCode: REASON.badArgument,
    message: `this agent does not know how to run a queued ${record.kind}`,
  });
  return { opId: record.id, kind: record.kind, ok: false, action: 'failed' };
}

/**
 * Take up every queued request the machine is now idle enough to honour.
 *
 * Idleness is checked once per request rather than once for the batch: running
 * the first one can legitimately make the machine busy again, and the second
 * request has no business ignoring that.
 */
export async function drainQueue(config, { lock, force = false } = {}) {
  const results = [];
  for (const record of dueQueue()) {
    const liveness = {};
    for (const service of config.services) liveness[service.id] = livenessOf(service);
    const { busy } = await checkAllBusy(config, { liveness });
    // A request queued with --force by an agent before 3.1.0, which refuses
    // that combination now, keeps what it asked for.
    if (busy.busy && !force && record.force !== true) {
      results.push({
        opId: record.id,
        kind: record.kind,
        service: record.service ?? null,
        action: 'queued',
        reasonCode: busyReasonCode(busy),
        message: `still waiting for an idle window (${busy.reason})`,
      });
      continue;
    }
    note(`running the queued ${record.kind}${record.service ? ` of ${record.service}` : ''}`, 'queue');
    results.push(await runQueued(config, record, { lock }));
    // A request taken up now would only be refused, and a refusal would spend
    // it. Like everything after a power transition, it waits in the queue.
    if (pendingPowerOperation() || (await unsettledWork())) break;
  }
  return results;
}

/**
 * What a cycle WOULD do, without taking the lock or touching anything.
 *
 * Used by doctor and by the clients' policy preview, so a person can see why a
 * machine is not updating without having to wait for the next tick to find out.
 */
export function previewCycle(config) {
  const { ordered, cycles } = orderServices(config.services);
  return {
    ok: true,
    action: 'cycled',
    dryRun: true,
    orderingCycles: cycles.map((path) => path.join(' -> ')),
    queued: dueQueue().map(summarize),
    children: ordered.map((service) => {
      const eligibility = scheduledEligibility(config, service, { state: serviceState(service, { readOnly: true }) });
      return {
        service: service.id,
        name: service.name,
        wouldRun: eligibility.eligible,
        reasonCode: eligibility.reasonCode,
        message: eligibility.reason,
      };
    }),
  };
}

/**
 * The whole cycle. This is what the systemd timer, the Windows task and the
 * launchd job run.
 *
 * Returns a per-service outcome list, not one aggregate answer: "the cycle
 * succeeded" is meaningless on a machine where one service updated, one was
 * paused and one is behind because its registry is unreachable.
 */
export async function runCycle(config, options = {}) {
  const { force = false, services = null, dryRun = false, opId: requestedId = null } = options;
  if (dryRun) return previewCycle(config);

  const started = Date.now();
  const id = requestedId ?? newOperationId();
  const opened = beginOperation({
    id,
    kind: 'cycle',
    mode: 'scheduled',
    force,
    services,
    systemId: config.system.id,
  });
  // As for every mutation, an id that names a different cycle is a bad --op
  // argument, never a lock conflict (operate.mjs says why).
  if (opened.conflict) {
    return { ok: false, reasonCode: REASON.badArgument, opId: null, message: opened.error, children: [], refusal: { command: 'cycle', argument: '--op' } };
  }
  if (!opened.ok) {
    return {
      ok: false,
      action: 'failed',
      reasonCode: REASON.internal,
      message: `the cycle record could not be written (${opened.error}), so no maintenance was attempted`,
      children: [],
    };
  }
  if (opened.replay) {
    if (opened.record.state === 'finished') {
      return { ...opened.record.result, children: opened.record.children ?? [], opId: id, op: summarize(opened.record), replayed: true };
    }
    return {
      ok: true, action: 'already-running', reasonCode: REASON.alreadyRunning, opId: id, op: summarize(opened.record), replayed: true,
      detached: opened.record.detached === true, children: opened.record.children ?? [], message: `cycle ${id} is already ${opened.record.state}; it was not started again`,
    };
  }

  // A cycle record must carry its children from the moment it exists, because a
  // client can read it while it is still running.
  updateOperation(id, { children: [] });

  const acquired = acquireOperationLock({ kind: 'cycle', opId: id, target: 'cycle' });
  if (!acquired.ok) {
    const result = {
      ok: false,
      action: 'conflict',
      reasonCode: acquired.reasonCode ?? REASON.operationInProgress,
      message: acquired.message,
      conflict: acquired.conflictDetail ?? null,
    };
    finishOperation(id, result);
    return { ...result, opId: id };
  }
  const lock = acquired.lock;

  const report = { recovery: null, queue: [], children: [], expired: [] };
  // From the services phase on, a cycle that throws may have left an update
  // half done, so its failure is unknown rather than failed (see the catch).
  let servicesReached = false;
  try {
    // The cycle's record, and every child's, lives as long as this hold on the lock.
    updateOperation(id, { lockOwner: lock.opId });
    const transitioning = pendingPowerOperation({ excludeId: id });
    if (transitioning) {
      const result = { ok: false, action: 'conflict', reasonCode: REASON.operationInProgress, message: `operation ${transitioning.id} is awaiting a power transition` };
      finishOperation(id, result);
      return { ...result, opId: id, ...report };
    }
    // Recovery starts services, so it waits for a command an earlier run could
    // not see stop just as every other mutation does. Putting a service back
    // while its installer may still be running is the damage it exists to undo.
    const unsettled = await unsettledWork();
    if (unsettled) {
      const result = unsettled.conflict
        ? { ok: false, action: 'conflict', reasonCode: REASON.operationInProgress, message: unsettled.message, conflict: unsettled.conflict }
        : { ok: false, action: 'failed', reasonCode: REASON.internal, message: unsettled.message };
      finishOperation(id, result);
      return { ...result, opId: id, ...report };
    }
    lock.phase('recovering');
    recordPhase(id, 'recovering');
    report.recovery = await withOperationContext(id, () => recoverMachine(config, { lockFiles: config.services.map((service) => service.lockFile) }));

    lock.phase('queued');
    recordPhase(id, 'queued');
    report.queue = await drainQueue(config, { lock, force });

    if (pendingPowerOperation()) {
      const result = { ok: true, action: 'cycled', reasonCode: null, message: 'a queued power transition was dispatched; further maintenance waits until it settles' };
      finishOperation(id, result);
      return { ...result, opId: id, ...report };
    }

    lock.phase('services');
    recordPhase(id, 'services');
    servicesReached = true;
    const selected = services ? config.services.filter((service) => services.includes(service.id)) : config.services;
    const { ordered, cycles } = orderServices(selected);
    if (cycles.length > 0) {
      log(`WARNING: the service ordering has a cycle: ${cycles.map((path) => path.join(' -> ')).join('; ')}`, 'cycle');
    }

    for (const [index, service] of ordered.entries()) {
      updateOperation(id, { progress: { step: index + 1, of: ordered.length, note: service.name } });
      const eligibility = force
        ? { eligible: true, reasonCode: null, reason: 'forced' }
        : scheduledEligibility(config, service, {});
      if (!eligibility.eligible) {
        report.children.push({
          opId: null,
          service: service.id,
          name: service.name,
          action: 'deferred',
          reasonCode: eligibility.reasonCode,
          message: eligibility.reason,
        });
        continue;
      }
      lock.phase(`services:${service.id}`);
      const result = await runUpdate(config, service, {
        mode: 'scheduled',
        force,
        existingLock: lock,
      });
      report.children.push({
        opId: result.opId ?? null,
        service: service.id,
        name: service.name,
        action: result.action,
        reasonCode: result.reasonCode ?? null,
        message: result.message,
        from: result.from ?? null,
        to: result.to ?? null,
      });
    }

    lock.phase('done');
    report.expired = expireQueue().map(summarize);

    // The record of a cycle carries what it did to each service, so `op <id>`
    // answers the whole question without a second call per child.
    updateOperation(id, {
      children: report.children.map((child) => ({
        opId: child.opId ?? null,
        service: child.service ?? null,
        action: child.action ?? null,
        reasonCode: child.reasonCode ?? null,
      })),
    });

    const failed = report.children.filter((child) => child.action === 'failed');
    const updated = report.children.filter((child) => child.action === 'updated');
    const walked =
      report.children.length === 0
        ? 'no services on this machine are eligible for a scheduled update'
        : `${updated.length} updated, ${report.children.length - updated.length - failed.length} left alone, ${failed.length} failed`;
    // A service recovery could not put back is named in the cycle's own answer
    // too, which is what a client shows, with what it is waiting for.
    const owed = report.recovery.restored.filter((entry) => entry.settled === false);
    const message = owed.length === 0 ? walked : `${walked}; recovery is still owed: ${owed.map((entry) => entry.message).join('; ')}`;

    // A failed cycle says why with the reason of the first service that failed,
    // so it is never the failure with no code that contract 3 exists to remove.
    // And a cycle whose own recovery left a start command that could not be
    // seen to stop ends as any operation does in that case (operate.mjs).
    const concluded = {
      ok: failed.length === 0,
      action: failed.length === 0 ? 'cycled' : 'failed',
      reasonCode: failed.length === 0 ? null : failed[0].reasonCode ?? REASON.internal,
      message,
    };
    const ownUnsettled = await unsettledWork({ opId: id });
    const ending = ownUnsettled ? stillAtWork(concluded, ownUnsettled) : concluded;
    const summary = { ...ending, durationMs: Date.now() - started, ...report };
    finishOperation(id, ending);
    return { ...summary, opId: id };
  } catch (err) {
    const message = err?.message ?? String(err);
    log(`cycle threw: ${message}`, 'cycle');
    const result = { ok: false, action: 'failed', reasonCode: REASON.internal, message, ...(servicesReached ? { outcome: 'unknown' } : {}) };
    finishOperation(id, result);
    return { ...result, opId: id, ...report };
  } finally {
    lock.release();
  }
}
