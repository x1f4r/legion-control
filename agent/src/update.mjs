// The update cycle, and the restart that shares its gate.
//
// The ordering for a service we install ourselves is the whole point, so it is
// spelled out:
//
//   open the record -> take the machine lock -> RECOVER anything a previous run
//   left half done -> resolve the target version -> compare -> check busy ->
//   pre-warm the cache -> drain -> CHECK BUSY AGAIN -> stop -> install ->
//   verify -> start -> health -> close the record.
//
// Four of those steps are there because of a specific way this went wrong:
//
//   Recovery comes BEFORE the version lookup. It used to come after, so a
//   machine whose registry was unreachable never reached the step that would
//   have restarted the service a killed run had left stopped. The recovery a
//   failure needs must not sit behind a network call that the same failure took
//   out.
//
//   The busy check happens TWICE, and the second one is immediately before the
//   stop. Between the first check and the stop sits a cache warm that can take
//   three attempts of up to three minutes each, and work started inside that
//   window used to be thrown away by a decision made nine minutes earlier. The
//   drain sits in that window too, so work that starts while it runs is seen by
//   the second check rather than stopped.
//
//   A drain runs before the stop where the service has one. Checking is not
//   preventing: only the service itself can refuse new work, and where it can,
//   we ask it to.
//
//   An update is only reported as done when something PROVES it. A command that
//   exits zero has proved that it ran, and nothing else.
//
// Only what the install changed is ever rolled back. A drain the service
// refused, or a stop it did not complete, leaves it exactly as it was. The npm
// path used to answer either with a rollback, which stopped the service that
// had just said it was not ready and reinstalled the version it was running.
//
// We NEVER install while the machine is busy without being told to. An installer
// replaces files underneath a running service, and a service that loads code
// lazily starts failing on files that changed under it. Deferring costs a delay;
// installing under a live turn costs the user's work.

import { describeFailure } from './config.mjs';
import { REASON } from './contract.mjs';
import { withOperation } from './operate.mjs';
import { appendLog, listOperations, recordPhase, updateOperation } from './operations.mjs';
import { runMutationArgv, unsettledWork } from './process-runner.mjs';
import { busyReasonCode, checkAllBusy } from './probes/busy.mjs';
import { describeHealth } from './probes/health.mjs';
import { hasUpdatePath, scheduledEligibility } from './policy.mjs';
import { log, note } from './log.mjs';
import { ensureServiceStateWritable, requireServiceStateSave as saveServiceState, ServiceStateError, serviceState } from './state.mjs';
import * as appProvider from './providers/app.mjs';
import * as commandProvider from './providers/command.mjs';
import * as npmProvider from './providers/npm.mjs';
import {
  canRestart,
  drainService,
  isRunning,
  livenessOf,
  processEvidence,
  restartService,
  serviceHealth,
  startAndSettle,
  stopAndWait,
  waitForServiceHealth,
} from './service.mjs';

const HEALTH_WAIT_MS = 120000;
// How many times an app is asked to apply the SAME staged build before we stop
// asking. Three is enough to ride out a transient failure, and small enough that
// a build which will never apply cannot keep restarting the user's app.
const APPLY_ATTEMPTS = 3;
const STOP_WAIT_MS = 20000;

/**
 * The 2.x per-service record of the last update.
 *
 * Its shape is fixed by the contract and carries no reason code: a 3.x client
 * reads the operation record for that, and adding a field here would break every
 * 2.x client that still decodes this object strictly.
 */
function recordLastUpdate(service, result, from, to, message) {
  return saveServiceState(service, {
    lastUpdate: { at: new Date().toISOString(), from: from ?? null, to: to ?? null, result, message: message ?? '' },
  });
}

/** Best effort: whatever else went wrong, do not leave the machine without the service. */
async function ensureServiceUp(service) {
  const health = await serviceHealth(service);
  if (health.ok) return true;
  // Except over a command that may still be running: a service started while
  // its installer is still writing is the collision every lock here prevents.
  const unsettled = await unsettledWork();
  if (unsettled) {
    log(`${service.name} was not started again: ${unsettled.message}`, 'update');
    return false;
  }
  await startAndSettle(service);
  const second = await waitForServiceHealth(service, 60000);
  if (!second.ok) log(`WARNING: ${service.name} is not answering after the recovery start`, 'update');
  return second.ok;
}

/**
 * Put a service back that a previous run left stopped.
 *
 * A run killed by its scheduler's time limit (systemd TimeoutStartSec=900, the
 * Windows task ExecutionTimeLimit) can have stopped the service deliberately and
 * then died before starting it again, which no restart policy undoes. This runs
 * before anything else, including the version lookup, so an offline machine
 * still gets its service back.
 */
async function recoverService(service, opId) {
  if (isRunning(service)) return { recovered: false, message: null };
  recordPhase(opId, 'recovering', { note: `${service.id} was not running at the start of the cycle` });
  const recovered = await ensureServiceUp(service);
  const message = recovered
    ? `${service.name} was not running at the start of the cycle and has been started`
    : `${service.name} was not running and could not be started`;
  log(message, 'update');
  appendLog(opId, message);
  return { recovered, message };
}

/**
 * The busy state, with what the process probe already knows folded in.
 *
 * The liveness hint is what lets a state database's "running" row be retired
 * when the thing that would be running it is gone or has restarted since.
 */
async function busyNow(config, service) {
  const liveness = {};
  if (service) liveness[service.id] = livenessOf(service);
  const { busy, entries } = await checkAllBusy(config, { liveness });
  return { busy, entries };
}

