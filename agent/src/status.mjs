// The bounded, honest snapshot.
//
// Two rules the first version broke.
//
// IT MUST FIT IN THE BUDGET. Probes ran one after another and each had its own
// timeout, so two harmless sixteen-second busy probes produced a perfectly good
// reply after thirty-two seconds — past the point where both clients had already
// given up and called the machine unreachable. Independent probes now run
// concurrently against ONE shared deadline, and whatever does not fit is
// reported as timed out rather than waited for.
//
// IT MUST NOT GUESS. Five different things used to collapse into "healthy":
// whether the machine answered at all, whether the agent ran, whether the
// service's process exists, whether it is serving, and whether anything outside
// the machine can reach it. They are now five separate fields, each with its own
// timestamp and its own error, and the external one is not probed here at all
// because turning a poll people press twenty times a day into an Internet round
// trip is how status becomes the slow, flaky part of the app.
//
// Status NEVER mutates. It may write the reconstructible cache and nothing else,
// and it derives the state of an interrupted operation at read time rather than
// fixing the record, which is a mutating command's job.

import os from 'node:os';
import path from 'node:path';
import {
  AGENT_VERSION,
  basePath,
  compilePattern,
  deadline as makeDeadline,
  describeFailure,
  mapWithLimit,
  platformName,
  runArgvAsync,
  runCommandAsync,
} from './config.mjs';
import { CONTRACT_VERSION } from './contract.mjs';
import { listActions } from './actions.mjs';
import { bootTargets } from './boot.mjs';
import { readController } from './controller.mjs';
import { checkHealth, healthPort } from './probes/health.mjs';
import { probeProcessAsync } from './probes/process.mjs';
import { collectTelemetry } from './probes/telemetry.mjs';
import { deriveState, listOperations, summarize } from './operations.mjs';
import { describeServiceUpdates, describeSystemUpdates } from './policy.mjs';
import { canDrain, canRestart } from './service.mjs';
import { serviceState } from './state.mjs';
import { statusBootIdentity, statusBusy } from './status-probes.mjs';
import * as appProvider from './providers/app.mjs';
import * as npmProvider from './providers/npm.mjs';

export const DEFAULT_STATUS_BUDGET_MS = 20000;
const SERVICE_CONCURRENCY = 4;

function firstVersionLine(stdout) {
  return String(stdout ?? '').split('\n').map((line) => line.trim()).find(Boolean) ?? null;
}

/** Status has async readers; mutation providers keep their sequential API. */
async function statusVersions(service, clock) {
  const installed = async () => {
    if (service.kind === 'npm') return npmProvider.installedVersion(service, { allowProbe: false, readOnly: true });
    const timeoutMs = clock.slice(4000);
    if (timeoutMs <= 0) return null;
    if (service.kind === 'app') {
      if (!service.path) return null;
      const result = await runCommandAsync('/usr/libexec/PlistBuddy',
        ['-c', 'Print :CFBundleShortVersionString', path.join(service.path, 'Contents', 'Info.plist')], { timeoutMs });
      return result.ok ? result.stdout.trim() || null : null;
    }
    if (!service.installedVersion) return null;
    const result = await runArgvAsync(service.installedVersion, { timeoutMs });
    return result.ok ? firstVersionLine(result.stdout) : null;
  };
  const latest = () => {
    if (service.kind !== 'app' && service.kind !== 'npm' && !service.latestVersion) return { version: null, error: null };
    const source = service.kind === 'app' ? 'github' : service.kind === 'npm' ? 'npm' : 'command';
    return npmProvider.cachedLookup(npmProvider.channelCacheKey(service, source), { readOnly: true, cacheWaitMs: 0 }, async () => {
      const timeoutMs = clock.slice(4000);
      if (timeoutMs <= 0) return { version: null, error: 'there was no time left in the status budget to check the latest version' };
      if (service.kind === 'app') return appProvider.fetchLatestVersion(service, { timeoutMs });
      let result;
      if (service.kind === 'npm') {
        const invocation = npmProvider.npmInvocation(service.npmPrefix);
        if (invocation.error) return { version: null, error: invocation.error };
        result = await runCommandAsync(invocation.file,
          [...invocation.lead, 'view', `${service.package}@${service.channel || 'latest'}`, 'version', '--json'],
          { timeoutMs, shell: invocation.shell });
      } else result = await runArgvAsync(service.latestVersion, { timeoutMs });
      if (!result.ok) return { version: null, error: describeFailure(result) };
      let version = firstVersionLine(result.stdout);
      if (service.kind === 'npm') {
        try {
          const parsed = JSON.parse(result.stdout.trim());
          version = Array.isArray(parsed) ? parsed.at(-1) : parsed;
        } catch { return { version: null, error: 'npm view returned output that is not JSON' }; }
      }
      const pattern = service.kind === 'npm' ? npmProvider.versionPattern(service) : compilePattern(service.versionPattern, null);
      return typeof version === 'string' && version.length > 0 && (!pattern || pattern.test(version))
        ? { version, error: null }
        : { version: null, error: `unexpected version: ${JSON.stringify(version)}` };
    });
  };
  const [installedVersion, latestVersion] = await Promise.all([installed(), latest()]);
  return { installed: installedVersion, latest: latestVersion, staged: service.kind === 'app' ? appProvider.stagedVersion(service) : null };
}

