// One service's lifecycle, on top of the probes: look at it, start it, stop it
// and wait for the process to really be gone, restart it, wait for it to answer.
//
// Everything here takes a normalized service object. Nothing in this file knows
// which product it is looking after.

import { describeFailure, detectPlatform, runArgv, runCommand, sleep } from './config.mjs';
import { checkHealth, healthPort } from './probes/health.mjs';
import { isRunning, probeProcess, processStartedAt, startProcess, stopProcess } from './probes/process.mjs';
import { NO_RELAY, probeRelay } from './probes/relay.mjs';
import * as appProvider from './providers/app.mjs';

const BIND_GRACE_MS = 2000;

export { isRunning, healthPort, processStartedAt };

/** Find the service the command should act on. */
export function selectService(config, id) {
  if (!id) return config.services[0] ?? null;
  return config.services.find((service) => service.id === id) ?? null;
}

/**
 * One call that answers "is the process there" and "is the relay up". They are
 * probed together because on Windows each PowerShell start costs about half a
 * second and `status` has a budget.
 */
export function probeService(service) {
  const wantsRelay = service.relay?.type === 'cloudflared';
  const probe = probeProcess(service, { includeRelay: wantsRelay && service.process?.type === 'scheduled-task' });
  const relay = probe.relay ?? (wantsRelay ? probeRelay(service) : NO_RELAY);
  return {
    running: probe.running,
    unitState: probe.unitState,
    pids: probe.pids,
    relay,
    error: probe.error,
    port: healthPort(service),
  };
}

/**
 * What the busy probes are allowed to use as outside evidence about a service.
 *
 * Gathered once and passed down rather than looked up inside each probe, because
 * on Windows every one of these is a PowerShell start and the busy check would
 * otherwise pay for it again per service.
 */
export function livenessOf(service) {
  let serviceRunning = null;
  try {
    serviceRunning = isRunning(service);
  } catch {
    /* no evidence is a valid answer; the probe keeps its protection */
  }
  return { serviceRunning, startedAt: serviceRunning === true ? processStartedAt(service) : null };
}

/**
 * The health check, with "none" resolved against the process probe. The process
 * is only probed when the answer actually depends on it: on Windows that probe
 * is a PowerShell start, and paying for one before every http health check would
 * be half a second thrown away each time.
 */
export async function serviceHealth(service, { running, timeoutMs = null, useAsync = false } = {}) {
  const dependsOnProcess = (service.health?.type ?? 'none') === 'none';
  const up = running ?? (dependsOnProcess ? isRunning(service) : true);
  return checkHealth(service, { running: up, timeoutMs, useAsync });
}

/**
 * Poll until the service answers or the budget runs out. A service whose health
 * is "none" is healthy as soon as its process is back, so that case polls the
 * process probe rather than returning true on the spot.
 */
export async function waitForServiceHealth(service, timeoutMs, { intervalMs = 2000 } = {}) {
  const deadline = Date.now() + timeoutMs;
  let last = { ok: false, status: 0, error: 'not checked' };
  for (;;) {
    const running = service.health?.type === 'none' ? isRunning(service) : true;
    last = await checkHealth(service, { running });
    if (last.ok) return last;
    if (Date.now() >= deadline) return last;
    await sleep(intervalMs);
  }
}

export function startService(service) {
  return startProcess(service);
}

export function stopService(service) {
  return stopProcess(service);
}

/** Whether this service knows how to stop taking new work. */
export function canDrain(service) {
  return Boolean(service.drain?.command);
}

/**
 * Ask a service to stop accepting new work and finish what it has, before it is
 * stopped.
 *
 * There is no generic way to do this — a systemd unit has no notion of it and an
 * app certainly does not — so it happens only where the config names a command
 * for it. Where one exists it closes the window the busy check cannot: between
 * "nothing is running" and the stop, new work can start, and a drain is the only
 * thing that prevents that rather than merely noticing it afterwards.
 *
 * A drain that fails stops the update. The alternative is to stop a service that
 * has just told us it is not ready, which is the thing the drain existed to
 * avoid.
 */
export function drainService(service) {
  if (!canDrain(service)) return { ok: true, drained: false, message: null };
  const result = runArgv(service.drain.command, { timeoutMs: service.drain.timeoutSeconds * 1000 });
  return {
    ok: result.ok,
    drained: result.ok,
    message: result.ok ? `${service.name} was drained` : `the drain command failed: ${describeFailure(result)}`,
  };
}

/**
 * Stop and wait for the process to actually disappear. An installer must not
 * replace a service's files while a process still has them open.
 */
export async function stopAndWait(service, timeoutMs = 20000) {
  // An app has its own budget and its own wording: quitting an app someone is
  // working in is not the same act as stopping a background service, and 20 s is
  // not long enough for a shutdown that has anything to save.
  if (service.process?.type === 'app') {
    return appProvider.quitAndWait(service, Math.max(timeoutMs, appProvider.QUIT_WAIT_MS));
  }

  const stopped = stopService(service);
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (!isRunning(service)) return { ok: true, message: null };
    await sleep(1000);
  }
  return {
    ok: false,
    message: stopped.ok
      ? `${service.name} was still running ${Math.round(timeoutMs / 1000)} s after the stop request`
      : `stop failed: ${stopped.message}`,
  };
}

/** Start and give the service a moment to bind before anyone health-checks it. */
export async function startAndSettle(service) {
  const started = startService(service);
  await sleep(BIND_GRACE_MS);
  return started;
}

export async function restartService(service) {
  const type = service.process?.type;
  if (type === 'app') {
    // Note what restarting an app of this kind really is: it is the update
    // mechanism. Quitting is the trigger the updater waits for, so a restart
    // with something staged comes back on the new version whether or not anyone
    // asked for an update. That is the app's behaviour, not ours, and it is why
    // the busy gate in front of restart matters just as much here as it does for
    // a background service.
    //
    // launchIfStopped is true here and false in the update cycle, and the
    // difference is who asked. This path is only ever reached because a person
    // pressed restart, and starting a closed app is what they meant. The
    // scheduled cycle gets the opposite default, so it can never reopen an app
    // that was closed on purpose.
    return appProvider.restartApp(service, { launchIfStopped: true });
  }
  if (type === 'systemd-user' || type === 'systemd-system') {
    // One `systemctl restart` rather than a stop/start pair: it keeps the unit's
    // own ordering and its restart policy intact.
    const unit = service.process.unit;
    const result =
      type === 'systemd-system'
        ? runCommand('sudo', ['-n', 'systemctl', 'restart', unit], { timeoutMs: 90000 })
        : runCommand('systemctl', ['--user', 'restart', unit], { timeoutMs: 90000 });
    await sleep(BIND_GRACE_MS);
    return { ok: result.ok, message: result.ok ? null : describeFailure(result) };
  }
  const stopped = await stopAndWait(service, 20000);
  if (!stopped.ok) return stopped;
  return startAndSettle(service);
}

/** Whether this platform can restart the service at all. */
export function canRestart(service) {
  const type = service.process?.type;
  if (!type || type === 'none') return false;
  const platform = detectPlatform();
  if (type === 'app') return platform === 'mac';
  if (type === 'scheduled-task') return platform === 'windows';
  if (type === 'systemd-user' || type === 'systemd-system') return platform === 'linux';
  return Boolean(service.process.start || service.process.stop);
}
