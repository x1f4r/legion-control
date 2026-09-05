#!/usr/bin/env node
// legionctl — the Legion Control agent.
//
// Contract: every invocation prints exactly ONE JSON object to stdout and
// nothing else, including on failure. Progress and diagnostics go to stderr and
// to <base>/legionctl.log. Exit code 0 when the command did what was asked,
// 1 otherwise — where "did what was asked" includes deferring, queueing and
// doing nothing for a stated reason, because those are answers and not errors.
//
// Two rules that shape the whole file:
//
//   EVERY VALUE THAT ARRIVES OVER SSH IS VALIDATED. The restricted dispatcher
//   accepts a small quoted-argv grammar, then semantic ids such as services and
//   boot targets are checked before they reach code that uses them. A value that
//   fails is refused with bad-argument and the list of values that would work.
//
//   NOTHING MUTATES WHILE THE CONFIG IS BROKEN. A machine whose config.json does
//   not parse can still be read — status, doctor, logs, history all answer, and
//   they say why everything else is refused — but no command that would change
//   the machine runs on a configuration nobody can read.

import { spawnDetached } from './detached.mjs';
import { fileURLToPath } from 'node:url';
import { checkWorkerModules } from './selftest-modules.mjs';
import {
  AGENT_VERSION,
  basePath,
  configPath,
  isSupportedPlatform,
  loadConfig,
  platformName,
  readJsonDocument,
  saveConfig,
} from './config.mjs';
import {
  CONTRACT_VERSION,
  DURATION_PATTERN,
  envelope,
  isValidOperationId,
  isValidToken,
  parseDuration,
  REASON,
  resolveDeadline,
} from './contract.mjs';
import { runAction } from './actions.mjs';
import { MAX_CONTROLLER_BYTES, readController, readControllerMeta, storeController } from './controller.mjs';
import { buildBundle, runDoctor } from './doctor.mjs';
import { acquireOperationLock } from './lock.mjs';
import { log, readLogLines } from './log.mjs';
import {
  beginOperation,
  cancelOperation,
  currentInitiator,
  deriveState,
  listOperations,
  newOperationId,
  queueOperation,
  readOperation,
  summarize,
  updateOperation,
  wireRecord,
} from './operations.mjs';
import { describeServiceUpdates, describeSystemUpdates, validatePolicyPatch } from './policy.mjs';
import { commandServiceConfig } from './service-config.mjs';
import { runBoot, runSleep } from './power.mjs';
import { aggregateBusy, checkAllBusy } from './probes/busy.mjs';
import { runCycle } from './scheduler.mjs';
import { livenessOf, selectService } from './service.mjs';
import { buildStatus, DEFAULT_STATUS_BUDGET_MS } from './status.mjs';
import { serviceState } from './state.mjs';
import { checkBundle, resolveBootstrapLayout, runBootstrapInstall, runRollback, runSelfUpdate } from './selfupdate.mjs';
import { runRestart, runUpdate } from './update.mjs';
import { authorize } from './dispatch.mjs';

/**
 * The command table. `flags` stand alone, `options` take a value, `positional`
 * documents what a bare argument means. Anything outside it is refused, which is
 * what stops a typo from being read as a different command's flag.
 */
const COMMANDS = {
  status: { flags: [], options: ['budget-ms'], mutates: false, summary: 'bounded snapshot of the machine, its services and its operations' },
  busy: { flags: [], options: ['budget-ms'], mutates: false, summary: 'the aggregated busy state, with per-service evidence' },
  update: {
    flags: ['force', 'detach', 'when-idle'],
    options: ['service', 'op', 'expires'],
    mutates: true,
    summary: 'update one service',
  },
  restart: {
    flags: ['force', 'detach', 'when-idle'],
    options: ['service', 'op', 'expires'],
    mutates: true,
    summary: 'restart one service',
  },
  boot: {
    flags: ['force', 'no-reboot', 'when-idle'],
    options: ['op', 'expires'],
    mutates: true,
    summary: 'arm a boot target and reboot into it',
  },
  sleep: { flags: ['force', 'when-idle'], options: ['op', 'expires'], mutates: true, summary: 'suspend this machine' },
  run: {
    flags: ['force', 'detach', 'when-idle'],
    options: ['op', 'expires'],
    mutates: true,
    summary: 'run a configured action',
  },
  cycle: { flags: ['dry-run', 'force'], options: ['op'], mutates: true, summary: 'the scheduled maintenance cycle over every eligible service' },
  'auto-update': { flags: [], options: ['service'], mutates: true, summary: 'on, off, pause <duration> or resume' },
  policy: { flags: [], options: ['service'], mutates: true, summary: 'read the update policy; "policy set" reads a JSON patch from stdin' },
  'service-config': { flags: ['stdin'], options: [], mutates: true, summary: 'administrative service configuration: get, validate --stdin, or set --stdin' },
  op: { flags: [], options: ['wait'], mutates: false, summary: 'one operation record; --wait long-polls until it finishes' },
  cancel: { flags: [], options: [], mutates: true, summary: 'cancel a queued operation' },
  history: { flags: [], options: ['limit', 'service', 'kind'], mutates: false, summary: 'recent operations, newest first' },
  logs: { flags: [], options: ['lines', 'op'], mutates: false, summary: 'the tail of the agent log, or one operation\'s log' },
  doctor: { flags: ['deep'], options: ['service'], mutates: false, summary: 'check everything that can be wrong with this machine' },
  bundle: { flags: [], options: [], mutates: false, summary: 'a diagnostic bundle with secrets removed' },
  config: {
    flags: ['replace'],
    options: ['controller-id', 'revision'],
    mutates: true,
    summary: 'the stored setup document; "config set" replaces it from stdin, "config meta" reads its identity',
  },
  'self-update': {
    flags: ['stdin', 'install', 'rollback', 'check'],
    options: ['from', 'op'],
    mutates: true,
    summary: 'replace this agent from a signed bundle',
  },
  dispatch: { flags: [], options: [], mutates: true, summary: 'run one allowed command from SSH_ORIGINAL_COMMAND (restricted keys)' },
  'op-run': { flags: [], options: [], mutates: true, internal: true, summary: 'the detached worker for one operation' },
  version: { flags: ['check'], options: [], mutates: false, summary: 'the agent version, and with --check a self-test' },
  help: { flags: [], options: [], mutates: false, summary: 'print this list' },
};

const VALUE_OPTIONS = new Set([
  'service',
  'op',
  'expires',
  'budget-ms',
  'wait',
  'limit',
  'kind',
  'lines',
  'controller-id',
  'revision',
  'from',
]);

/** Options whose value is a path and therefore not held to the token grammar. */
const PATH_OPTIONS = new Set(['from']);

function emit(payload, exitCode) {
  process.stdout.write(`${JSON.stringify(payload)}\n`);
  process.exitCode = exitCode;
}

function parseArgs(argv) {
  const flags = new Set();
  const options = new Map();
  const positional = [];
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === '-h' || arg === '--help') {
      flags.add('help');
      continue;
    }
    if (!arg.startsWith('--')) {
      positional.push(arg);
      continue;
    }
    const body = arg.slice(2);
    const equals = body.indexOf('=');
    if (equals !== -1) {
      options.set(body.slice(0, equals), body.slice(equals + 1));
      continue;
    }
    if (VALUE_OPTIONS.has(body)) {
      // The value is the next argument, consumed here so it cannot be mistaken
      // for the command's own positional argument.
      options.set(body, argv[index + 1] ?? '');
      index += 1;
      continue;
    }
    flags.add(body);
  }
  return { flags, options, positional };
}