/**
 * The per-service policy block as status carries it.
 *
 * `describeServiceUpdates` answers more than status is allowed to say — the
 * pause source, whether the windows were inherited, the next window — because
 * `policy` needs those. Status carries the fixed set and nothing else, so a key
 * added for one command cannot leak into a reply another one validates.
 */
function statusUpdates(described) {
  const block = {
    automatic: described.automatic,
    inherited: described.inherited,
    pauseUntil: described.pauseUntil,
    eligibleNow: described.eligibleNow,
    deferredReason: described.deferredReason,
  };
  if (described.maintenanceWindows !== undefined) block.maintenanceWindows = described.maintenanceWindows;
  if (described.order) block.order = described.order;
  if (Array.isArray(described.after) && described.after.length > 0) block.after = described.after;
  return block;
}

function timedOutBusy(serviceName) {
  return {
    busy: true,
    unknown: true,
    monitored: true,
    reason: 'busy state unknown',
    evidence: 'timed-out',
    checkedAt: new Date().toISOString(),
    elapsedMs: 0,
    error: `there was no time left in the status budget to ask ${serviceName}`,
    runningTurns: 0,
    pendingTurns: 0,
    pendingApprovals: 0,
    staleTurns: 0,
    staleApprovals: 0,
    threads: [],
    threadsTruncated: 0,
  };
}