function deferred({ service, from = null, to = null, busy, message }) {
  return {
    ok: true,
    action: 'deferred',
    reasonCode: busyReasonCode(busy),
    from,
    to,
    busy,
    message,
    service: service?.id ?? null,
  };
}

// ---------------------------------------------------------------------------
// Restart
// ---------------------------------------------------------------------------

/** The restart itself, with the lock already held and busy already decided. */
async function performRestart(service, { opId }) {
  if (!canRestart(service)) {
    return { ok: false, action: 'failed', reasonCode: REASON.notConfigured, message: `${service.name} has no process to restart` };
  }

  recordPhase(opId, 'restarting');
  const restarted = await restartService(service);
  if (!restarted.ok) {
    await ensureServiceUp(service);
    log(`restart failed: ${restarted.message}`, 'restart');
    return { ok: false, action: 'failed', reasonCode: REASON.notRunning, message: restarted.message };
  }

  recordPhase(opId, 'health');
  const health = await waitForServiceHealth(service, HEALTH_WAIT_MS);
  if (!health.ok) {
    const detail = describeHealth(health);
    log(`restart finished but ${service.name} is not healthy: ${detail}`, 'restart');
    return {
      ok: false,
      action: 'failed',
      reasonCode: REASON.notRunning,
      message: `${service.name} restarted, but it is not answering: ${detail}`,
    };
  }

  log(`restart complete, ${service.name} healthy`, 'restart');
  return { ok: true, action: 'restarted', reasonCode: null, message: `${service.name} restarted and answering` };
}

/**
 * Restart one service, refusing while anything on the machine is busy unless
 * forced, and holding the same machine lock every other mutating verb takes.
 */
export async function runRestart(config, service, options = {}) {
  return withOperation(
    {
      kind: 'restart',
      service: service.id,
      serviceLockFile: service.lockFile,
      systemId: config.system.id,
      ...options,
    },
    async ({ opId, lock }) => {
      ensureServiceStateWritable(service);
      lock.phase('checking-busy');
      recordPhase(opId, 'checking-busy');
      const { busy } = await busyNow(config, service);
      if (busy.busy && !options.force) {
        log(`restart of ${service.id} deferred: ${busy.reason}`, 'restart');
        return deferred({
          service,
          busy,
          message: `the machine is busy (${busy.reason}); pass --force to restart anyway, or --when-idle to queue it`,
        });
      }
      lock.phase('restarting');
      note(`restarting ${service.name}${options.force ? ' (forced)' : ''}`, 'restart');
      return { ...(await performRestart(service, { opId })), service: service.id };
    },
  );
}

// ---------------------------------------------------------------------------
// npm
// ---------------------------------------------------------------------------

/**
 * Everything from the install on, with the service already stopped. Any step
 * that fails throws, and `reached.installing` says whether the install had
 * begun, which is the one thing that decides whether there is anything to roll
 * back.
 */
async function performUpgrade(service, to, prefix, opId, lock, reached) {
  lock.phase('installing');
  recordPhase(opId, 'installing', { note: `npm install ${service.package}@${to}` });
  reached.installing = true;
  const installed = await npmProvider.installVersion(service, to, prefix);
  if (!installed.ok) throw new Error(installed.message);

  // Verify at the prefix we installed into, not by re-running prefix discovery.
  lock.phase('verifying');
  recordPhase(opId, 'verifying');
  const onDisk = npmProvider.installedVersionAt(service, prefix);
  if (onDisk !== to) {
    throw new Error(`npm reported success but the installed version is ${onDisk ?? 'missing'}, not ${to}`);
  }

  lock.phase('starting');
  recordPhase(opId, 'starting');
  const started = await startAndSettle(service);
  if (!started.ok) throw new Error(`could not start ${service.name}: ${started.message}`);

  lock.phase('health');
  recordPhase(opId, 'health');
  const health = await waitForServiceHealth(service, HEALTH_WAIT_MS);
  if (!health.ok) throw new Error(`${service.name} did not become healthy: ${describeHealth(health)}`);
  return { to };
}

/**
 * Nothing is rolled back over a command that may still be running. The runner
 * answers only once a timed-out install has been stopped and seen gone; when it
 * could not see that, the install may still be writing, and reinstalling or
 * starting anything over it would be two writers on one service. The operation
 * then reports its outcome as unknown (operate.mjs), and the ledger keeps every
 * later mutation off the machine until it has gone.
 */
async function rollbackHeld() {
  const unsettled = await unsettledWork();
  return unsettled ? `nothing was rolled back or restarted, because ${unsettled.message}` : null;
}

/**
 * Enter a phase the way every step of a run does, on the lock a second
 * mutation reads and on the record a client polls. A rollback is handed this
 * rather than the run itself, because recovery rolls back for a run that is
 * already over and notes each step on that run's record instead.
 */
function entering(opId, lock) {
  return (phase, progress = null) => {
    lock.phase(phase);
    recordPhase(opId, phase, progress);
  };
}

