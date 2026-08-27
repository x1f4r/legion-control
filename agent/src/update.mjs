// The update cycle, and the restart that shares its busy gate.
//
// The ordering for a service we install ourselves is the whole point, so it is
// spelled out:
//   resolve the target version -> compare -> check busy -> pre-warm the cache ->
//   take the maintenance lock -> stop -> install -> verify -> start -> health.
//
// We NEVER install while the machine is busy. An installer replaces files
// underneath a running service, and a service that loads code lazily will start
// failing on files that changed under it. Deferring costs a delay; installing
// under a live turn costs the user's work.

import fs from 'node:fs';
import path from 'node:path';
import { loadState, saveServiceState, serviceState } from './config.mjs';
import { checkAllBusy } from './probes/busy.mjs';
import { describeHealth } from './probes/health.mjs';
import { log, note } from './log.mjs';
import * as appProvider from './providers/app.mjs';
import * as commandProvider from './providers/command.mjs';
import * as npmProvider from './providers/npm.mjs';
import {
  canRestart,
  isRunning,
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
// A hard ceiling on how long any lock may be honoured, so a recycled pid can
// never wedge the updater permanently. The real staleness test is whether the
// process that wrote the lock is still alive: the schedulers kill a run well
// before this (systemd TimeoutStartSec=900, the Windows task ExecutionTimeLimit
// PT15M), so a dead owner means the run was killed mid-flight and its lock must
// be taken over. Judging staleness by age alone cannot work here, because a
// worst-case cycle can legitimately run longer than the 15 minute cadence.
const LOCK_HARD_CEILING_MS = 60 * 60 * 1000;
const LEGACY_LOCK_STALE_MS = 20 * 60 * 1000;

/** True when the pid is still running. EPERM means it exists but is not ours. */
function processIsAlive(pid) {
  if (!Number.isInteger(pid) || pid <= 0) return false;
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    return err.code === 'EPERM';
  }
}

/** Parse "pid=1234" out of a lock file body, or null when it is not there. */
function lockOwnerPid(body) {
  const match = /^pid=(\d+)$/m.exec(body ?? '');
  return match ? Number.parseInt(match[1], 10) : null;
}

function recordLastUpdate(service, result, from, to, message) {
  const lastUpdate = {
    at: new Date().toISOString(),
    from: from ?? null,
    to: to ?? null,
    result,
    message: message ?? '',
  };
  saveServiceState(service, { lastUpdate });
  return lastUpdate;
}

function acquireLock(service, target) {
  const file = service.lockFile;
  try {
    fs.mkdirSync(path.dirname(file), { recursive: true });
  } catch (err) {
    return { ok: false, held: false, message: `cannot create ${path.dirname(file)}: ${err.message}` };
  }

  for (let attempt = 0; attempt < 2; attempt += 1) {
    try {
      const fd = fs.openSync(file, 'wx');
      try {
        // The target version stays on the first line: anything else on the
        // machine that honours this file only cares that it exists, but a human
        // reading it wants to see what is being installed.
        fs.writeFileSync(fd, `${target}\npid=${process.pid}\nstarted=${new Date().toISOString()}\n`, 'utf8');
      } finally {
        fs.closeSync(fd);
      }
      return { ok: true, held: true, file, message: null };
    } catch (err) {
      if (err.code !== 'EEXIST') return { ok: false, held: false, message: `cannot write ${file}: ${err.message}` };
      let ageMs = Infinity;
      let body = '';
      try {
        ageMs = Date.now() - fs.statSync(file).mtimeMs;
        body = fs.readFileSync(file, 'utf8');
      } catch {
        /* treat an unreadable lock as stale */
      }
      const ownerPid = lockOwnerPid(body);
      const ownerAlive = ownerPid !== null && processIsAlive(ownerPid);
      if (ownerAlive && ageMs < LOCK_HARD_CEILING_MS) {
        return { ok: false, held: false, message: `another update (pid ${ownerPid}) holds ${file}` };
      }
      // A lock with no pid line was written by something that predates this
      // agent, so fall back to age alone, using the same 20 minutes a watchdog
      // honouring the same file would apply.
      if (ownerPid === null && ageMs < LEGACY_LOCK_STALE_MS) {
        return { ok: false, held: false, message: `another update holds ${file}` };
      }
      log(
        `removing a stale maintenance lock (${Math.round(ageMs / 1000)} s old, owner ${
          ownerPid === null ? 'unknown' : `pid ${ownerPid} is gone`
        })`,
        'update',
      );
      try {
        fs.rmSync(file, { force: true });
      } catch (removeError) {
        return { ok: false, held: false, message: `cannot remove the stale lock: ${removeError.message}` };
      }
    }
  }
  return { ok: false, held: false, message: `could not take ${file}` };
}