// ---------------------------------------------------------------------------
// The envelope
// ---------------------------------------------------------------------------

let systemIdentity = null;

function wrap(payload) {
  return envelope(payload, { agentVersion: AGENT_VERSION, system: systemIdentity ?? { id: platformName(), name: platformName() } });
}

function ok(payload) {
  return { payload: wrap({ ok: true, ...payload }), exitCode: 0 };
}

/**
 * A refusal. `exitCode` is 0 for the answers that are not errors — deferred,
 * queued, noop, conflict — because a client scripting against exit codes should
 * not have to treat "the machine is busy" as a failure.
 */
function refuse(reasonCode, message, extra = {}, { exitCode = 1 } = {}) {
  return { payload: wrap({ ok: false, reasonCode, message, ...extra }), exitCode };
}

function badArgument(message, { command = null, argument = null, accepts = null } = {}) {
  return refuse(REASON.badArgument, message, {
    command,
    argument,
    ...(accepts ? { accepts } : {}),
    error: message,
  });
}

/** Every mutating reply carries its action; these actions are answers, not errors. */
const NON_ERROR_ACTIONS = new Set(['deferred', 'queued', 'noop', 'conflict', 'accepted', 'armed', 'skipped', 'checked', 'already-running']);

function mutationReply(result, extra = {}) {
  const payload = wrap({
    ok: result.ok === true,
    reasonCode: result.reasonCode ?? null,
    message: result.message ?? '',
    action: result.action,
    ...(result.replayed === true ? { replayed: true } : {}),
    ...(Array.isArray(result.notes) && result.notes.length > 0 ? { notes: result.notes } : {}),
    ...extra,
  });
  const exitCode = result.ok === true || NON_ERROR_ACTIONS.has(result.action) ? 0 : 1;
  return { payload, exitCode };
}

// ---------------------------------------------------------------------------
// Argument validation
// ---------------------------------------------------------------------------

/**
 * Check every positional and every option value against the wire grammar.
 *
 * This is the one place it happens, and it happens before dispatch, so no
 * command can be added later that forgets. Path options are exempt because a
 * real path legitimately contains characters the grammar excludes; they are only
 * accepted from an unrestricted session, and the dispatcher refuses them.
 */
function validateArguments(command, positional, options) {
  for (const value of positional) {
    if (!isValidToken(value)) {
      return badArgument(
        `${JSON.stringify(value)} is not a valid argument: it must match ^[A-Za-z0-9][A-Za-z0-9._:@/\\~=+-]*$`,
        { command, argument: value },
      );
    }
  }
  for (const [name, value] of options) {
    if (PATH_OPTIONS.has(name)) continue;
    if (!isValidToken(value)) {
      return badArgument(
        `--${name} ${JSON.stringify(value)} is not a valid value: it must match ^[A-Za-z0-9][A-Za-z0-9._:@/\\~=+-]*$`,
        { command, argument: `--${name}` },
      );
    }
  }

  if (options.has('op') && !isValidOperationId(options.get('op'))) {
    return badArgument(`--op ${JSON.stringify(options.get('op'))} is not an operation id: 8 to 64 characters of a-z, 0-9 and dashes`, {
      command,
      argument: '--op',
    });
  }
  if (options.has('expires')) {
    const value = options.get('expires');
    if (!DURATION_PATTERN.test(value) && Number.isNaN(Date.parse(value))) {
      return badArgument(`--expires ${JSON.stringify(value)} is not a duration like 30m, 4h or 2d, nor an ISO timestamp`, {
        command,
        argument: '--expires',
      });
    }
  }
  for (const name of ['budget-ms', 'wait', 'limit', 'lines', 'revision']) {
    if (!options.has(name)) continue;
    const parsed = Number(options.get(name));
    if (!Number.isInteger(parsed) || parsed < 0) {
      return badArgument(`--${name} ${JSON.stringify(options.get(name))} is not a whole number`, { command, argument: `--${name}` });
    }
  }
  return null;
}

function integerOption(options, name, fallback, { max = null } = {}) {
  if (!options.has(name)) return fallback;
  const parsed = Number(options.get(name));
  if (!Number.isInteger(parsed) || parsed <= 0) return fallback;
  return max === null ? parsed : Math.min(parsed, max);
}

// ---------------------------------------------------------------------------
// Shared option handling for the mutating verbs
// ---------------------------------------------------------------------------

/**
 * Everything the operation wrapper needs from the command line.
 *
 * `--op` is what makes a retry safe: a client whose link dropped sends the same
 * id and gets the first answer instead of starting the work twice.
 */
function operationOptions(flags, options) {
  return {
    force: flags.has('force'),
    opId: options.get('op') ?? null,
    initiator: currentInitiator(),
    mode: flags.has('force') ? 'force' : 'manual',
  };
}

function queueTtl(options) {
  if (!options.has('expires')) return { ok: true, ttlMs: null, expiresAt: null };
  const value = options.get('expires');
  const ms = parseDuration(value);
  if (ms !== null) return { ok: true, ttlMs: ms, expiresAt: null };
  const resolved = resolveDeadline(value);
  if (!resolved.ok) return { ok: false, error: resolved.error };
  return { ok: true, ttlMs: null, expiresAt: resolved.at };
}

/** The keys a mutating reply carries whatever its outcome, per command. */
function queuedShape(kind, { service, target, actionId }) {
  if (kind === 'update') return { service, from: null, to: null };
  if (kind === 'restart') return { service };
  if (kind === 'boot') return { target, armed: false };
  if (kind === 'run') return { id: actionId, exitCode: null, output: '' };
  return {};
}

/** Park a request until the machine is idle, and answer `queued`. */
function queueRequest({ kind, service = null, target = null, actionId = null, flags, options, config }) {
  const ttl = queueTtl(options);
  if (!ttl.ok) return badArgument(ttl.error, { command: kind, argument: '--expires' });

  const queued = queueOperation({
    id: options.get('op') ?? newOperationId(),
    kind,
    service,
    target,
    actionId,
    force: flags.has('force'),
    systemId: config.system.id,
    noReboot: flags.has('no-reboot'),
    ttlMs: ttl.ttlMs,
    expiresAt: ttl.expiresAt,
  });

  if (queued.conflict) {
    return refuse(queued.reasonCode ?? REASON.badArgument, queued.error, { action: 'conflict', op: wireRecord(queued.record) }, { exitCode: 0 });
  }
  if (!queued.ok) {
    return refuse(REASON.internal, `the request could not be recorded: ${queued.error}`, { action: 'failed' });
  }
  if (queued.replay) {
    const record = queued.record;
    return mutationReply({
      ...(record.state === 'finished' ? record.result : {
        ok: true,
        action: record.state === 'queued' ? 'queued' : 'accepted',
        message: `this request is already ${record.state} as ${record.id}`,
      }),
      replayed: true,
    }, {
      ...queuedShape(kind, { service, target, actionId }),
      ...(kind === 'boot' ? { armed: ['armed', 'rebooting', 'rebooted'].includes(record.result?.action) } : {}),
      ...(kind === 'update' ? { from: record.result?.from ?? null, to: record.result?.to ?? null } : {}),
      ...(kind === 'run' ? { exitCode: record.result?.exitCode ?? null, output: record.result?.output ?? '' } : {}),
      op: wireRecord(record),
    });
  }

  const what = service ?? target ?? actionId ?? kind;
  const verb = kind === 'boot' && flags.has('no-reboot') ? 'armed without rebooting' : { update: 'updated', restart: 'restarted', run: 'run', boot: 'booted', sleep: 'suspended' }[kind] ?? kind;
  return ok({
    action: 'queued',
    message: `${what} will be ${verb} on the next idle cycle, or dropped at ${queued.record.expiresAt}`,
    // Every mutating reply keeps its command's shape, whatever the answer is, so
    // a client decodes one thing per verb rather than one per outcome.
    ...queuedShape(kind, { service, target, actionId }),
    op: wireRecord(queued.record),
    ...(queued.replaced ? { replaced: queued.replaced } : {}),
  });
}

