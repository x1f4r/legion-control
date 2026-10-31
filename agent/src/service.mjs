// One service's lifecycle, on top of the probes: look at it, start it, stop it
// and wait for the process to really be gone, restart it, wait for it to answer.
//
// Everything here takes a normalized service object. Nothing in this file knows
// which product it is looking after.

import { REASON } from './contract.mjs';
import { detectPlatform } from './paths.mjs';
import { describeFailure, runMutationArgv, sleep } from './process-runner.mjs';
import { checkServiceBusy } from './probes/busy.mjs';
import { checkHealth, describeHealth, healthPort } from './probes/health.mjs';
import { isRunning, probeProcess, processEvidence, processStartedAt, startProcess, stopProcess, systemctlMutation } from './probes/process.mjs';
import { NO_RELAY, probeRelay } from './probes/relay.mjs';
import * as appProvider from './providers/app.mjs';

/** How long a started service gets to bind before anyone checks it, and how long systemd gets to restart one. */
export const BIND_GRACE_MS = 2000;
export const SYSTEMD_RESTART_MS = 90000;

export { isRunning, healthPort, processEvidence, processStartedAt };

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
 * The same evidence for every service on the machine, for a gate that asks
 * all of them. A state database's leftover row belongs to its own service,
 * and only that service's liveness can retire it, so a gate that passed only
 * the service it was about to act on kept every other service's dead rows
 * blocking it.
 */
export function machineLiveness(config) {
  return Object.fromEntries(config.services.map((service) => [service.id, livenessOf(service)]));
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
 * What recovery may act on to say whether a service is up: the process probe's
 * reading, whose unknown is neither of the others (processEvidence says why).
 *
 * Except for a command process with no running command, which has no probe
 * that could fail, only none at all. Its health check is then the one reading
 * there is of it. One that passes is a positive reading that it is up, which
 * is all an obligation to put it back asks for. One that fails is no reading
 * that it is stopped, because a service can be up and not answering, so it
 * stays unknown: recovery neither starts it nor takes it for running.
 */
export async function recoveryEvidence(service) {
  const evidence = processEvidence(service);
  const unprobed = service.process?.type === 'command' && !service.process.running;
  const checked = service.health?.type === 'http' || service.health?.type === 'command';
  if (evidence.state !== 'unknown' || !unprobed || !checked) return evidence;
  const health = await checkHealth(service);
  if (health.ok) return { state: 'running', error: null };
  return { state: 'unknown', error: `${evidence.error}, and its health check does not pass: ${describeHealth(health)}` };
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

export async function startService(service) {
  return startProcess(service);
}

export async function stopService(service) {
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
export async function drainService(service) {
  if (!canDrain(service)) return { ok: true, drained: false, message: null };
  const result = await runMutationArgv(service.drain.command, { timeoutMs: service.drain.timeoutSeconds * 1000 });
  return {
    ok: result.ok,
    drained: result.ok,
    message: result.ok ? `${service.name} was drained` : `the drain command failed: ${describeFailure(result)}`,
  };
}

/**
 * Wait for a service that has no process of its own to have nothing running.
 *
 * A CLI only ever runs as the sessions somebody starts, so there is nothing to
 * stop, and the process probe answers "running" for it for ever: with nothing
 * to start or stop, nothing can be down either. Waiting on that probe failed
 * every such update 20 s after a stop nobody had asked for. What its profile
 * promises instead is that every related process has exited, which is the
 * question its busy probe answers, so that is asked until it says idle or the
 * budget runs out. One still busy then is not a stop that failed, because
 * nothing was asked to stop: `busy` carries the answer, and the update waits.
 */
async function waitUntilIdle(service, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  const own = (service.busy?.timeoutSeconds ?? 8) * 1000;
  for (;;) {
    const busy = await checkServiceBusy(service, { timeoutMs: Math.min(own, Math.max(0, deadline - Date.now())), liveness: livenessOf(service) });
    if (!busy.busy) return { ok: true, message: null };
    // Another reading goes only where it has a second of its own to answer in.
    // One asked with the last few milliseconds of the budget times out, and a
    // plain "still running" would end as "could not be established".
    if (Date.now() + 2000 >= deadline) {
      const waited = Math.round(timeoutMs / 1000);
      const state = busy.unknown ? 'whether anything related was still running could not be established' : 'something related was still running';
      return { ok: false, busy, message: `${service.name} has nothing of its own to stop, and ${waited} s later ${state}: ${busy.error ?? busy.reason}` };
    }
    await sleep(1000);
  }
}

/**
 * Stop and wait for the process to actually disappear. An installer must not
 * replace a service's files while a process still has them open.
 *
 * A stop that did not complete carries the reasonCode of what may follow it.
 * One nothing could ask for is `not-configured`, and one refused while the
 * service plainly kept running is `internal`: either way it is still up, as it
 * was. One asked for and not seen through, still running at the deadline or
 * never readable, may yet be under way, so it is `timed-out`, whose outcome is
 * unknown. None of them is `busy`: every client words that as nothing having
 * been touched, and a stop request is something.
 */
export async function stopAndWait(service, timeoutMs = 20000) {
  // An app has its own budget and its own wording: quitting an app someone is
  // working in is not the same act as stopping a background service, and 20 s is
  // not long enough for a shutdown that has anything to save.
  if (service.process?.type === 'app') {
    return appProvider.quitAndWait(service, Math.max(timeoutMs, appProvider.QUIT_WAIT_MS));
  }
  if ((service.process?.type ?? 'none') === 'none') return waitUntilIdle(service, timeoutMs);

  const stopped = await stopService(service);
  const deadline = Date.now() + timeoutMs;
  let evidence = processEvidence(service);
  while (evidence.state !== 'stopped') {
    // Nothing was asked to stop, so nothing is going to.
    if (stopped.notConfigured && evidence.state === 'running') break;
    if (Date.now() + 1000 >= deadline) break;
    await sleep(1000);
    evidence = processEvidence(service);
  }
  if (evidence.state === 'stopped') return { ok: true, message: null };

  const waited = Math.round(timeoutMs / 1000);
  if (stopped.notConfigured) {
    return { ok: false, reasonCode: REASON.notConfigured, message: `${stopped.message}, so it could not be stopped` };
  }
  if (!stopped.ok && evidence.state === 'running') {
    return { ok: false, reasonCode: REASON.internal, message: `the stop request failed (${stopped.message}), and ${service.name} was still running ${waited} s later` };
  }
  if (evidence.state === 'running') {
    return { ok: false, reasonCode: REASON.timedOut, message: `${service.name} was still running ${waited} s after the stop request` };
  }
  const asked = stopped.ok ? 'was asked to stop' : `was asked to stop, though the request failed (${stopped.message})`;
  return { ok: false, reasonCode: REASON.timedOut, message: `${service.name} ${asked}, and ${waited} s later whether it had could not be established: ${evidence.error}` };
}

/** Start and give the service a moment to bind before anyone health-checks it. */
export async function startAndSettle(service) {
  const started = await startService(service);
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
    const result = await systemctlMutation(service, 'restart', SYSTEMD_RESTART_MS);
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