function releaseLock(lock) {
  if (!lock?.held) return;
  try {
    fs.rmSync(lock.file, { force: true });
  } catch (err) {
    log(`WARNING: could not remove the maintenance lock ${lock.file}: ${err.message}`, 'update');
  }
}

/** Best effort: whatever else went wrong, do not leave the machine without the service. */
async function ensureServiceUp(service) {
  const health = await serviceHealth(service, { timeoutMs: 5000 });
  if (health.ok) return true;
  await startAndSettle(service);
  const second = await waitForServiceHealth(service, 60000);
  if (!second.ok) log(`WARNING: ${service.name} is not answering after the recovery start`, 'update');
  return second.ok;
}

/** The state kept for one service, with the first service falling back to the old top-level keys. */
function stateFor(config, service) {
  return serviceState(loadState(), service, config.services[0]?.id === service.id);
}

/** Restart one service, refusing while anything on the machine is busy unless forced. */
export async function runRestart(config, service, { force = false } = {}) {
  const { busy } = await checkAllBusy(config);
  if (busy.busy && !force) {
    log(`restart of ${service.id} deferred: ${busy.reason}`, 'restart');
    return {
      ok: true,
      action: 'deferred',
      message: `the machine is busy (${busy.reason}); pass --force to restart anyway`,
      busy,
    };
  }
  if (!canRestart(service)) {
    return { ok: false, action: 'failed', message: `${service.name} has no process to restart`, busy };
  }

  note(`restarting ${service.name}${force ? ' (forced)' : ''}`, 'restart');
  const restarted = await restartService(service);
  if (!restarted.ok) {
    await ensureServiceUp(service);
    log(`restart failed: ${restarted.message}`, 'restart');
    return { ok: false, action: 'failed', message: restarted.message, busy };
  }

  const health = await waitForServiceHealth(service, HEALTH_WAIT_MS);
  if (!health.ok) {
    const detail = describeHealth(health);
    log(`restart finished but ${service.name} is not healthy: ${detail}`, 'restart');
    return {
      ok: false,
      action: 'failed',
      message: `${service.name} restarted, but it is not answering: ${detail}`,
      busy,
    };
  }

  log(`restart complete, ${service.name} healthy`, 'restart');
  return { ok: true, action: 'restarted', message: `${service.name} restarted and answering`, busy };
}

// ---------------------------------------------------------------------------
// npm
// ---------------------------------------------------------------------------

async function performUpgrade(service, to, prefix) {
  const stopped = await stopAndWait(service, STOP_WAIT_MS);
  if (!stopped.ok) throw new Error(stopped.message);

  const installed = npmProvider.installVersion(service, to, prefix);
  if (!installed.ok) throw new Error(installed.message);

  // Verify at the prefix we installed into, not by re-running prefix discovery.
  const onDisk = npmProvider.installedVersionAt(service, prefix);
  if (onDisk !== to) {
    throw new Error(`npm reported success but the installed version is ${onDisk ?? 'missing'}, not ${to}`);
  }

  const started = await startAndSettle(service);
  if (!started.ok) throw new Error(`could not start ${service.name}: ${started.message}`);

  const health = await waitForServiceHealth(service, HEALTH_WAIT_MS);
  if (!health.ok) throw new Error(`${service.name} did not become healthy: ${describeHealth(health)}`);
  return { to };
}