/**
 * Start the work in a detached process and answer `accepted`.
 *
 * Windows sshd kills the children of a session when the session ends, so a
 * detached spawn there has to go through WMI, which creates the process outside
 * the session's job. If either route fails the work is run synchronously instead
 * and the reply says `detached: false` — a slow answer beats a lost one.
 */
function detachOperation(record) {
  return spawnDetached({ entry: fileURLToPath(new URL('./index.mjs', import.meta.url)), id: record.id, home: basePath() });
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

async function commandStatus(loaded, options) {
  const budgetMs = integerOption(options, 'budget-ms', DEFAULT_STATUS_BUDGET_MS, { max: 120000 });
  const status = await buildStatus(loaded, { budgetMs });
  return { payload: wrap(status), exitCode: status.ok ? 0 : 1 };
}

async function commandBusy(config, options) {
  const budgetMs = integerOption(options, 'budget-ms', 24000, { max: 120000 });
  const liveness = {};
  for (const service of config.services) liveness[service.id] = livenessOf(service);
  const { entries } = await checkAllBusy(config, { budgetMs, liveness });
  const aggregate = aggregateBusy(entries);

  // One service is the common case, and its own busy object carries far more
  // detail than the aggregate could, so it is passed straight through rather
  // than repeated under a second key.
  // A machine with NO services answers in the same shape rather than with an
  // empty list, because "there is nothing here to be busy with" is an answer
  // about the machine, not about a service.
  if (entries.length <= 1) {
    const only =
      entries.length === 1
        ? entries[0].busy
        : {
            busy: false,
            unknown: false,
            monitored: false,
            reason: 'no services are configured',
            evidence: 'none',
            checkedAt: new Date().toISOString(),
            elapsedMs: 0,
            error: null,
          };
    return ok({
      ...only,
      monitoredServices: aggregate.monitoredServices,
      unmonitoredServices: aggregate.unmonitoredServices,
    });
  }
  return ok({
    busy: aggregate.busy,
    unknown: aggregate.unknown,
    reason: aggregate.reason,
    monitoredServices: aggregate.monitoredServices,
    unmonitoredServices: aggregate.unmonitoredServices,
    services: entries.map((entry) => ({ id: entry.service.id, name: entry.service.name, ...entry.busy })),
  });
}

/** A list a person can read: "a, b and c". */
function listOf(values) {
  if (values.length === 0) return 'none';
  if (values.length === 1) return values[0];
  return `${values.slice(0, -1).join(', ')} and ${values.at(-1)}`;
}

function missingService(config, id, command = 'update') {
  const known = config.services.map((service) => service.id);
  return refuse(
    REASON.unknownService,
    known.length === 0
      ? 'this machine has no services configured'
      : `this machine has no service called ${JSON.stringify(id ?? '')}; it has ${listOf(known)}`,
    { command, argument: '--service', accepts: known, error: `unknown service: ${id ?? '(none given)'}` },
  );
}

async function commandUpdate(config, flags, options) {
  const service = selectService(config, options.get('service'));
  if (!service) return missingService(config, options.get('service'));

  if (flags.has('when-idle')) {
    return queueRequest({ kind: 'update', service: service.id, flags, options, config });
  }

  const shared = operationOptions(flags, options);
  if (flags.has('detach')) return detachedRun(config, { kind: 'update', service: service.id, ...shared });

  const result = await runUpdate(config, service, shared);
  return mutationReply(result, {
    service: service.id,
    from: result.from ?? null,
    to: result.to ?? null,
    op: result.op ? wireRecord(readOperation(result.opId)) : null,
    ...(result.verified !== undefined ? { verified: result.verified } : {}),
    ...(result.replayed ? { replayed: true } : {}),
    ...(result.conflict ? { conflict: result.conflict } : {}),
  });
}

async function commandRestart(config, flags, options) {
  const service = selectService(config, options.get('service'));
  if (!service) return missingService(config, options.get('service'));

  if (flags.has('when-idle')) {
    return queueRequest({ kind: 'restart', service: service.id, flags, options, config });
  }

  const shared = operationOptions(flags, options);
  if (flags.has('detach')) return detachedRun(config, { kind: 'restart', service: service.id, ...shared });

  const result = await runRestart(config, service, shared);
  return mutationReply(result, {
    service: service.id,
    op: result.opId ? wireRecord(readOperation(result.opId)) : null,
    ...(result.replayed ? { replayed: true } : {}),
    ...(result.conflict ? { conflict: result.conflict } : {}),
  });
}

async function commandBoot(config, positional, flags, options) {
  const target = positional[0] ?? null;
  const known = Object.keys(config.boot?.targets ?? {});
  // An unknown target is a refusal about the argument, answered in the error
  // shape, rather than a boot reply that pretends something was attempted.
  // Booting into the system already running is a real no-op, and stays one.
  if (target !== config.system.id && (!target || !known.includes(target))) {
    return refuse(
      known.length === 0 ? REASON.notConfigured : REASON.unknownTarget,
      known.length === 0
        ? `${config.system.name} has no boot targets configured, so there is no other system to point it at`
        : `this machine has no boot target called ${JSON.stringify(target ?? '')}; it can boot into ${listOf(known)}`,
      { command: 'boot', argument: target ?? null, accepts: known, error: `unknown boot target: ${target ?? '(none given)'}` },
    );
  }

  if (flags.has('when-idle')) {
    if (!target) return badArgument('boot --when-idle needs a target', { command: 'boot' });
    return queueRequest({ kind: 'boot', target, flags, options, config });
  }

  const result = await runBoot(config, target, { ...operationOptions(flags, options), noReboot: flags.has('no-reboot') });
  return mutationReply(result, {
    target: result.target ?? target ?? null,
    armed: result.armed === true,
    op: result.opId ? wireRecord(readOperation(result.opId)) : null,
    ...(result.replayed ? { replayed: true } : {}),
    ...(result.conflict ? { conflict: result.conflict } : {}),
  });
}

async function commandSleep(config, flags, options) {
  if (flags.has('when-idle')) return queueRequest({ kind: 'sleep', flags, options, config });

  const result = await runSleep(config, operationOptions(flags, options));
  return mutationReply(result, {
    op: result.opId ? wireRecord(readOperation(result.opId)) : null,
    ...(result.replayed ? { replayed: true } : {}),
    ...(result.conflict ? { conflict: result.conflict } : {}),
  });
}

async function commandRun(config, positional, flags, options) {
  const id = positional[0] ?? null;
  const known = config.actions.map((action) => action.id);
  if (!id || !known.includes(id)) {
    return refuse(
      REASON.unknownAction,
      known.length === 0
        ? 'this machine has no actions configured'
        : `this machine has no action called ${JSON.stringify(id ?? '')}; it has ${listOf(known)}`,
      { command: 'run', argument: id ?? null, accepts: known, error: `unknown action: ${id ?? '(none given)'}` },
    );
  }
  if (flags.has('when-idle')) {
    if (!id) return badArgument('run --when-idle needs an action id', { command: 'run' });
    return queueRequest({ kind: 'run', actionId: id, flags, options, config });
  }

  const shared = operationOptions(flags, options);
  if (flags.has('detach')) return detachedRun(config, { kind: 'run', actionId: id, ...shared });

  const result = await runAction(config, id, shared);
  return mutationReply(result, {
    id: result.id ?? id ?? null,
    exitCode: result.exitCode ?? null,
    output: result.output ?? '',
    detached: false,
    op: result.opId ? wireRecord(readOperation(result.opId)) : null,
    ...(result.replayed ? { replayed: true } : {}),
    ...(result.conflict ? { conflict: result.conflict } : {}),
  });
}

async function commandCycle(config, flags, options) {
  const result = await runCycle(config, {
    force: flags.has('force'),
    dryRun: flags.has('dry-run'),
    opId: options.get('op') ?? null,
  });

  if (flags.has('dry-run')) {
    return ok({
      action: 'cycled',
      dryRun: true,
      message: `${result.children.filter((child) => child.wouldRun).length} of ${result.children.length} services would be updated now`,
      op: null,
      plan: result.children.map((child) => ({
        service: child.service,
        wouldRun: child.wouldRun,
        reasonCode: child.reasonCode ?? null,
        detail: child.message,
      })),
      queued: result.queued,
    });
  }

  return mutationReply(result, {
    dryRun: false,
    op: result.opId ? wireRecord(readOperation(result.opId)) : null,
    children: (result.children ?? []).map((child) => ({
      opId: child.opId ?? null,
      service: child.service ?? null,
      action: child.action ?? null,
      reasonCode: child.reasonCode ?? null,
    })),
    queued: (result.queue ?? []).map((entry) => ({
      opId: entry.opId ?? null,
      service: entry.service ?? null,
      action: entry.action ?? null,
      reasonCode: entry.reasonCode ?? null,
    })),
    recovered: (result.recovery?.interrupted ?? []).map((entry) => ({
      opId: entry.id,
      service: entry.service ?? null,
      action: entry.action ?? null,
      reasonCode: entry.reasonCode ?? null,
    })),
    ...(result.conflict ? { conflict: result.conflict } : {}),
  });
}

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

/**
 * The per-service policy block, projected to what the contract names.
 *
 * `describeServiceUpdates` answers more than the wire carries, because the cycle
 * and status need different parts of it. Projecting explicitly means a field
 * added for one caller cannot leak into a reply another one validates.
 */
function policyUpdates(described, service) {
  const own = service?.updates ?? {};
  return {
    automatic: described.automatic,
    inherited: described.inherited,
    pauseUntil: described.pauseUntil,
    maintenanceWindows: own.maintenanceWindows ?? null,
    eligibleNow: described.eligibleNow,
    deferredReason: described.deferredReason,
    // Which of the three came from the machine rather than the service. A UI
    // offering "inherit" per row cannot derive this from the effective values.
    inheritedKeys: {
      automatic: own.automatic === null || own.automatic === undefined,
      pauseUntil: own.pauseUntil === null || own.pauseUntil === undefined,
      maintenanceWindows: own.maintenanceWindows === null || own.maintenanceWindows === undefined,
    },
    inWindowNow: described.eligibleNow || described.deferredReason !== 'outside-window',
    nextWindow: described.nextWindow ?? null,
  };
}

function policyReply(config, serviceId, { message = null, changed = [] } = {}) {
  if (!serviceId) {
    return ok({
      ...(message ? { message } : {}),
      service: null,
      updates: describeSystemUpdates(config, {}),
      autoUpdate: config.updates.automatic === true,
      changed,
      // A machine-wide read carries every service's effective policy too, so a
      // client does not need one round trip per service to render the page.
      services: config.services.map((service) => ({
        id: service.id,
        updates: policyUpdates(describeServiceUpdates(config, service, { state: serviceState(service, { readOnly: true }) }), service),
      })),
    });
  }

  const service = config.services.find((entry) => entry.id === serviceId);
  if (!service) return missingService(config, serviceId);
  return ok({
    ...(message ? { message } : {}),
    service: serviceId,
    updates: policyUpdates(describeServiceUpdates(config, service, { state: serviceState(service, { readOnly: true }) }), service),
    autoUpdate: config.updates.automatic === true,
    changed,
  });
}

async function commandPolicy(config, positional, options) {
  const serviceId = options.get('service') ?? null;
  if (serviceId && !config.services.some((service) => service.id === serviceId)) return missingService(config, serviceId);

  const sub = positional[0];
  if (sub === undefined) return policyReply(config, serviceId);
  if (sub !== 'set') {
    return badArgument(`policy takes no argument, or "set"; got ${JSON.stringify(sub)}`, { command: 'policy', argument: sub });
  }

  const text = await readStdin();
  if (text === null || text.trim().length === 0) {
    return badArgument('policy set reads a JSON patch from stdin, and nothing arrived', { command: 'policy' });
  }
  let raw;
  try {
    raw = JSON.parse(text);
  } catch (err) {
    return badArgument(`the policy patch does not parse: ${err.message}`, { command: 'policy' });
  }

  const validated = validatePolicyPatch(raw, { perService: Boolean(serviceId) });
  if (!validated.ok) {
    return refuse(
      REASON.badArgument,
      `the policy patch is not usable: ${validated.errors.map((entry) => `${entry.path}: ${entry.message}`).join('; ')}`,
      { command: 'policy', problems: validated.errors, error: 'invalid policy patch' },
    );
  }

  const patch = validated.patch;
  const changed = Object.keys(patch);
  if (changed.length === 0) return policyReply(config, serviceId, { message: 'nothing in the patch changed anything' });

  // A pause set from a client is operational, not a change to the document the
  // machine's owner wrote, so it lands in state rather than rewriting config.json
  // — except at the machine level, where there is no per-service state to hold it.
  const saved = saveConfig((stored) => {
    const next = { ...stored };
    if (serviceId) {
      const services = Array.isArray(next.services) ? next.services.map((entry) => ({ ...entry })) : [];
      const index = services.findIndex((entry) => entry.id === serviceId);
      if (index === -1) return next;
      const updates = { ...(services[index].updates ?? {}) };
      for (const [key, value] of Object.entries(patch)) {
        if (value === null) delete updates[key];
        else updates[key] = value;
      }
      services[index] = { ...services[index], updates };
      next.services = services;
      return next;
    }
    const updates = { ...(next.updates ?? {}) };
    for (const [key, value] of Object.entries(patch)) {
      // At the machine level there is nothing to inherit from, so a null clears
      // the setting rather than meaning "inherit".
      if (key === 'pauseUntil' && value === null) updates.pauseUntil = null;
      else if (key === 'maintenanceWindows' && value === null) updates.maintenanceWindows = [];
      else updates[key] = value;
    }
    next.updates = updates;
    // The 2.x spelling is kept in step so an older client reading the file sees
    // the same answer.
    if ('automatic' in patch) next.autoUpdate = patch.automatic;
    return next;
  });

  if (!saved.ok) {
    return refuse(REASON.configInvalid, saved.error ?? 'the policy could not be written', {
      command: 'policy',
      problems: saved.errors ?? [],
      error: saved.error,
    });
  }

  const described = serviceId
    ? `updates for ${serviceId}`
    : 'updates for this machine';
  const summary = patch.pauseUntil
    ? `${described} are paused until ${patch.pauseUntil}`
    : patch.automatic === false
      ? `automatic ${described} are off`
      : patch.automatic === true
        ? `automatic ${described} are on`
        : `${described} were changed`;
  log(`policy set: ${changed.join(', ')}${serviceId ? ` for ${serviceId}` : ''}`, 'policy');
  return policyReply(saved.config, serviceId, { message: summary, changed });
}

function commandAutoUpdate(config, positional, options) {
  const value = positional[0];
  const serviceId = options.get('service') ?? null;
  if (serviceId && !config.services.some((service) => service.id === serviceId)) return missingService(config, serviceId);

  const apply = (patch, message) => {
    const saved = saveConfig((stored) => {
      const next = { ...stored };
      if (serviceId) {
        const services = Array.isArray(next.services) ? next.services.map((entry) => ({ ...entry })) : [];
        const index = services.findIndex((entry) => entry.id === serviceId);
        if (index === -1) return next;
        services[index] = { ...services[index], updates: { ...(services[index].updates ?? {}), ...patch } };
        next.services = services;
        return next;
      }
      next.updates = { ...(next.updates ?? {}), ...patch };
      if ('automatic' in patch) next.autoUpdate = patch.automatic;
      return next;
    });
    if (!saved.ok) {
      return refuse(REASON.configInvalid, saved.error ?? 'config.json could not be written', {
        command: 'auto-update',
        error: saved.error,
      });
    }
    log(message, 'auto-update');
    return policyReply(saved.config, serviceId, { message, changed: Object.keys(patch) });
  };

  if (value === 'on' || value === 'off') {
    return apply(
      { automatic: value === 'on' },
      `automatic updates are ${value} for ${serviceId ?? 'this machine'}`,
    );
  }
  if (value === 'resume') {
    return apply({ pauseUntil: null }, `updates for ${serviceId ?? 'this machine'} are no longer paused`);
  }
  if (value === 'pause') {
    const duration = positional[1];
    if (!duration) {
      return badArgument('auto-update pause needs a duration like 4h, or an ISO timestamp', { command: 'auto-update' });
    }
    const resolved = resolveDeadline(duration);
    if (!resolved.ok) return badArgument(resolved.error, { command: 'auto-update', argument: duration });
    return apply({ pauseUntil: resolved.at }, `updates for ${serviceId ?? 'this machine'} are paused until ${resolved.at}`);
  }

  return badArgument('auto-update takes on, off, "pause <duration>" or resume', {
    command: 'auto-update',
    argument: value ?? null,
    accepts: ['on', 'off', 'pause', 'resume'],
  });
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

async function commandOp(config, positional, options) {
  const id = positional[0];
  if (!id) return badArgument('op takes one operation id', { command: 'op' });
  if (!isValidOperationId(id)) return badArgument(`${JSON.stringify(id)} is not an operation id`, { command: 'op', argument: id });

  const waitSeconds = integerOption(options, 'wait', 0, { max: 120 });
  const deadline = Date.now() + waitSeconds * 1000;
  const startedAt = Date.now();

  for (;;) {
    const record = readOperation(id);
    if (!record) {
      return refuse(REASON.badArgument, `there is no operation ${id} on this machine`, {
        id,
        op: null,
        command: 'op',
        error: `unknown operation ${id}`,
      });
    }
    // Derived, not written: status and op both promise to change nothing, and a
    // record whose process is plainly gone must not be reported as still running.
    const derived = deriveState(record, { systemId: config.system.id });
    if (derived.state === 'finished' || Date.now() >= deadline) {
      return ok({
        id,
        op: wireRecord(derived),
        ...(waitSeconds > 0
          ? { waited: { requestedSeconds: waitSeconds, elapsedMs: Date.now() - startedAt, finished: derived.state === 'finished' } }
          : {}),
      });
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
}

function commandCancel(positional) {
  const id = positional[0];
  if (!id) return badArgument('cancel takes one operation id', { command: 'cancel' });
  if (!isValidOperationId(id)) return badArgument(`${JSON.stringify(id)} is not an operation id`, { command: 'cancel', argument: id });

  const result = cancelOperation(id);
  return mutationReply(result, { id, op: result.op ? wireRecord(readOperation(id)) : null });
}

function commandHistory(config, options) {
  const limit = integerOption(options, 'limit', 30, { max: 200 });
  const service = options.get('service') ?? null;
  const kind = options.get('kind') ?? null;
  const records = listOperations({ limit, service, kind }).map((record) => deriveState(record, { systemId: config?.system.id ?? null }));
  const retained = listOperations({ limit: 200 }).length;
  return ok({
    operations: records.map(summarize),
    limit,
    returned: records.length,
    retained,
    filter: { service, kind },
  });
}

function commandLogs(options) {
  const requested = integerOption(options, 'lines', 100, { max: 500 });
  const opId = options.get('op') ?? null;

  if (opId) {
    if (!isValidOperationId(opId)) return badArgument(`${JSON.stringify(opId)} is not an operation id`, { command: 'logs', argument: '--op' });
    const record = readOperation(opId);
    if (!record) {
      return refuse(REASON.badArgument, `there is no operation ${opId} on this machine`, {
        command: 'logs',
        error: `unknown operation ${opId}`,
      });
    }
    const lines = (record.log ?? []).slice(-requested);
    return ok({
      source: 'operation',
      opId,
      path: null,
      requested,
      returned: lines.length,
      truncated: (record.log ?? []).length > lines.length,
      lines: lines.map((entry) => ({ at: entry.at ?? null, command: record.kind, line: entry.line })),
    });
  }

  const read = readLogLines({ lines: requested });
  return ok({
    source: 'agent',
    opId: null,
    path: read.path,
    requested,
    returned: read.lines.length,
    truncated: read.truncated,
    lines: read.lines,
  });
}

// ---------------------------------------------------------------------------
// The controller document
// ---------------------------------------------------------------------------

/**
 * Everything on stdin, as bytes.
 *
 * A terminal with nothing piped into it would block forever, and the contract is
 * one JSON object and out, so an interactive `config set` is answered at once
 * rather than waited on. The read is capped for the same reason the document is.
 */
function readStdinBytes(limit = MAX_CONTROLLER_BYTES) {
  if (process.stdin.isTTY) return Promise.resolve(null);
  return new Promise((resolve) => {
    const chunks = [];
    let size = 0;
    let settled = false;
    const finish = (value) => {
      if (settled) return;
      settled = true;
      resolve(value);
    };
    process.stdin.on('data', (chunk) => {
      chunks.push(chunk);
      size += chunk.length;
      if (size > limit) {
        process.stdin.destroy();
        finish(Buffer.concat(chunks));
      }
    });
    process.stdin.on('end', () => finish(Buffer.concat(chunks)));
    process.stdin.on('error', () => finish(null));
  });
}

async function readStdin() {
  const bytes = await readStdinBytes();
  return bytes === null ? null : bytes.toString('utf8');
}

async function commandConfig(positional, flags, options) {
  const sub = positional[0];

  if (sub === undefined) {
    const stored = readController();
    const payload = {
      controller: stored.document,
      hash: stored.hash,
      bytes: stored.raw ? Buffer.byteLength(stored.raw, 'utf8') : 0,
      meta: stored.meta,
    };
    // A file that is there but unreadable is worth saying out loud: the hash is
    // still reported, so a client will push over it, and until then nobody
    // should be left guessing why there are no machines.
    if (stored.error) payload.error = stored.error;
    return ok(payload);
  }

  if (sub === 'meta') {
    const meta = readControllerMeta();
    return ok({ meta, hash: meta.hash });
  }

  if (sub !== 'set') {
    return badArgument(`config takes no argument, "set" or "meta"; got ${JSON.stringify(sub)}`, { command: 'config', argument: sub });
  }

  const bytes = await readStdinBytes();
  if (bytes === null || bytes.length === 0) {
    return refuse(REASON.badArgument, 'no document arrived on stdin', {
      hash: null,
      bytes: 0,
      id: options.get('controller-id') ?? null,
      revision: options.has('revision') ? Number(options.get('revision')) : null,
      error: 'no document on stdin',
    });
  }

  const id = options.get('controller-id') ?? null;
  const revision = options.has('revision') ? Number(options.get('revision')) : null;

  // The setup document is machine-wide state, so replacing it is serialised
  // against every other mutation. Without this a `config set` landing in the
  // middle of an install could be read half-written by the update that is
  // running, and two concurrent pushes could interleave their revisions.
  const lock = acquireOperationLock({ kind: 'config', target: 'config set' });
  if (!lock.ok) {
    return refuse(lock.reasonCode ?? REASON.operationInProgress, lock.message, {
      hash: null,
      bytes: 0,
      id,
      revision,
      error: lock.message,
      ...(lock.conflictDetail ? { conflict: lock.conflictDetail } : {}),
    }, { exitCode: 0 });
  }

  try {
    const stored = storeController(bytes, { id, revision, replace: flags.has('replace') });
    if (!stored.ok) {
      log(`config set refused: ${stored.error}`, 'config');
      return refuse(stored.reasonCode ?? REASON.badArgument, stored.error, {
        hash: stored.hash ?? null,
        bytes: stored.bytes ?? 0,
        id,
        revision,
        ...(stored.divergent ? { divergent: true } : {}),
        ...(stored.current ? { current: stored.current } : {}),
        error: stored.error,
      });
    }

    const action = stored.action ?? 'stored';
    const message =
      action === 'noop'
        ? `this machine already holds these exact bytes at revision ${stored.meta.revision}; nothing was written`
        : action === 'replaced'
          ? `replaced the setup on this machine with revision ${stored.meta.revision} of ${stored.meta.id}`
          : `stored revision ${stored.meta.revision} from ${stored.meta.source}, ${stored.bytes} bytes`;
    log(`config set: ${action}, ${stored.bytes} bytes, ${stored.hash.slice(0, 12)}`, 'config');
    return ok({
      message,
      action,
      hash: stored.hash,
      bytes: stored.bytes,
      id: stored.meta.id,
      revision: stored.meta.revision,
      source: stored.meta.source,
      ...(action === 'replaced' ? { replaced: true } : {}),
    });
  } finally {
    lock.lock.release();
  }
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

async function commandDoctor(loaded, flags, options) {
  const result = await runDoctor(loaded, { service: options.get('service') ?? null, deep: flags.has('deep') });
  return { payload: wrap(result), exitCode: result.ok ? 0 : 1 };
}

async function commandBundle(loaded) {
  const raw = readJsonDocument(loaded.path ?? configPath());
  const doctor = await runDoctor(loaded, { deep: false });
  const status = await buildStatus(loaded, { budgetMs: DEFAULT_STATUS_BUDGET_MS });
  const history = commandHistory(loaded.config, new Map()).payload;
  const logs = commandLogs(new Map([['lines', '200']])).payload;
  const bundle = await buildBundle({
    doctor: wrap(doctor),
    status: wrap(status),
    history,
    logs,
    config: loaded.config,
    rawConfig: raw.state === 'ok' ? raw.value : null,
  });
  return ok(bundle);
}

// ---------------------------------------------------------------------------
// Self-update
// ---------------------------------------------------------------------------

async function commandSelfUpdate(config, flags, options) {
  const modes = ['check', 'rollback', 'install'].filter((flag) => flags.has(flag));
  if (modes.length > 1 || (flags.has('install') && (flags.has('stdin') || options.has('from')))) {
    return badArgument('self-update --check, --rollback and --install are separate modes; --install reads its own staged tree', { command: 'self-update' });
  }
  if ((flags.has('stdin') && options.has('from')) || (flags.has('rollback') && (flags.has('stdin') || options.has('from')))) {
    return badArgument('self-update accepts one bundle source; --rollback does not take a bundle', { command: 'self-update' });
  }

  if (flags.has('check')) {
    const result = await checkBundle({ from: options.get('from') ?? null, stdin: flags.has('stdin') });
    return mutationReply(result, {
      current: { version: AGENT_VERSION, contract: CONTRACT_VERSION },
      staged: null,
      previous: null,
      manifest: result.manifest,
      selfTest: null,
      op: null,
      available: result.ok,
    });
  }

  if (flags.has('rollback')) {
    const result = await runRollback(config, operationOptions(flags, options));
    return mutationReply(result, {
      current: result.current,
      staged: result.staged ?? null,
      previous: result.previous ?? null,
      manifest: result.manifest ?? null,
      selfTest: result.selfTest ?? null,
      op: result.opId ? wireRecord(readOperation(result.opId)) : null,
    });
  }

  if (flags.has('install')) {
    const result = await runBootstrapInstall(config, operationOptions(flags, options));
    return mutationReply(result, {
      current: result.current,
      staged: result.staged ?? null,
      previous: result.previous ?? null,
      manifest: result.manifest ?? null,
      selfTest: result.selfTest ?? null,
      op: result.opId ? wireRecord(readOperation(result.opId)) : null,
    });
  }

  const from = options.get('from') ?? null;
  if (!from && !flags.has('stdin')) {
    return badArgument('self-update needs --from PATH, --stdin, --check or --rollback', { command: 'self-update' });
  }

  const result = await runSelfUpdate(config, { from, stdin: flags.has('stdin'), ...operationOptions(flags, options) });
  return mutationReply(result, {
    current: result.current,
    staged: result.staged ?? null,
    previous: result.previous ?? null,
    manifest: result.manifest ?? null,
    selfTest: result.selfTest ?? null,
    op: result.opId ? wireRecord(readOperation(result.opId)) : null,
  });
}

// ---------------------------------------------------------------------------
// Detached work
// ---------------------------------------------------------------------------

/** Open the record, hand it to a detached worker, and answer `accepted`. */
async function detachedRun(config, { kind, service = null, target = null, actionId = null, opId, force, mode }) {
  const id = opId ?? newOperationId();
  const opened = beginOperation({ id, kind, service, target, actionId, force, mode,
    systemId: config.system.id, detached: true, queueRequested: false });
  if (!opened.ok) {
    return refuse(opened.reasonCode ?? REASON.internal, opened.error, {
      action: opened.conflict ? 'conflict' : 'failed', service, op: wireRecord(opened.record),
    });
  }
  if (opened.replay) {
    const existing = opened.record;
    if (existing.state === 'finished') {
      return mutationReply({ ...existing.result, action: existing.result?.action ?? 'failed' }, {
        service, op: wireRecord(existing), replayed: true,
      });
    }
    return ok({ action: 'accepted', reasonCode: REASON.alreadyRunning,
      message: `operation ${id} is already ${existing.state}`, service,
      detached: existing.detached === true, op: wireRecord(existing), replayed: true });
  }

  updateOperation(id, { launchPendingUntil: new Date(Date.now() + 30000).toISOString() });
  const started = await detachOperation(opened.record);
  if (!started.ok) {
    if (started.uncertain) {
      return ok({ action: 'accepted', reasonCode: REASON.timedOut,
        message: `the worker launch could not be confirmed: ${started.error}; query operation ${id} before retrying`,
        service, detached: true, op: wireRecord(readOperation(id)) });
    }
    log(`could not detach ${kind}: ${started.error}; running it in this session instead`, kind);
    updateOperation(id, { detached: false });
    return { synchronous: { id, kind, service, target, actionId, force, mode } };
  }
  updateOperation(id, (record) => record.state === 'running' && record.pid === process.pid ? { pid: started.pid } : {});
  return ok({ action: 'accepted',
    message: `${kind} accepted as operation ${id}; ask "op ${id} --wait 20" for the result`,
    service, detached: true, op: wireRecord(readOperation(id)) });
}

/** The detached worker. Never called by a client. */
async function commandOpRun(config, positional) {
  const id = positional[0];
  if (!id || !isValidOperationId(id)) return badArgument('op-run takes one operation id', { command: 'op-run' });
  const record = readOperation(id);
  if (!record) return badArgument(`there is no operation ${id}`, { command: 'op-run' });

  const shared = { existingOpId: id, force: record.force === true, noReboot: record.noReboot === true, mode: record.mode ?? 'manual' };
  let result;
  if (record.kind === 'update' || record.kind === 'restart') {
    const service = config.services.find((entry) => entry.id === record.service);
    if (!service) return missingService(config, record.service);
    result = record.kind === 'update' ? await runUpdate(config, service, shared) : await runRestart(config, service, shared);
  } else if (record.kind === 'run') {
    result = await runAction(config, record.actionId, shared);
  } else if (record.kind === 'boot') {
    result = await runBoot(config, record.target, shared);
  } else if (record.kind === 'sleep') {
    result = await runSleep(config, shared);
  } else {
    return badArgument(`a ${record.kind} operation cannot be run detached`, { command: 'op-run' });
  }
  return mutationReply(result, { op: wireRecord(readOperation(id)) });
}

// ---------------------------------------------------------------------------
// Help and version
// ---------------------------------------------------------------------------

function helpPayload(error) {
  const payload = {
    ok: !error,
    usage: 'node index.mjs <command> [arguments] [--flags]',
    commands: Object.entries(COMMANDS)
      .filter(([, meta]) => !meta.internal)
      .map(([name, meta]) => ({
        name,
        flags: [...meta.flags.map((flag) => `--${flag}`), ...meta.options.map((option) => `--${option} <value>`)],
        // Which commands change the machine, so a client can grey out the rest
        // when it is talking to a machine whose configuration is broken.
        mutates: meta.mutates === true,
        summary: meta.summary,
      })),
  };
  if (error) payload.error = error;
  return payload;
}

/**
 * `version --check` is the self-test a staged agent has to pass before it
 * replaces a working one. It loads application modules and separately parses
 * child workers without executing their entry points. It changes nothing.
 */
async function commandVersion(flags, loaded) {
  const payload = {
    node: process.versions.node,
    base: basePath(),
    os: platformName(),
  };
  if (flags.has('check')) {
    // Application modules are imported for real. A verified bundle that then
    // cannot start is exactly what this catches, while the working agent is
    // still in place — which is why self-update runs it on the staged tree
    // before swapping anything.
    const modules = [
      'actions.mjs', 'agent-swap.mjs', 'archive.mjs', 'boot.mjs', 'config.mjs', 'contract.mjs', 'controller.mjs',
      'dispatch.mjs', 'doctor.mjs', 'endpoint.mjs', 'http.mjs', 'lock.mjs', 'log.mjs', 'mutex.mjs',
      'operate.mjs', 'operations.mjs', 'policy.mjs', 'power.mjs', 'probes/busy.mjs',
      'probes/health.mjs', 'probes/process.mjs', 'probes/relay.mjs', 'probes/t3-sqlite.mjs',
      'providers/app.mjs', 'providers/command.mjs', 'providers/npm.mjs', 'scheduler.mjs',
      'selfupdate.mjs', 'service.mjs', 'state.mjs', 'status.mjs', 'trust.mjs', 'update.mjs',
    ];
    const failed = [];
    let loadedCount = 0;
    for (const name of modules) {
      try {
        await import(`./${name}`);
        loadedCount += 1;
      } catch (err) {
        failed.push(`${name}: ${err.message}`);
      }
    }
    const workers = checkWorkerModules(fileURLToPath(new URL('.', import.meta.url)));
    if (!workers.ok) failed.push(`worker modules: ${workers.error}`);
    else loadedCount += 2;
    // The configuration is read but never written: a self-test that changed the
    // machine it is testing would be worse than no self-test.
    if (!loaded.ok && loaded.config === null) failed.push(`config.json: ${loaded.error}`);
    payload.selfTest = { ok: failed.length === 0, modules: loadedCount, failed };
    if (failed.length > 0) return refuse(REASON.internal, 'agent self-test failed', payload);
  }
  return ok(payload);
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

async function dispatch(command, positional, flags, options, loaded) {
  const config = loaded.config;
  switch (command) {
    case 'status':
      return commandStatus(loaded, options);
    case 'busy':
      return commandBusy(config, options);
    case 'update':
      return commandUpdate(config, flags, options);
    case 'restart':
      return commandRestart(config, flags, options);
    case 'boot':
      return commandBoot(config, positional, flags, options);
    case 'sleep':
      return commandSleep(config, flags, options);
    case 'run':
      return commandRun(config, positional, flags, options);
    case 'cycle':
      return commandCycle(config, flags, options);
    case 'auto-update':
      return commandAutoUpdate(config, positional, options);
    case 'policy':
      return commandPolicy(config, positional, options);
    case 'op':
      return commandOp(config, positional, options);
    case 'cancel':
      return commandCancel(positional);
    case 'history':
      return commandHistory(config, options);
    case 'logs':
      return commandLogs(options);
    case 'doctor':
      return commandDoctor(loaded, flags, options);
    case 'bundle':
      return commandBundle(loaded);
    case 'config':
      return commandConfig(positional, flags, options);
    case 'self-update':
      return commandSelfUpdate(config, flags, options);
    case 'op-run':
      return commandOpRun(config, positional);
    case 'version':
      return commandVersion(flags, loaded);
    case 'help':
      return { payload: wrap(helpPayload(null)), exitCode: 0 };
    default:
      return refuse(REASON.badArgument, `unknown command: ${command}`, {
        command,
        argument: command,
        accepts: Object.keys(COMMANDS).filter((name) => !COMMANDS[name].internal),
        error: `unknown command: ${command}`,
      });
  }
}

/**
 * Run one command, having already decided what it is.
 *
 * Split out from main() because `dispatch` re-enters it with the argv it parsed
 * out of SSH_ORIGINAL_COMMAND, and both paths have to be validated identically.
 */
async function execute(argv) {
  const { flags, options, positional } = parseArgs(argv);
  const command = positional.shift();

  if (!command || flags.has('help')) {
    return { payload: wrap(helpPayload(command ? null : 'no command given')), exitCode: command ? 0 : 1 };
  }

  const meta = COMMANDS[command];
  if (!meta) {
    return refuse(REASON.badArgument, `unknown command: ${command}`, {
      command,
      argument: command,
      accepts: Object.keys(COMMANDS).filter((name) => !COMMANDS[name].internal),
      error: `unknown command: ${command}`,
    });
  }

  const unknownFlag = [...flags].find((flag) => flag !== 'help' && !meta.flags.includes(flag));
  const unknownOption = [...options.keys()].find((option) => !meta.options.includes(option));
  if (unknownFlag !== undefined || unknownOption !== undefined) {
    const offender = unknownFlag ?? unknownOption;
    return badArgument(`${command} does not take --${offender}`, {
      command,
      argument: `--${offender}`,
      accepts: [...meta.flags.map((flag) => `--${flag}`), ...meta.options.map((option) => `--${option} <value>`)],
    });
  }

  const invalid = validateArguments(command, positional, options);
  if (invalid) return invalid;

  // Administrative documents can contain private command arguments. This path
  // enforces restricted-session refusal before config loading or input reads,
  // and owns its read-only validation and locked compare-and-swap itself.
  if (command === 'service-config') {
    const result = await commandServiceConfig(positional, flags);
    return { payload: wrap(result), exitCode: result.ok ? 0 : 1 };
  }

  const bootstrapInstall = command === 'self-update' && flags.has('install');
  if (bootstrapInstall) {
    const layout = resolveBootstrapLayout(import.meta.url, process.env.LEGIONCTL_HOME ?? null);
    if (!layout.ok) return badArgument(layout.error, { command: 'self-update', argument: '--install' });
    process.env.LEGIONCTL_HOME = layout.base;
  }

  // Everything except version and help touches a real machine, so refuse early
  // and clearly on a platform this agent was never written for.
  if (!isSupportedPlatform() && command !== 'version' && command !== 'help') {
    return refuse(
      REASON.unsupportedPlatform,
      `legionctl runs on linux, windows and mac only; this host reports ${JSON.stringify(process.platform)}`,
      { command, os: platformName() },
    );
  }

  // Read-only commands promise to change nothing, including the last-good copy.
  const controllerRead = command === 'config' && positional[0] !== 'set';
  const bundleCheck = command === 'self-update' && flags.has('check');
  const readOnlyInvocation = !meta.mutates || controllerRead || bundleCheck ||
    (command === 'policy' && positional[0] === undefined) ||
    (command === 'cycle' && flags.has('dry-run'));
  const loaded = loadConfig({ readOnly: readOnlyInvocation });
  if (loaded.config) systemIdentity = loaded.config.system;

  // A machine whose configuration cannot be read can still be inspected, and
  // every inspection says why the rest is refused. Nothing that would change it
  // runs on a configuration nobody can read.
  if (meta.mutates && !controllerRead && !bundleCheck && !loaded.ok && !bootstrapInstall) {
    const problems = loaded.problems ?? [];
    // The file is there and unusable. `config-missing` is reserved for a machine
    // that has no config at all, and that machine is inert rather than broken —
    // its mutations are refused by having nothing to act on, not by this gate.
    return refuse(
      REASON.configInvalid,
      `${command} was refused because config.json cannot be used: ${problems.map((problem) => `${problem.path}: ${problem.message}`).join('; ') || loaded.error}`,
      {
        command,
        problems,
        error: loaded.error,
        action: 'failed',
      },
    );
  }

  const result = await dispatch(command, positional, flags, options, loaded);

  // A detach that could not spawn falls back to doing the work here, so the
  // caller gets a real answer rather than a promise nobody will keep.
  if (result.synchronous) {
    const fallback = result.synchronous;
    const shared = { existingOpId: fallback.id, force: fallback.force, mode: fallback.mode };
    let ran;
    if (fallback.kind === 'update' || fallback.kind === 'restart') {
      const service = loaded.config.services.find((entry) => entry.id === fallback.service);
      ran = fallback.kind === 'update' ? await runUpdate(loaded.config, service, shared) : await runRestart(loaded.config, service, shared);
    } else {
      ran = await runAction(loaded.config, fallback.actionId, shared);
    }
    return mutationReply(ran, {
      service: fallback.service ?? null,
      detached: false,
      op: wireRecord(readOperation(fallback.id)),
    });
  }

  return result;
}

/**
 * A restricted key's command, validated and run in this process.
 *
 * The string comes from the client, so it is parsed by the dispatcher's own
 * grammar — no shell is involved anywhere on this path — and only then handed to
 * the same execute() an ordinary session uses.
 */
async function commandDispatch() {
  const line = process.env.SSH_ORIGINAL_COMMAND ?? '';
  const authorized = authorize(line);
  if (!authorized.ok) {
    log(`restricted session refused: ${authorized.message}`, 'dispatch');
    return refuse(authorized.reasonCode, authorized.message, {
      command: null,
      argument: authorized.argument ?? null,
      accepts: authorized.accepts,
      error: authorized.message,
    });
  }
  process.env.LEGIONCTL_RESTRICTED = '1';
  return execute(authorized.argv);
}

async function main() {
  const argv = process.argv.slice(2);
  const { payload, exitCode } = argv[0] === 'dispatch' ? await commandDispatch() : await execute(argv);
  emit(payload, exitCode);
}

main().catch((error) => {
  const message = error?.stack ? String(error.stack).split('\n')[0] : String(error);
  log(`unhandled failure: ${message}`, 'main');
  emit(
    {
      ok: false,
      contract: CONTRACT_VERSION,
      agentVersion: AGENT_VERSION,
      system: systemIdentity ?? { id: platformName(), name: platformName() },
      reasonCode: REASON.internal,
      message,
      command: null,
      error: message,
    },
    1,
  );
});