async function rollBack(service, previous, prefix, enter) {
  const held = await rollbackHeld();
  if (held) return { ok: false, message: held };
  if (!previous) {
    await ensureServiceUp(service);
    return { ok: false, message: 'nothing to roll back to: no previous version was installed' };
  }
  enter('rolling-back', { note: `back to ${previous}` });
  note(`rolling back to ${previous}`, 'update');

  // The same rule as the forward install: npm may not rewrite the package while
  // a process still has those files open. An upgrade that got as far as the
  // start can have left the new version running, and one that will not stop
  // now is left alone rather than reinstalled underneath.
  const stopped = await stopAndWait(service, STOP_WAIT_MS);
  if (!stopped.ok) {
    await ensureServiceUp(service);
    return { ok: false, message: `rollback skipped, ${service.name} would not stop: ${stopped.message}` };
  }

  const reinstalled = await npmProvider.installVersion(service, previous, prefix);
  if (!reinstalled.ok) {
    await ensureServiceUp(service);
    return { ok: false, message: `rollback install failed: ${reinstalled.message}` };
  }
  const started = await startAndSettle(service);
  const health = started.ok ? await waitForServiceHealth(service, HEALTH_WAIT_MS) : { ok: false };
  if (!health.ok) {
    // Whatever else happened, never end a cycle with the service left stopped.
    const recovered = await ensureServiceUp(service);
    return {
      ok: false,
      message: recovered
        ? `rolled back to ${previous}; ${service.name} needed a second start but is healthy now`
        : `rolled back to ${previous} but ${service.name} is not healthy`,
    };
  }
  return { ok: true, message: `rolled back to ${previous}` };
}

/**
 * "Already on the target": the only remaining question is whether it is alive.
 * Shared by the npm and command cycles, which reach it the same way. Runs inside
 * the held lock, so it restarts directly rather than re-entering runRestart.
 */
async function alreadyOnTarget(config, service, from, to, { force, opId }) {
  const health = await serviceHealth(service);
  if (health.ok) {
    if (serviceState(service).pendingRestart) {
      saveServiceState(service, { pendingRestart: false, pendingVersion: null });
    }
    return {
      ok: true,
      action: 'noop',
      reasonCode: REASON.alreadyOnTarget,
      from,
      to,
      message: `already on ${to} and healthy`,
    };
  }

  const { busy } = await busyNow(config, service);
  if (busy.busy && !force) {
    return deferred({
      service,
      from,
      to,
      busy,
      message: `already on ${to} but unhealthy; the machine is busy (${busy.reason}), so the restart was deferred`,
    });
  }

  const restart = await performRestart(service, { opId });
  if (!restart.ok) {
    const message = `already on ${to} but unhealthy; restart failed: ${restart.message}`;
    recordLastUpdate(service, 'failed', from, to, message);
    return { ok: false, action: 'failed', reasonCode: restart.reasonCode, from, to, message };
  }
  const message = `already on ${to}; ${service.name} was unhealthy and has been restarted`;
  recordLastUpdate(service, 'ok', from, to, message);
  return { ok: true, action: 'updated', reasonCode: null, from, to, message };
}

async function runNpmUpdate(config, service, { force, opId, lock }) {
  const prefix = npmProvider.resolveNpmPrefix(service);
  const from = npmProvider.installedVersion(service);

  // Recovery first, before anything that needs the network.
  lock.phase('recovering');
  await recoverService(service, opId);

  lock.phase('resolving');
  recordPhase(opId, 'resolving');
  const channel = service.channel || 'latest';
  const resolved = npmProvider.fetchLatestVersion(service, { timeoutMs: 30000 });
  const to = resolved.version;
  const pattern = npmProvider.versionPattern(service);
  if (!to || (pattern && !pattern.test(to))) {
    // Not a failure of the machine, and NOT something --force may push past: an
    // install of an unknown version is not an update, it is a guess. It is a
    // failed update all the same, never a no-op: nobody can say the service is
    // current, and a client must not show it as done.
    const message = `could not resolve a valid ${channel} version: ${resolved.error ?? 'no version returned'}`;
    log(message, 'update');
    recordLastUpdate(service, 'failed', from, null, message);
    return { ok: false, action: 'failed', reasonCode: REASON.latestUnknown, from, to: null, message };
  }

  // What recovery reads if this run dies part way through: the version it is
  // going from and to, and the prefix it installs into (contract section 5.5).
  updateOperation(opId, { from, to, npmPrefix: prefix });

  if (from === to) return alreadyOnTarget(config, service, from, to, { force, opId });

  lock.phase('checking-busy');
  recordPhase(opId, 'checking-busy');
  const { busy } = await busyNow(config, service);
  if (busy.busy && !force) {
    saveServiceState(service, { pendingRestart: true, pendingVersion: to });
    const message = `the machine is busy (${busy.reason}); ${to} will be installed at the next idle window`;
    log(message, 'update');
    recordLastUpdate(service, 'deferred', from, to, message);
    return deferred({ service, from, to, busy, message });
  }

  lock.phase('warming');
  recordPhase(opId, 'warming', { note: `${service.package}@${to}` });
  const warmed = npmProvider.warmCache(service, to);
  if (!warmed.ok) log(`npm cache add did not succeed after ${warmed.attempts} attempts: ${warmed.message}`, 'update');

  note(`updating ${service.name} ${from ?? 'nothing'} -> ${to}`, 'update');

  lock.phase('stopping');
  recordPhase(opId, 'stopping');
  const drained = await drainService(service);
  if (!drained.ok) {
    log(drained.message, 'update');
    recordLastUpdate(service, 'failed', from, to, drained.message);
    return { ok: false, action: 'failed', reasonCode: REASON.busy, from, to, message: drained.message };
  }
  if (drained.drained) appendLog(opId, drained.message);

  // The second check, and the important one, immediately before the stop. The
  // warm above can legitimately take minutes; anything that started during it,
  // or that the drain left running, would otherwise be destroyed by a decision
  // made before it existed.
  const fresh = await busyNow(config, service);
  if (fresh.busy.busy && !force) {
    saveServiceState(service, { pendingRestart: true, pendingVersion: to });
    const message = `work started while the update was being prepared (${fresh.busy.reason}); ${to} is downloaded and will be installed at the next idle window`;
    log(message, 'update');
    recordLastUpdate(service, 'deferred', from, to, message);
    return deferred({ service, from, to, busy: fresh.busy, message });
  }

  const stopped = await stopAndWait(service, STOP_WAIT_MS);
  if (!stopped.ok) {
    const message = `${service.name} would not stop: ${stopped.message}`;
    log(message, 'update');
    recordLastUpdate(service, 'failed', from, to, message);
    return { ok: false, action: 'failed', reasonCode: REASON.notRunning, from, to, message };
  }

  const reached = { installing: false };
  try {
    await performUpgrade(service, to, prefix, opId, lock, reached);
    saveServiceState(service, { pendingRestart: false, pendingVersion: null });
    const message = `updated ${from ?? 'nothing'} -> ${to}`;
    log(message, 'update');
    recordLastUpdate(service, 'ok', from, to, message);
    return { ok: true, action: 'updated', reasonCode: null, from, to, verified: true, message };
  } catch (err) {
    if (err instanceof ServiceStateError) throw err;
    const failure = err?.message ?? String(err);
    log(`update to ${to} failed: ${failure}`, 'update');
    if (!reached.installing) {
      // Stopped, and nothing on disk has changed: the service only has to come
      // back on the version it was already running.
      const restarted = await ensureServiceUp(service);
      const message = `${failure}; nothing was installed, and ${service.name} ${restarted ? 'was started again' : 'could not be started again'}`;
      recordLastUpdate(service, 'failed', from, to, message);
      return { ok: false, action: 'failed', reasonCode: REASON.internal, from, to, message };
    }
    const rolledBack = await rollBack(service, from, prefix, entering(opId, lock));
    const message = `${failure}; ${rolledBack.message}`;
    if (rolledBack.ok) {
      recordLastUpdate(service, 'rolled-back', from, to, message);
      return { ok: false, action: 'rolled-back', reasonCode: REASON.rolledBack, from, to, message };
    }
    recordLastUpdate(service, 'failed', from, to, message);
    return { ok: false, action: 'failed', reasonCode: REASON.applyFailed, from, to, message };
  }
}