async function rollBack(service, previous, prefix) {
  if (!previous) {
    await ensureServiceUp(service);
    return { ok: false, message: 'nothing to roll back to: no previous version was installed' };
  }
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
 * Shared by the npm and command cycles, which reach it the same way.
 */
async function alreadyOnTarget(config, service, from, to, force) {
  const health = await serviceHealth(service, { timeoutMs: 5000 });
  if (health.ok) {
    if (stateFor(config, service).pendingRestart) {
      saveServiceState(service, { pendingRestart: false, pendingVersion: null });
    }
    return { ok: true, action: 'noop', from, to, message: `already on ${to} and healthy` };
  }
  const restart = await runRestart(config, service, { force });
  if (restart.action === 'deferred') {
    return { ok: true, action: 'deferred', from, to, message: `already on ${to} but unhealthy; ${restart.message}` };
  }
  if (!restart.ok) {
    const message = `already on ${to} but unhealthy; restart failed: ${restart.message}`;
    recordLastUpdate(service, 'failed', from, to, message);
    return { ok: false, action: 'failed', from, to, message };
  }
  const message = `already on ${to}; ${service.name} was unhealthy and has been restarted`;
  recordLastUpdate(service, 'ok', from, to, message);
  return { ok: true, action: 'updated', from, to, message };
}

async function runNpmUpdate(config, service, { force }) {
  const prefix = npmProvider.resolveNpmPrefix(service);
  const from = npmProvider.installedVersion(service);

  const channel = service.channel || 'latest';
  const resolved = npmProvider.fetchLatestVersion(service, { timeoutMs: 30000 });
  const to = resolved.version;
  const pattern = npmProvider.versionPattern(service);
  if (!to || (pattern && !pattern.test(to))) {
    const message = `could not resolve a valid ${channel} version: ${resolved.error ?? 'no version returned'}`;
    log(message, 'update');
    recordLastUpdate(service, 'failed', from, null, message);
    return { ok: false, action: 'failed', from, to: null, message };
  }

  if (from === to) return alreadyOnTarget(config, service, from, to, force);

  // If a previous run was killed mid-install (systemd TimeoutStartSec, the
  // Windows task time limit) it can have left the service deliberately stopped,
  // which a restart policy does not undo. Put it back before deciding anything
  // else: starting a stopped service is never destructive, and without this the
  // stale rows that same kill left behind would read as busy and defer the
  // update for hours with the service still down.
  if (!isRunning(service)) {
    const recovered = await ensureServiceUp(service);
    log(
      recovered
        ? `${service.name} was not running at the start of the cycle and has been started`
        : `${service.name} was not running and could not be started`,
      'update',
    );
  }

  // Busy check comes BEFORE anything that touches the installed files.
  const { busy } = await checkAllBusy(config);
  if (busy.busy && !force) {
    saveServiceState(service, { pendingRestart: true, pendingVersion: to });
    const message = `the machine is busy (${busy.reason}); ${to} will be installed at the next idle window`;
    log(message, 'update');
    recordLastUpdate(service, 'deferred', from, to, message);
    return { ok: true, action: 'deferred', from, to, message, busy };
  }

  const warmed = npmProvider.warmCache(service, to);
  if (!warmed.ok) log(`npm cache add did not succeed after ${warmed.attempts} attempts: ${warmed.message}`, 'update');

  const lock = acquireLock(service, to);
  if (!lock.ok) {
    const message = `maintenance lock unavailable: ${lock.message}`;
    log(message, 'update');
    return { ok: true, action: 'noop', from, to, message };
  }

  try {
    note(`updating ${service.name} ${from ?? 'nothing'} -> ${to}`, 'update');
    await performUpgrade(service, to, prefix);
    saveServiceState(service, { pendingRestart: false, pendingVersion: null });
    const message = `updated ${from ?? 'nothing'} -> ${to}`;
    log(message, 'update');
    recordLastUpdate(service, 'ok', from, to, message);
    return { ok: true, action: 'updated', from, to, message };
  } catch (err) {
    const failure = err?.message ?? String(err);
    log(`update to ${to} failed: ${failure}`, 'update');
    const rolledBack = await rollBack(service, from, prefix);
    const message = `${failure}; ${rolledBack.message}`;
    if (rolledBack.ok) {
      recordLastUpdate(service, 'rolled-back', from, to, message);
      return { ok: false, action: 'rolled-back', from, to, message };
    }
    recordLastUpdate(service, 'failed', from, to, message);
    return { ok: false, action: 'failed', from, to, message };
  } finally {
    releaseLock(lock);
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
async function runAppUpdate(config, service, { force }) {
  const state = stateFor(config, service);
  const from = appProvider.installedVersion(service);
  const staged = appProvider.stagedVersion(service);

  // The whole decision is one comparison between two version strings, so we need
  // both of them. installedVersion() returns null when the bundle's Info.plist
  // cannot be read — PlistBuddy timed out, the bundle is mid swap, the path
  // points somewhere wrong — and null is never equal to a staged version, so
  // letting it fall through would read "an update is waiting" straight out of a
  // failed read and quit the app for nothing. Same fail-closed rule as the busy
  // check: a comparison we could not make means do nothing at all.
  if (staged !== null && from === null) {
    const message = `cannot tell whether ${staged} is waiting: the version of ${service.path} could not be read, so ${service.name} was left alone`;
    log(message, 'update');
    return { ok: false, action: 'failed', from: null, to: staged, message };
  }

  // Nothing waiting. Remember that pending/ keeps the archive of the version
  // that is already running, so "staged equals installed" is the ordinary steady
  // state right after an update applied, not a sign that anything went wrong.
  if (staged === null || staged === from) {
    const latest = (await appProvider.cachedLatestVersion(service, { timeoutMs: 8000 })).version;
    let message =
      staged === null
        ? `nothing is staged: ${service.name} has not downloaded an update`
        : `nothing is staged: the downloaded build (${staged}) is the one already running`;
    // Worth surfacing: this is the one state that looks like a stall. The app is
    // behind, and the reason is that it has not fetched the new build yet, which
    // is entirely its own business and not something a restart would fix.
    if (latest && latest !== from) message += `; the release feed has ${latest}, which the app has not downloaded yet`;
    if (state.pendingRestart) saveServiceState(service, { pendingRestart: false, pendingVersion: null });
    return { ok: true, action: 'noop', from, to: null, message };
  }

  // A build that the app will never apply must not be retried forever. Without
  // this, a staged version that fails to take (the app was force killed after
  // staging, so the in-session install-on-quit flag is gone; or the machine is
  // offline so the updater never re-registers the download) makes the 15 minute
  // timer quit and relaunch the app the user works in, every quarter hour,
  // indefinitely. Give up after APPLY_ATTEMPTS tries at the SAME target and say
  // so; --force still overrides, and a different staged version resets it.
  const failures = state.applyFailures;
  if (!force && failures && failures.version === staged && failures.count >= APPLY_ATTEMPTS) {
    const message =
      `${staged} failed to apply ${failures.count} times, so it will not be retried automatically. ` +
      `Quit and reopen ${service.name} yourself to apply it, or pass --force to try again`;
    return { ok: true, action: 'noop', from, to: staged, message };
  }

  // Not running: there is nothing to do and nothing is wrong. The updater
  // applies a staged build when the app quits, so a closed app will pick this up
  // by itself the next time it is opened and closed. Opening it from a
  // background timer would be worse than useless.
  if (!appProvider.isAppRunning(service)) {
    return {
      ok: true,
      action: 'noop',
      from,
      to: staged,
      message: `${staged} is downloaded, and ${service.name} is closed; it applies by itself the next time you quit the app`,
    };
  }

  // Busy gate, and for an app it is the important one: quitting it is quitting
  // the window the user is actually working in.
  const { busy } = await checkAllBusy(config);
  if (busy.busy && !force) {
    saveServiceState(service, { pendingRestart: true, pendingVersion: staged });
    const message = `the machine is busy (${busy.reason}); ${staged} is downloaded and will be applied at the next idle window`;
    log(message, 'update');
    recordLastUpdate(service, 'deferred', from, staged, message);
    return { ok: true, action: 'deferred', from, to: staged, message, busy };
  }

  const lock = acquireLock(service, staged);
  if (!lock.ok) {
    const message = `maintenance lock unavailable: ${lock.message}`;
    log(message, 'update');
    return { ok: true, action: 'noop', from, to: staged, message };
  }

  try {
    note(`applying the staged ${service.name} build ${from ?? 'unknown'} -> ${staged}`, 'update');
    const restarted = await appProvider.restartApp(service);
    if (restarted.notRunning) {
      // It quit between the check above and here, which is fine: the staged build
      // applies on that quit anyway. Nothing left for us to do.
      const message = `${service.name} closed while the update was starting; ${staged} applies on that quit by itself`;
      log(message, 'update');
      return { ok: true, action: 'noop', from, to: staged, message };
    }
    if (!restarted.ok) {
      log(`update to ${staged} failed: ${restarted.message}`, 'update');
      recordLastUpdate(service, 'failed', from, staged, restarted.message);
      return { ok: false, action: 'failed', from, to: staged, message: restarted.message };
    }

    const health = await waitForServiceHealth(service, HEALTH_WAIT_MS);
    if (!health.ok) {
      const message = `${service.name} was restarted but is not answering: ${describeHealth(health)}`;
      log(message, 'update');
      recordLastUpdate(service, 'failed', from, staged, message);
      return { ok: false, action: 'failed', from, to: staged, message };
    }

    // Re-read the bundle rather than trusting the restart, but poll for it: the
    // swap happens on quit and a single read right afterwards can catch it mid
    // flight and report a perfectly good update as failed.
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
      return { ok: false, action: 'failed', from, to: staged, message };
    }

    saveServiceState(service, { pendingRestart: false, pendingVersion: null, applyFailures: null });
    const message = `applied the staged build ${from ?? 'unknown'} -> ${staged}`;
    log(message, 'update');
    recordLastUpdate(service, 'ok', from, staged, message);
    return { ok: true, action: 'updated', from, to: staged, message };
  } finally {
    releaseLock(lock);
  }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

async function runCommandUpdate(config, service, { force }) {
  const from = commandProvider.installedVersion(service);
  if (!service.update) {
    return { ok: true, action: 'noop', from, to: null, message: `${service.name} has no update command configured` };
  }

  const resolved = commandProvider.fetchLatestVersion(service, { timeoutMs: 30000 });
  const to = resolved.version;
  if (!to) {
    // With no way to ask what the newest version is, running the update on every
    // scheduled cycle would stop and start the service every quarter hour for
    // nothing. Only an explicit --force runs it blind.
    if (!force) {
      const why = service.latestVersion
        ? `could not resolve the newest version: ${resolved.error ?? 'no version returned'}`
        : `${service.name} has no latestVersion command, so there is nothing to compare against`;
      return { ok: true, action: 'noop', from, to: null, message: `${why}; pass --force to update anyway` };
    }
  } else if (from === to) {
    return alreadyOnTarget(config, service, from, to, force);
  }

  const { busy } = await checkAllBusy(config);
  if (busy.busy && !force) {
    saveServiceState(service, { pendingRestart: true, pendingVersion: to });
    const message = `the machine is busy (${busy.reason}); ${to ?? 'the update'} will be applied at the next idle window`;
    log(message, 'update');
    recordLastUpdate(service, 'deferred', from, to, message);
    return { ok: true, action: 'deferred', from, to, message, busy };
  }

  const lock = acquireLock(service, to ?? 'update');
  if (!lock.ok) {
    const message = `maintenance lock unavailable: ${lock.message}`;
    log(message, 'update');
    return { ok: true, action: 'noop', from, to, message };
  }

  try {
    note(`updating ${service.name} ${from ?? 'nothing'} -> ${to ?? 'whatever the update command installs'}`, 'update');
    const stopped = await stopAndWait(service, STOP_WAIT_MS);
    if (!stopped.ok) {
      const message = `${service.name} would not stop: ${stopped.message}`;
      log(message, 'update');
      recordLastUpdate(service, 'failed', from, to, message);
      return { ok: false, action: 'failed', from, to, message };
    }

    const updated = commandProvider.update(service);
    // There is no rollback here: an update described by one command has no
    // previous artefact to put back. The most we can do is make sure the service
    // comes up again on whatever is now installed.
    if (!updated.ok) {
      await ensureServiceUp(service);
      const message = `the update command failed: ${updated.message}`;
      log(message, 'update');
      recordLastUpdate(service, 'failed', from, to, message);
      return { ok: false, action: 'failed', from, to, message };
    }

    const started = await startAndSettle(service);
    if (!started.ok) {
      const message = `${service.name} was updated but would not start: ${started.message}`;
      log(message, 'update');
      recordLastUpdate(service, 'failed', from, to, message);
      return { ok: false, action: 'failed', from, to, message };
    }

    const health = await waitForServiceHealth(service, HEALTH_WAIT_MS);
    if (!health.ok) {
      const message = `${service.name} was updated but is not answering: ${describeHealth(health)}`;
      log(message, 'update');
      recordLastUpdate(service, 'failed', from, to, message);
      return { ok: false, action: 'failed', from, to, message };
    }

    const now = commandProvider.installedVersion(service) ?? to;
    saveServiceState(service, { pendingRestart: false, pendingVersion: null });
    const message = `updated ${from ?? 'nothing'} -> ${now ?? 'an unreported version'}`;
    log(message, 'update');
    recordLastUpdate(service, 'ok', from, now, message);
    return { ok: true, action: 'updated', from, to: now, message };
  } finally {
    releaseLock(lock);
  }
}

/** The installed version, without any discovery that would slow a refusal down. */
function installedFor(service) {
  if (service.kind === 'app') return appProvider.installedVersion(service);
  if (service.kind === 'npm') return npmProvider.installedVersion(service, { allowProbe: false });
  return commandProvider.installedVersion(service);
}

/**
 * One update cycle for one service. Always returns an object; never throws.
 */
export async function runUpdate(config, service, { force = false } = {}) {
  if (!config.autoUpdate && !force) {
    return {
      ok: true,
      action: 'noop',
      from: installedFor(service),
      to: null,
      message: 'auto-update is off; pass --force to update anyway',
    };
  }

  if (service.kind === 'app') return runAppUpdate(config, service, { force });
  if (service.kind === 'npm') return runNpmUpdate(config, service, { force });
  return runCommandUpdate(config, service, { force });
}