/** Everything `status` says about one service, inside whatever budget is left. */
async function describeService(config, service, { isFirst, clock, notes, operations }) {
  const state = serviceState(service, { isFirst, readOnly: true });

  // Gather process/start-time evidence once, without blocking other services.
  const measured = await probeProcessAsync(service, { clock, includeRelay: service.relay?.type === 'cloudflared' });
  const probe = { ...measured, port: healthPort(service), relay: measured.relay ?? { configured: false, running: false } };
  if (probe.error) notes.push(`${service.name}: ${probe.error}`);
  const startedAt = probe.startedAt ?? null;
  // Failure is not evidence that the service stopped; busy probes must keep
  // protecting unfinished work when process liveness could not be measured.
  const systemdTransition = service.process?.type?.startsWith('systemd-') && !['active', 'inactive', 'failed'].includes(probe.unitState);
  const liveness = { serviceRunning: probe.error || systemdTransition ? null : probe.running, startedAt };

  const busy = clock.expired()
    ? timedOutBusy(service.name)
    : await statusBusy(service, { clock, liveness });
  if (busy.unknown && busy.error) notes.push(`${service.name} busy: ${busy.error}`);
  if (busy.monitored === false && busy.evidence === 'unmonitored') {
    notes.push(`${service.name}: busy probe not monitored`);
  }

  // status must stay fast and must not change anything, so it never runs prefix
  // discovery (up to 15 s, and it writes the probe result) and every "what is
  // the newest version" lookup goes through the ten minute cache.
  const { installed, latest, staged } = await statusVersions(service, clock);
  if (!latest.version && latest.error) notes.push(`${service.name}: ${latest.error}`);

  // Skip the health probe when nothing is running: it keeps status inside its
  // budget, and a service that is down cannot be healthy anyway.
  const health = probe.running
    ? await checkHealth(service, { running: true, timeoutMs: clock.slice((service.health?.timeoutSeconds ?? 5) * 1000), useAsync: true })
    : { ok: false, status: 0, error: probe.error ? `${service.name} process state is unknown` : `${service.name} is not running`, checkedAt: new Date().toISOString(), elapsedMs: 0 };
  if (!health.ok && health.error && probe.running) notes.push(`${service.name}: ${health.error}`);

  // A configured relay that is down makes the service unreachable from outside,
  // so it counts against health. An absent relay does not.
  const relayOk = !probe.relay.configured || probe.relay.running;
  if (!relayOk) notes.push(`${service.name}: the relay is configured but not running`);

  const last = operations.find((record) => record.service === service.id) ?? null;

  return {
    id: service.id,
    name: service.name,
    kind: service.kind,
    installed,
    // With no answer we report "cannot tell" rather than crying wolf, and null is
    // not the same as "yes". If the lookup did not come back we do not know what
    // the newest version is, and claiming the install is current on that basis
    // would be a confident lie: the one moment this happens is right after a
    // boot, which is exactly when a night of new builds is most likely to be
    // waiting.
    latest: latest.version ?? null,
    channel: service.channel ?? null,
    upToDate: latest.version ? installed === latest.version : null,
    running: probe.running,
    healthy: probe.running && health.ok && relayOk,
    port: probe.port,
    staged,
    appPath: service.kind === 'app' ? service.path : null,
    busy,
    relay: probe.relay,
    // The four things that used to be one word.
    process: { running: probe.running, state: probe.unitState, startedAt, error: probe.error ?? null },
    health: { ok: health.ok, status: health.status, checkedAt: health.checkedAt, elapsedMs: health.elapsedMs, error: health.error ?? null },
    // Never probed here: an external reachability check on every poll would turn
    // the thing people press twenty times a day into an Internet round trip.
    // `doctor --deep` is where that question is actually asked.
    endpoint: { configured: Boolean(service.endpoint), reachable: null },
    updates: statusUpdates(describeServiceUpdates(config, service, { state, busy })),
    pendingVersion: state.pendingVersion ?? null,
    drain: canDrain(service),
    lastOperation: last
      ? { opId: last.id, kind: last.kind, action: last.result?.action ?? null, at: last.finishedAt ?? last.requestedAt ?? null }
      : null,
    pendingRestart: Boolean(state.pendingRestart) && (!state.pendingVersion || state.pendingVersion !== installed),
    lastUpdate: state.lastUpdate ?? null,
    canUpdate: service.kind === 'command' ? Boolean(service.update) : true,
    canRestart: canRestart(service),
  };
}

/**
 * The whole snapshot.
 *
 * `loaded` is the result of loadConfig(), passed in rather than read here so a
 * broken config still produces a reply that says so instead of nothing at all.
 */
