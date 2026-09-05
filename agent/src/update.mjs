// The update cycle, and the restart that shares its gate.
//
// The ordering for a service we install ourselves is the whole point, so it is
// spelled out:
//
//   open the record -> take the machine lock -> RECOVER anything a previous run
//   left half done -> resolve the target version -> compare -> check busy ->
//   pre-warm the cache -> CHECK BUSY AGAIN -> drain -> stop -> install ->
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
//   window used to be thrown away by a decision made nine minutes earlier.
//
//   A drain runs before the stop where the service has one. Checking is not
//   preventing: only the service itself can refuse new work, and where it can,
//   we ask it to.
//
//   An update is only reported as done when something PROVES it. A command that
//   exits zero has proved that it ran, and nothing else.
//
// We NEVER install while the machine is busy without being told to. An installer
// replaces files underneath a running service, and a service that loads code
// lazily starts failing on files that changed under it. Deferring costs a delay;
// installing under a live turn costs the user's work.

import { describeFailure, runArgv } from './config.mjs';
import { REASON } from './contract.mjs';
import { withOperation } from './operate.mjs';
import { appendLog, recordPhase } from './operations.mjs';
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

async function performUpgrade(service, to, prefix, opId, lock) {
  lock.phase('stopping');
  recordPhase(opId, 'stopping');
  const drained = drainService(service);
  if (!drained.ok) throw new Error(drained.message);
  if (drained.drained) appendLog(opId, drained.message);

  const stopped = await stopAndWait(service, STOP_WAIT_MS);
  if (!stopped.ok) throw new Error(stopped.message);

  lock.phase('installing');
  recordPhase(opId, 'installing', { note: `npm install ${service.package}@${to}` });
  const installed = npmProvider.installVersion(service, to, prefix);
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

async function rollBack(service, previous, prefix, opId, lock) {
  if (!previous) {
    await ensureServiceUp(service);
    return { ok: false, message: 'nothing to roll back to: no previous version was installed' };
  }
  lock.phase('rolling-back');
  recordPhase(opId, 'rolling-back', { note: `back to ${previous}` });
  note(`rolling back to ${previous}`, 'update');

  // The same rule as the forward install: npm may not rewrite the package while
  // a process still has those files open. If the upgrade failed at the stop, the
  // old service is still up on the version we would reinstall, so the honest
  // move is to leave it alone rather than corrupt a working install.
  const stopped = await stopAndWait(service, STOP_WAIT_MS);
  if (!stopped.ok) {
    await ensureServiceUp(service);
    return { ok: false, message: `rollback skipped, ${service.name} would not stop: ${stopped.message}` };
  }

  const reinstalled = npmProvider.installVersion(service, previous, prefix);
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
    // install of an unknown version is not an update, it is a guess.
    const message = `could not resolve a valid ${channel} version: ${resolved.error ?? 'no version returned'}`;
    log(message, 'update');
    recordLastUpdate(service, 'failed', from, null, message);
    return { ok: true, action: 'noop', reasonCode: REASON.latestUnknown, from, to: null, message };
  }

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

  // The second check, and the important one. The warm above can legitimately
  // take minutes; anything that started during it would otherwise be destroyed
  // by a decision made before it existed.
  lock.phase('checking-busy');
  const fresh = await busyNow(config, service);
  if (fresh.busy.busy && !force) {
    saveServiceState(service, { pendingRestart: true, pendingVersion: to });
    const message = `work started while the download was being prepared (${fresh.busy.reason}); ${to} is downloaded and will be installed at the next idle window`;
    log(message, 'update');
    recordLastUpdate(service, 'deferred', from, to, message);
    return deferred({ service, from, to, busy: fresh.busy, message });
  }

  try {
    note(`updating ${service.name} ${from ?? 'nothing'} -> ${to}`, 'update');
    await performUpgrade(service, to, prefix, opId, lock);
    saveServiceState(service, { pendingRestart: false, pendingVersion: null });
    const message = `updated ${from ?? 'nothing'} -> ${to}`;
    log(message, 'update');
    recordLastUpdate(service, 'ok', from, to, message);
    return { ok: true, action: 'updated', reasonCode: null, from, to, verified: true, message };
  } catch (err) {
    if (err instanceof ServiceStateError) throw err;
    const failure = err?.message ?? String(err);
    log(`update to ${to} failed: ${failure}`, 'update');
    const rolledBack = await rollBack(service, from, prefix, opId, lock);
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
      ok: true,
      action: 'noop',
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
  // worse than useless.
  if (!appProvider.isAppRunning(service)) {
    return {
      ok: true,
      action: 'noop',
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
    return { ok: true, action: 'noop', reasonCode: REASON.appClosed, from, to: staged, message };
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
function checkPostcondition(service, { to }) {
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
    const result = runArgv(service.verify, { timeoutMs: 120000 });
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
async function rollBackCommand(service, opId, lock) {
  // Whatever happens here, the version reported afterwards is read fresh from
  // the service. A rollback changes what is installed, so echoing the reading
  // taken before it would name a version that is no longer there.
  const finalVersion = () => commandProvider.installedVersion(service);

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
  lock.phase('rolling-back');
  recordPhase(opId, 'rolling-back');
  const result = runArgv(service.rollback, { timeoutMs: 600000 });
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
    return { ok: true, action: 'noop', reasonCode: REASON.latestUnknown, from, to: null, message };
  }

  if (!to && !service.latestVersion && !service.verify) {
    // Nothing to compare against and nothing to verify with: there is no way
    // this update could ever be reported honestly, so it is refused before it
    // touches the service rather than after.
    const message =
      `${service.name} has neither a latestVersion command nor a verify command, so an update could not be confirmed. ` +
      'Configure one of them; --force overrides the busy gate, not the evidence';
    return { ok: true, action: 'noop', reasonCode: REASON.postconditionFailed, from, to: null, message };
  }

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
  const drained = drainService(service);
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
  const updated = commandProvider.update(service);
  if (!updated.ok) {
    const rolled = await rollBackCommand(service, opId, lock);
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
    const rolled = await rollBackCommand(service, opId, lock);
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
    const rolled = await rollBackCommand(service, opId, lock);
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
  const postcondition = checkPostcondition(service, { to });
  if (!postcondition.ok) {
    const rolled = await rollBackCommand(service, opId, lock);
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
        return {
          ok: false, action: 'failed', reasonCode: REASON.internal, to: installedFor(service),
          message: `${error.message}; ${stateReady ? 'work may have changed the service; its state could not be saved, so completion is not confirmed' : 'no service work was started'}`,
        };
      }
    },
  );
  return { ...result, service: service.id };
}