// ---------------------------------------------------------------------------
// A self-updating app
// ---------------------------------------------------------------------------

/**
 * One update cycle for an app that updates itself, which is a completely
 * different shape from the npm one and shares nothing with it but the busy gate.
 *
 * Nothing here downloads, installs or unpacks anything. The app has already done
 * all of that by itself; the staged build is sitting in the updater cache
 * waiting for the app to quit. So the entire cycle is: is something waiting, is
 * it safe to quit, quit, start again, confirm the version actually moved.
 *
 * The confirmation at the end is not ceremony. Quitting is a request, and the
 * updater applying the staged build is a side effect we do not control. If the
 * version did not change we have to say the update failed, because the
 * alternative is reporting success for a restart that achieved nothing.
 */
async function runAppUpdate(config, service, { force, opId, lock }) {
  const state = serviceState(service, { isFirst: config.services[0]?.id === service.id });
  const from = appProvider.installedVersion(service);
  const staged = appProvider.stagedVersion(service);

  // The whole decision is one comparison between two version strings, so we need
  // both. installedVersion() returns null when the bundle's Info.plist cannot be
  // read — PlistBuddy timed out, the bundle is mid swap, the path points
  // somewhere wrong — and null is never equal to a staged version, so letting it
  // fall through would read "an update is waiting" straight out of a failed read
  // and quit the app for nothing.
  if (staged !== null && from === null) {
    const message = `cannot tell whether ${staged} is waiting: the version of ${service.path} could not be read, so ${service.name} was left alone`;
    log(message, 'update');
    return { ok: false, action: 'failed', reasonCode: REASON.notInstalled, from: null, to: staged, message };
  }

  // Nothing waiting. pending/ is NOT cleared once a staged build has applied, so
  // "staged equals installed" is the ordinary steady state right after an update
  // went in, not a sign that anything went wrong.
  if (staged === null || staged === from) {
    const latest = (await appProvider.cachedLatestVersion(service, { timeoutMs: 8000 })).version;
    let message =
      staged === null
        ? `nothing is staged: ${service.name} has not downloaded an update`
        : `nothing is staged: the downloaded build (${staged}) is the one already running`;
    // Worth surfacing: this is the one state that looks like a stall. The app is
    // behind, and the reason is that it has not fetched the new build yet, which
    // is its own business and not something a restart would fix.
    if (latest && latest !== from) message += `; the release feed has ${latest}, which the app has not downloaded yet`;
    if (state.pendingRestart) saveServiceState(service, { pendingRestart: false, pendingVersion: null });
    return { ok: true, action: 'noop', reasonCode: REASON.noUpdate, from, to: null, latest: latest ?? null, message };
  }

  // A build the app will never apply must not be retried forever. Without this a
  // staged version that fails to take makes the fifteen minute timer quit and
  // relaunch the app the user works in, every quarter hour, indefinitely.
  const failures = state.applyFailures;
  if (!force && failures && failures.version === staged && failures.count >= APPLY_ATTEMPTS) {
    const message =
      `${staged} failed to apply ${failures.count} times, so it will not be retried automatically. ` +
      `Quit and reopen ${service.name} yourself to apply it, or pass --force to try again`;
    return {
      ok: false,
      action: 'failed',
      reasonCode: REASON.applyAttemptsExhausted,
      from,
      to: staged,
      attempts: failures.count,
      message,
    };
  }

  // Not running: nothing to do and nothing wrong. The updater applies a staged
  // build when the app quits, so a closed app picks this up by itself the next
  // time it is opened and closed. Opening it from a background timer would be
  // worse than useless. Not applied yet is deferred, not done.
  if (!appProvider.isAppRunning(service)) {
    return {
      ok: true,
      action: 'deferred',
      reasonCode: REASON.appClosed,
      from,
      to: staged,
      message: `${staged} is downloaded, and ${service.name} is closed; it applies by itself the next time you quit the app`,
    };
  }

  lock.phase('checking-busy');
  recordPhase(opId, 'checking-busy');
  const { busy } = await busyNow(config, service);
  if (busy.busy && !force) {
    saveServiceState(service, { pendingRestart: true, pendingVersion: staged });
    const message = `the machine is busy (${busy.reason}); ${staged} is downloaded and will be applied at the next idle window`;
    log(message, 'update');
    recordLastUpdate(service, 'deferred', from, staged, message);
    return deferred({ service, from, to: staged, busy, message });
  }

  lock.phase('stopping');
  recordPhase(opId, 'stopping', { note: 'asking the app to quit so the staged build applies' });
  note(`applying the staged ${service.name} build ${from ?? 'unknown'} -> ${staged}`, 'update');
  const restarted = await appProvider.restartApp(service);
  if (restarted.notRunning) {
    const message = `${service.name} closed while the update was starting; ${staged} applies on that quit by itself`;
    log(message, 'update');
    return { ok: true, action: 'deferred', reasonCode: REASON.appClosed, from, to: staged, message };
  }
  if (!restarted.ok) {
    log(`update to ${staged} failed: ${restarted.message}`, 'update');
    recordLastUpdate(service, 'failed', from, staged, restarted.message);
    return { ok: false, action: 'failed', reasonCode: REASON.applyFailed, from, to: staged, message: restarted.message };
  }

  lock.phase('health');
  recordPhase(opId, 'health');
  const health = await waitForServiceHealth(service, HEALTH_WAIT_MS);
  if (!health.ok) {
    const message = `${service.name} was restarted but is not answering: ${describeHealth(health)}`;
    log(message, 'update');
    recordLastUpdate(service, 'failed', from, staged, message);
    return { ok: false, action: 'failed', reasonCode: REASON.notRunning, from, to: staged, message };
  }

  // Re-read the bundle rather than trusting the restart, but poll for it: the
  // swap happens on quit, and a single read right afterwards can catch it mid
  // flight and report a perfectly good update as failed.
  lock.phase('verifying');
  recordPhase(opId, 'verifying');
  const confirmed = await appProvider.waitForVersion(service, staged);
  if (!confirmed.ok) {
    const count = failures && failures.version === staged ? failures.count + 1 : 1;
    saveServiceState(service, { applyFailures: { version: staged, count, at: new Date().toISOString() } });
    const giveUp = count >= APPLY_ATTEMPTS ? '; it will not be retried automatically from here' : '';
    const message = `the staged update did not apply: ${service.name} restarted and is healthy, but it is still on ${
      confirmed.version ?? 'an unreadable version'
    } rather than ${staged} (attempt ${count})${giveUp}`;
    log(message, 'update');
    recordLastUpdate(service, 'failed', from, staged, message);
    return {
      ok: false,
      action: 'failed',
      reasonCode: REASON.applyFailed,
      from,
      // The version actually read, never the target substituted.
      to: confirmed.version ?? null,
      attempts: count,
      message,
    };
  }

  saveServiceState(service, { pendingRestart: false, pendingVersion: null, applyFailures: null });
  const message = `applied the staged build ${from ?? 'unknown'} -> ${staged}`;
  log(message, 'update');
  recordLastUpdate(service, 'ok', from, staged, message);
  return { ok: true, action: 'updated', reasonCode: null, from, to: staged, verified: true, message };
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/**
 * Did the update command actually do anything?
 *
 * An exit code of zero from a command somebody else wrote proves that the
 * command ran, and nothing else. The first version took whatever version it read
 * afterwards and reported it as the new one, so an updater that did nothing came
 * back as "updated 1.0.0 -> 1.0.0" against a target of 2.0.0. There are now two
 * possible postconditions and at least one has to be available:
 *
 *   version equality with the resolved target, where both can be read; and
 *   `verify`, a command the config supplies that fails when the update did not
 *   take, which is the general answer because plenty of things worth updating do
 *   not report a version at all.
 *
 * With NEITHER available there is no postcondition, and no flag creates one.
 * --force overrides the busy gate; it cannot make an unverifiable thing verified.
 */
async function checkPostcondition(service, { to }) {
  const installed = commandProvider.installedVersion(service);

  // The version this service is ACTUALLY on, or null when it cannot be read.
  // Never the target substituted for it: reporting the version we hoped for is
  // how a no-op updater came back as a completed update.
  if (!service.verify && !service.requireVersionMatch) {
    return {
      ok: false,
      installed,
      verified: false,
      reasonCode: REASON.postconditionFailed,
      message:
        `${service.name} ran its update command and came back healthy, but nothing here can confirm the update took. ` +
        'Add "verify" (a command that fails when it did not) or an installedVersion/latestVersion pair; --force overrides the busy gate, not the evidence',
    };
  }

  if (service.verify) {
    const result = await runMutationArgv(service.verify, { timeoutMs: 120000 });
    if (!result.ok) {
      return {
        ok: false,
        installed,
        verified: false,
        reasonCode: REASON.postconditionFailed,
        message: `the verify command says the update did not take: ${describeFailure(result)}`,
      };
    }
  }

  if (service.requireVersionMatch) {
    // A version match was asked for and there is no target to match against. That
    // is not a comparison that passes by default: it is a comparison that could
    // not be made, and the whole point of the postcondition is to refuse those.
    if (!to) {
      return {
        ok: false,
        installed,
        verified: false,
        reasonCode: REASON.postconditionFailed,
        message:
          `${service.name} is configured to confirm updates by version, but the target version could not be resolved, ` +
          'so there is nothing to compare the installed version against',
      };
    }
    if (installed === null) {
      return {
        ok: false,
        installed: null,
        verified: false,
        reasonCode: REASON.postconditionFailed,
        message: `the update command succeeded but the installed version could not be read, so there is no evidence ${to} was installed`,
      };
    }
    if (installed !== to) {
      return {
        ok: false,
        installed,
        verified: false,
        reasonCode: REASON.postconditionFailed,
        message: `the update command succeeded but ${service.name} is still on ${installed}, not ${to}`,
      };
    }
  }

  // A verify hook can pass while the version stays unreadable. That is a genuine
  // success — the service itself said the update took — but the version reported
  // is still the one actually read, which may be null.
  return { ok: true, installed, verified: true, reasonCode: null, message: null };
}

/**
 * Put a command-updated service back. There is no previous artefact to restore,
 * so this is only ever the rollback hook the config supplies — and saying "there
 * is no rollback" out loud is better than implying the machine is fine.
 */
async function rollBackCommand(service, enter) {
  // Whatever happens here, the version reported afterwards is read fresh from
  // the service. A rollback changes what is installed, so echoing the reading
  // taken before it would name a version that is no longer there.
  const finalVersion = () => commandProvider.installedVersion(service);

  const held = await rollbackHeld();
  if (held) return { ok: false, installed: finalVersion(), message: held };
  if (!service.rollback) {
    const recovered = await ensureServiceUp(service);
    return {
      ok: false,
      installed: finalVersion(),
      message: recovered
        ? 'no rollback command is configured, so the partial update was left in place and the service was restarted on it'
        : 'no rollback command is configured, and the service could not be restarted; this machine needs attention',
    };
  }
  enter('rolling-back');
  const result = await runMutationArgv(service.rollback, { timeoutMs: 600000 });
  if (!result.ok) {
    await ensureServiceUp(service);
    return { ok: false, installed: finalVersion(), message: `the rollback command failed: ${describeFailure(result)}` };
  }
  const started = await startAndSettle(service);
  const health = started.ok ? await waitForServiceHealth(service, HEALTH_WAIT_MS) : { ok: false };
  if (!health.ok) {
    const recovered = await ensureServiceUp(service);
    return {
      ok: recovered,
      installed: finalVersion(),
      message: recovered
        ? 'the rollback command ran and the service needed a second start, but it is healthy now'
        : 'the rollback command ran but the service is not healthy',
    };
  }
  const installed = finalVersion();
  return {
    ok: true,
    installed,
    message: `the rollback command ran and the service is healthy on ${installed ?? 'a version it does not report'}`,
  };
}

async function runCommandUpdate(config, service, { force, opId, lock }) {
  const from = commandProvider.installedVersion(service);
  if (!service.update) {
    return {
      ok: true,
      action: 'noop',
      reasonCode: REASON.noUpdate,
      from,
      to: null,
      message: `${service.name} has no update command configured`,
    };
  }

  lock.phase('recovering');
  await recoverService(service, opId);

  lock.phase('resolving');
  recordPhase(opId, 'resolving');
  const resolved = commandProvider.fetchLatestVersion(service, { timeoutMs: 30000 });
  const to = resolved.version;

  if (!to && service.latestVersion) {
    // A latestVersion command was configured and could not answer. Running the
    // update anyway would be installing something nobody can name, and --force
    // does not buy past that: it overrides the busy gate and nothing else.
    const message = `could not resolve the newest version: ${resolved.error ?? 'no version returned'}`;
    log(message, 'update');
    recordLastUpdate(service, 'failed', from, null, message);
    return { ok: false, action: 'failed', reasonCode: REASON.latestUnknown, from, to: null, message };
  }

  if (!to && !service.latestVersion && !service.verify) {
    // Nothing to compare against and nothing to verify with: there is no way
    // this update could ever be reported honestly, so it is refused before it
    // touches the service rather than after, and it is a failed update, not a
    // no-op a client would show as done.
    const message =
      `${service.name} has neither a latestVersion command nor a verify command, so an update could not be confirmed. ` +
      'Configure one of them; --force overrides the busy gate, not the evidence';
    return { ok: false, action: 'failed', reasonCode: REASON.postconditionFailed, from, to: null, message };
  }

  // What recovery reads if this run dies part way through (contract section 5.5).
  updateOperation(opId, { from, to });

  if (to && from === to) return alreadyOnTarget(config, service, from, to, { force, opId });

  lock.phase('checking-busy');
  recordPhase(opId, 'checking-busy');
  const { busy } = await busyNow(config, service);
  if (busy.busy && !force) {
    saveServiceState(service, { pendingRestart: true, pendingVersion: to });
    const message = `the machine is busy (${busy.reason}); ${to ?? 'the update'} will be applied at the next idle window`;
    log(message, 'update');
    recordLastUpdate(service, 'deferred', from, to, message);
    return deferred({ service, from, to, busy, message });
  }

  note(`updating ${service.name} ${from ?? 'nothing'} -> ${to ?? 'whatever the update command installs'}`, 'update');

  lock.phase('stopping');
  recordPhase(opId, 'stopping');
  const drained = await drainService(service);
  if (!drained.ok) {
    log(drained.message, 'update');
    recordLastUpdate(service, 'failed', from, to, drained.message);
    return { ok: false, action: 'failed', reasonCode: REASON.busy, from, to, message: drained.message };
  }

  // One last look, immediately before the stop.
  const fresh = await busyNow(config, service);
  if (fresh.busy.busy && !force) {
    saveServiceState(service, { pendingRestart: true, pendingVersion: to });
    const message = `work started while the update was being prepared (${fresh.busy.reason}); it will be applied at the next idle window`;
    log(message, 'update');
    recordLastUpdate(service, 'deferred', from, to, message);
    return deferred({ service, from, to, busy: fresh.busy, message });
  }

  const stopped = await stopAndWait(service, STOP_WAIT_MS);
  if (!stopped.ok) {
    const message = `${service.name} would not stop: ${stopped.message}`;
    log(message, 'update');
    recordLastUpdate(service, 'failed', from, to, message);
    return { ok: false, action: 'failed', reasonCode: REASON.notRunning, from, to, message };
  }

  lock.phase('installing');
  recordPhase(opId, 'installing');
  const updated = await commandProvider.update(service);
  if (!updated.ok) {
    const rolled = await rollBackCommand(service, entering(opId, lock));
    const message = `the update command failed: ${updated.message}; ${rolled.message}`;
    log(message, 'update');
    recordLastUpdate(service, rolled.ok ? 'rolled-back' : 'failed', from, to, message);
    return {
      ok: false,
      action: rolled.ok ? 'rolled-back' : 'failed',
      reasonCode: rolled.ok ? REASON.rolledBack : REASON.applyFailed,
      from,
      to: rolled.installed ?? null,
      message,
    };
  }

  lock.phase('starting');
  recordPhase(opId, 'starting');
  const started = await startAndSettle(service);
  if (!started.ok) {
    const rolled = await rollBackCommand(service, entering(opId, lock));
    const message = `${service.name} was updated but would not start: ${started.message}; ${rolled.message}`;
    log(message, 'update');
    recordLastUpdate(service, rolled.ok ? 'rolled-back' : 'failed', from, to, message);
    return {
      ok: false,
      action: rolled.ok ? 'rolled-back' : 'failed',
      reasonCode: rolled.ok ? REASON.rolledBack : REASON.notRunning,
      from,
      to: rolled.installed ?? null,
      message,
    };
  }

  lock.phase('health');
  recordPhase(opId, 'health');
  const health = await waitForServiceHealth(service, HEALTH_WAIT_MS);
  if (!health.ok) {
    const rolled = await rollBackCommand(service, entering(opId, lock));
    const message = `${service.name} was updated but is not answering: ${describeHealth(health)}; ${rolled.message}`;
    log(message, 'update');
    recordLastUpdate(service, rolled.ok ? 'rolled-back' : 'failed', from, to, message);
    return {
      ok: false,
      action: rolled.ok ? 'rolled-back' : 'failed',
      reasonCode: rolled.ok ? REASON.rolledBack : REASON.notRunning,
      from,
      to: rolled.installed ?? null,
      message,
    };
  }

  lock.phase('verifying');
  recordPhase(opId, 'verifying');
  const postcondition = await checkPostcondition(service, { to });
  if (!postcondition.ok) {
    const rolled = await rollBackCommand(service, entering(opId, lock));
    const message = `${postcondition.message}; ${rolled.message}`;
    log(message, 'update');
    recordLastUpdate(service, rolled.ok ? 'rolled-back' : 'failed', from, to, message);
    return {
      ok: false,
      action: rolled.ok ? 'rolled-back' : 'failed',
      reasonCode: rolled.ok ? REASON.rolledBack : postcondition.reasonCode,
      from,
      // The version actually installed after the rollback, read fresh. Never the
      // target, and never the reading taken before the rollback changed it.
      to: rolled.installed ?? null,
      verified: false,
      message,
    };
  }

  // The version READ, never the target substituted for it. A service whose
  // verify hook passed but which reports no version at all is honestly reported
  // as being on an unknown version rather than on the one we asked for.
  const now = postcondition.installed;
  saveServiceState(service, { pendingRestart: false, pendingVersion: null });
  const message = `updated ${from ?? 'nothing'} -> ${now ?? 'a version it does not report'}`;
  log(message, 'update');
  recordLastUpdate(service, 'ok', from, now, message);
  return {
    ok: true,
    action: 'updated',
    reasonCode: null,
    from,
    to: now,
    verified: true,
    message,
  };
}

// ---------------------------------------------------------------------------
// Recovery
// ---------------------------------------------------------------------------

/**
 * Whether an update of the same service asked for after this one has since
 * finished and settled its version: installed and verified it, rolled back, or
 * found it already on its target. What is installed is then that run's doing,
 * and rolling it back to the version an older run started from would undo it.
 */
function versionSettledSince(record) {
  return listOperations({ service: record.service, kind: 'update', state: 'finished', limit: 200 }).some(
    (later) =>
      later.id !== record.id &&
      String(later.requestedAt ?? '') > String(record.requestedAt ?? '') &&
      (['updated', 'rolled-back'].includes(later.result?.action) || later.result?.reasonCode === REASON.alreadyOnTarget),
  );
}

/**
 * The rollback an interrupted update still owes, or null when it owes none
 * (contract section 5.5): only when the version installed now is neither the
 * one it started from nor the one it was going to. A run that noted no `from`,
 * because nothing was installed before it or because an agent before 3.1.0
 * wrote its record, has nothing known to go back to, and an app keeps no
 * previous build to restore. A version that cannot be read now is neither: a
 * half-written install is exactly what reads that way.
 */
export function rollbackOwed(service, record) {
  if (record.kind !== 'update' || (service.kind !== 'npm' && service.kind !== 'command')) return null;
  const from = record.from ?? null;
  const to = record.to ?? null;
  if (from === null) return null;
  const installed =
    service.kind === 'npm'
      ? npmProvider.installedVersionAt(service, record.npmPrefix ?? npmProvider.resolveNpmPrefix(service))
      : commandProvider.installedVersion(service);
  if (installed === from || (to !== null && installed === to)) return null;
  if (versionSettledSince(record)) return null;
  return { from, to, installed };
}

/**
 * Roll a service back for an update that was interrupted half way, from the
 * cycle that found it. The rollback is the update's own, the npm reinstall or
 * the configured rollback command, with each step noted on the interrupted
 * record rather than entered as a phase of the cycle.
 *
 * A service that is running again is stopped for it only through the gate the
 * update's own stop goes through: a busy check, the drain, and a second check
 * immediately before anything stops. A unit that came back by itself on a
 * half-written tree takes new work like any other, and checking is not
 * preventing. A drain it refuses leaves it running, and the rollback owed.
 *
 * Returns { settled, action, reasonCode, message }. The obligation is met once
 * the rollback took, or once the service is seen running even though it did
 * not: putting the version right is then an update's business, and stopping a
 * running service on every cycle to try again would be damage of its own.
 */
export async function rollBackInterrupted(config, service, record, owed, { running }) {
  const found = `${service.name} is on ${owed.installed ?? 'a version that cannot be read'}, neither ${owed.from} nor ${owed.to ?? 'the version it was going to'}`;
  const held = await rollbackHeld();
  if (held) return { settled: false, action: 'conflict', reasonCode: REASON.operationInProgress, message: `${found}, and ${held}` };
  const waiting = (busy, why = '') => ({
    settled: false,
    action: 'deferred',
    reasonCode: busyReasonCode(busy),
    message: `${found}, and the rollback to ${owed.from} waits for an idle machine (${why}${busy.reason})`,
  });
  if (running && (service.kind === 'npm' || service.rollback)) {
    const { busy } = await busyNow(config, service);
    if (busy.busy) return waiting(busy);
    const drained = await drainService(service);
    if (!drained.ok) {
      return { settled: false, action: 'drain-refused', reasonCode: REASON.busy, message: `${found}, and ${service.name} was not rolled back to ${owed.from}: ${drained.message}` };
    }
    if (drained.drained) appendLog(record.id, `recovery: ${drained.message}`);
    const fresh = await busyNow(config, service);
    if (fresh.busy.busy) return waiting(fresh.busy, 'work started while it was being prepared: ');
  }
  note(`${found} after an interrupted update; rolling it back`, 'recover');
  const enter = (phase, progress = null) => appendLog(record.id, `recovery: ${phase}${progress?.note ? `, ${progress.note}` : ''}`);
  const rolled =
    service.kind === 'npm'
      ? await rollBack(service, owed.from, record.npmPrefix ?? npmProvider.resolveNpmPrefix(service), enter)
      : await rollBackCommand(service, enter);
  const message = `${found}; ${rolled.message}`;
  if (rolled.ok) return { settled: true, action: 'rolled-back', reasonCode: null, message };
  return { settled: processEvidence(service).state === 'running', action: 'rollback-failed', reasonCode: REASON.applyFailed, message };
}

// ---------------------------------------------------------------------------
// The entry point
// ---------------------------------------------------------------------------

/** The installed version, without any discovery that would slow a refusal down. */
export function installedFor(service) {
  if (service.kind === 'app') return appProvider.installedVersion(service);
  if (service.kind === 'npm') return npmProvider.installedVersion(service, { allowProbe: false });
  return commandProvider.installedVersion(service);
}

/**
 * One update cycle for one service. Always returns an object; never throws.
 *
 * `mode` is the separation the first version did not have:
 *
 *   "scheduled" is the timer asking. Policy decides, and a declined cycle
 *   reports which policy declined it.
 *   "manual" is a person asking. Policy is not consulted at all — the busy gate
 *   still is, and --force is the only thing that overrides it.
 */
export async function runUpdate(config, service, options = {}) {
  const { mode = 'manual', force = false } = options;
  let refusal = null;

  if (mode === 'scheduled') {
    const eligibility = scheduledEligibility(config, service, {});
    if (!eligibility.eligible) {
      refusal = {
        ok: true,
        action: 'deferred',
        reasonCode: eligibility.reasonCode,
        service: service.id,
        from: installedFor(service),
        to: null,
        message: eligibility.reason,
        opId: null,
      };
    }
  } else if (!hasUpdatePath(service)) {
    refusal = {
      ok: true,
      action: 'noop',
      reasonCode: REASON.noUpdate,
      service: service.id,
      from: installedFor(service),
      to: null,
      message: `${service.name} has no update command configured, so there is nothing to run`,
      opId: null,
    };
  }

  if (refusal && !options.existingOpId && !options.opId) return refusal;

  const result = await withOperation(
    {
      kind: 'update',
      service: service.id,
      serviceLockFile: service.lockFile,
      systemId: config.system.id,
      ...options,
    },
    async ({ opId, lock }) => {
      if (refusal) return refusal;
      let stateReady = false;
      try {
        ensureServiceStateWritable(service);
        stateReady = true;
        if (service.kind === 'app') return await runAppUpdate(config, service, { force, opId, lock });
        if (service.kind === 'npm') return await runNpmUpdate(config, service, { force, opId, lock });
        return await runCommandUpdate(config, service, { force, opId, lock });
      } catch (error) {
        if (!(error instanceof ServiceStateError)) throw error;
        // Once the state was writable the update may have run, so a failure to
        // record it is no evidence that it did not: the outcome is unknown.
        return {
          ok: false, action: 'failed', reasonCode: REASON.internal, to: installedFor(service),
          message: `${error.message}; ${stateReady ? 'work may have changed the service; its state could not be saved, so completion is not confirmed' : 'no service work was started'}`,
          ...(stateReady ? { outcome: 'unknown' } : {}),
        };
      }
    },
  );
  return { ...result, service: service.id };
}