export async function buildStatus(loaded, { budgetMs = DEFAULT_STATUS_BUDGET_MS } = {}) {
  const clock = makeDeadline(budgetMs);
  const startedAt = Date.now();
  const notes = [];
  const config = loaded.config;

  const agent = {
    version: AGENT_VERSION,
    contract: CONTRACT_VERSION,
    base: basePath(),
    node: process.versions.node,
    restrictedSession: process.env.LEGIONCTL_RESTRICTED === '1',
    capabilities: ['setup-lineage', 'wol-action', 'configured-telemetry', 'service-config-admin'],
  };

  const configBlock = {
    ok: loaded.ok === true,
    source: loaded.source ?? 'defaults',
    problems: [...(loaded.problems ?? []), ...(loaded.warnings ?? []), ...(loaded.migrations ?? [])],
  };

  // A config the agent cannot use still gets a reply: the client needs to be
  // able to say WHY every button is disabled, and a bare failure cannot.
  if (!config) {
    const stored = readController();
    return {
      ok: false,
      os: platformName(),
      hostname: os.hostname(),
      agent,
      config: configBlock,
      timing: { budgetMs, elapsedMs: Date.now() - startedAt, partial: false },
      services: [],
      // Fails closed, like everything else: a machine whose configuration cannot
      // be read is not a machine anyone should be starting an install on.
      busy: {
        busy: true,
        unknown: true,
        reason: 'the configuration could not be read, so nothing about this machine is known',
        monitoredServices: 0,
        unmonitoredServices: 0,
      },
      bootTargets: [],
      actions: [],
      autoUpdate: false,
      updates: { automatic: false, pauseUntil: null, maintenanceWindows: [], inWindowNow: true, nextWindow: null, lastCycle: null },
      operations: { running: [], queued: [], recent: [] },
      controller: {
        hash: stored.hash,
        id: stored.meta.id,
        revision: stored.meta.revision,
        updatedAt: stored.meta.updatedAt,
        source: stored.meta.source,
      },
      t3: null,
      pendingRestart: false,
      lastUpdate: null,
      connect: { configured: false, running: false },
      notes: configBlock.problems.map((problem) => `${problem.path}: ${problem.message}`),
    };
  }

  // Read once, share everywhere: the operations directory is scanned for the
  // running list, the queue, the recent five and each service's last outcome.
  const storedRecords = listOperations({ limit: 60 });
  const bootId = storedRecords.length ? await statusBootIdentity(clock) : null;
  const records = storedRecords.map((record) => deriveState(record, { systemId: config.system.id, bootId }));
  const finished = records.filter((record) => record.state === 'finished');
  const operations = {
    running: records.filter((record) => record.state === 'running').map(summarize),
    queued: records.filter((record) => record.state === 'queued').map(summarize),
    recent: finished.slice(0, 5).map(summarize),
  };
  const lastCycle = finished.find((record) => record.kind === 'cycle');

  const [services, metrics] = await Promise.all([
    mapWithLimit(config.services, SERVICE_CONCURRENCY, (service, index) =>
      describeService(config, service, { isFirst: index === 0, clock, notes, operations: finished })),
    loaded.ok === true && config.telemetry?.probes.length ? collectTelemetry(config.telemetry.probes, clock) : undefined,
  ]);

  const entries = services.map((described, index) => ({ service: config.services[index], busy: described.busy }));
  const blocking = entries.filter((entry) => entry.busy.busy === true);
  const busy = {
    busy: blocking.length > 0,
    unknown: entries.some((entry) => entry.busy.unknown === true),
    reason:
      entries.length === 0
        ? 'no services are configured'
        : entries.length === 1
          ? entries[0].busy.reason
          : entries.map((entry) => `${entry.service.name}: ${entry.busy.reason}`).join('; '),
    monitoredServices: entries.filter((entry) => entry.busy.monitored === true).length,
    unmonitoredServices: entries.filter((entry) => entry.busy.monitored === false).length,
  };

  const stored = readController();
  const first = services[0] ?? null;

  // Kept for 2.x clients, and removed in 4.0. Nothing new is built on these.
  const t3 = first
    ? {
        installed: first.installed,
        nightly: first.latest,
        upToDate: first.upToDate,
        serverRunning: first.running,
        healthy: first.healthy,
        port: first.port,
      }
    : null;
  if (t3 && first.kind === 'app') {
    t3.staged = first.staged;
    t3.appPath = first.appPath;
  }

  for (const problem of configBlock.problems) notes.push(`${problem.path}: ${problem.message}`);

  return {
    ok: true,
    os: platformName(),
    system: config.system,
    hostname: os.hostname(),
    agent,
    config: configBlock,
    timing: { budgetMs, elapsedMs: Date.now() - startedAt, partial: clock.expired() },
    services,
    ...(metrics ? { metrics } : {}),
    busy,
    bootTargets: bootTargets(config),
    actions: listActions(config),
    updates: describeSystemUpdates(config, {
      lastCycle: lastCycle
        ? { opId: lastCycle.id, at: lastCycle.finishedAt ?? null, action: lastCycle.result?.action ?? null }
        : null,
    }),
    operations,
    // Only the hash and the identity: the document itself can be hundreds of
    // lines and status is polled. A device compares these against what it holds
    // and asks for the document with `config` only when they differ.
    controller: {
      hash: stored.hash,
      id: stored.meta.id,
      revision: stored.meta.revision,
      updatedAt: stored.meta.updatedAt,
      source: stored.meta.source,
    },
    notes,
    // 2.x compatibility keys.
    autoUpdate: config.updates.automatic === true,
    t3,
    pendingRestart: first ? first.pendingRestart : false,
    lastUpdate: first ? first.lastUpdate : null,
    connect: first ? first.relay : { configured: false, running: false },
  };
}
