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
import { checkWorkerModules, importModules, WORKER_ENTRIES } from './selftest-modules.mjs';
import { loadConfig, readJsonDocument, saveConfig } from './config-store.mjs';
import { AGENT_METADATA_PROBLEM, AGENT_VERSION, basePath, configPath, isSupportedPlatform, platformName } from './paths.mjs';
import {
  CONTRACT_VERSION,
  DURATION_PATTERN,
  envelope,
  isValidOperationId,
  isValidToken,
  OK_REPLY_ACTIONS,
  outcomeOf,
  parseDuration,
  producedOutcome,
  REASON,
  resolveDeadline,
} from './contract.mjs';
import { runAction } from './actions.mjs';
import { MAX_CONTROLLER_BYTES, readController, readControllerMeta, storeController } from './controller.mjs';
import { buildBundle, runDoctor } from './doctor.mjs';
import { reportedFloor } from './floor.mjs';
import { acquireOperationLock } from './lock.mjs';
import { log, readLogLines } from './log.mjs';
import { redactReply, restrictedRedactor } from './redact.mjs';
import { finishUnrunnable, replayOf } from './operate.mjs';
import {
  beginOperation,
  cancelOperation,
  countOperations,
  currentInitiator,
  deriveState,
  listOperations,
  missingOperation,
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
import { processStartTicks } from './state.mjs';
import { buildStatus, DEFAULT_STATUS_BUDGET_MS } from './status.mjs';
import { checkBundle, resolveBootstrapLayout, runBootstrapInstall, runRollback, runSelfUpdate } from './selfupdate.mjs';
import { runRestart, runUpdate } from './update.mjs';
import { authorize, isRestrictedSession, parseCommandLine } from './dispatch.mjs';
import { PATH_OPTIONS, parseArgs } from './args.mjs';

/** For a command every invocation of which asks for a change. */
const always = () => true;

/**
 * The command table, and the only place a command is wired to what answers
 * it: help, the refusal of an unknown command and dispatch all read it, so a
 * command cannot be listed without being answered or answered without being
 * listed. `flags` stand alone, `options` take a value, and anything outside
 * them is refused, which is what stops a typo from being read as a different
 * command's flag.
 *
 * `mutates` says whether the command can change the machine at all, which is
 * what a broken config.json refuses. `changes` says whether one invocation
 * asks for a change, and so states an outcome (contract section 5.6): a dry
 * run, a bundle check and a read of a mutating command's setting do not.
 *
 * Three more say how a mutating command's invocation meets config.json.
 * `readOnly` names the ones that promise to change nothing, so loading the
 * file leaves even its last good copy alone; a command that cannot mutate
 * promises that always. `withoutConfig` names the ones that still answer when
 * the file cannot be used, because what they do never reads it. `prepare`
 * settles what has to be settled before anything is read at all, and a reply
 * from it answers the invocation there and then.
 *
 * `run` answers a validated invocation, given the loaded configuration.
 */
const COMMANDS = {
  status: {
    flags: [], options: ['budget-ms'], mutates: false, summary: 'bounded snapshot of the machine, its services and its operations',
    run: ({ loaded, options }) => commandStatus(loaded, options),
  },
  busy: {
    flags: [], options: ['budget-ms'], mutates: false, summary: 'the aggregated busy state, with per-service evidence',
    run: ({ config, options }) => commandBusy(config, options),
  },
  update: {
    flags: ['force', 'detach', 'when-idle'],
    options: ['service', 'op', 'expires'],
    mutates: true,
    changes: always,
    summary: 'update one service; --when-idle waits for an idle machine, up to --expires (1m to 7d), and cannot be combined with --force',
    run: ({ config, flags, options }) => commandUpdate(config, flags, options),
  },
  restart: {
    flags: ['force', 'detach', 'when-idle'],
    options: ['service', 'op', 'expires'],
    mutates: true,
    changes: always,
    summary: 'restart one service; --when-idle waits for an idle machine, up to --expires (1m to 7d), and cannot be combined with --force',
    run: ({ config, flags, options }) => commandRestart(config, flags, options),
  },
  boot: {
    flags: ['force', 'no-reboot', 'when-idle'],
    options: ['op', 'expires'],
    mutates: true,
    changes: always,
    summary: 'arm a boot target and reboot into it; --when-idle waits for an idle machine, up to --expires (1m to 7d), and cannot be combined with --force',
    run: ({ config, positional, flags, options }) => commandBoot(config, positional, flags, options),
  },
  sleep: {
    flags: ['force', 'when-idle'],
    options: ['op', 'expires'],
    mutates: true,
    changes: always,
    summary: 'suspend this machine; --when-idle waits for an idle machine, up to --expires (1m to 7d), and cannot be combined with --force',
    run: ({ config, flags, options }) => commandSleep(config, flags, options),
  },
  run: {
    flags: ['force', 'detach', 'when-idle'],
    options: ['op', 'expires'],
    mutates: true,
    changes: always,
    summary: 'run a configured action; --when-idle waits for an idle machine, up to --expires (1m to 7d), and cannot be combined with --force',
    run: ({ config, positional, flags, options }) => commandRun(config, positional, flags, options),
  },
  cycle: {
    flags: ['dry-run', 'force'], options: ['op'], mutates: true, summary: 'the scheduled maintenance cycle over every eligible service',
    changes: (positional, flags) => !flags.has('dry-run'),
    readOnly: (positional, flags) => flags.has('dry-run'),
    run: ({ config, flags, options }) => commandCycle(config, flags, options),
  },
  'auto-update': {
    flags: [], options: ['service'], mutates: true, summary: 'on, off, pause <duration> or resume',
    changes: always,
    run: ({ config, positional, options }) => commandAutoUpdate(config, positional, options),
  },
  policy: {
    flags: [], options: ['service'], mutates: true, summary: 'read the update policy; "policy set" reads a JSON patch from stdin',
    changes: (positional) => positional[0] === 'set',
    readOnly: (positional) => positional[0] === undefined,
    run: ({ config, positional, options }) => commandPolicy(config, positional, options),
  },
  'service-config': {
    flags: ['stdin'], options: [], mutates: true, summary: 'administrative service configuration: get, validate --stdin, or set --stdin',
    changes: (positional) => positional[0] === 'set',
    // Administrative documents can contain private command arguments. This
    // command refuses a restricted session before config loading or input
    // reads, and owns its read-only validation and locked compare-and-swap
    // itself, so it is answered before the config is loaded at all.
    beforeConfig: true,
    run: async ({ positional, flags }) => {
      const result = await commandServiceConfig(positional, flags);
      return { payload: wrap(result), exitCode: result.ok ? 0 : 1 };
    },
  },
  op: {
    flags: [], options: ['wait'], mutates: false, summary: 'one operation record; --wait long-polls until it finishes',
    run: ({ config, positional, options }) => commandOp(config, positional, options),
  },
  cancel: {
    flags: [], options: [], mutates: true, summary: 'cancel a queued operation',
    changes: always,
    run: ({ positional }) => commandCancel(positional),
  },
  history: {
    flags: [], options: ['limit', 'service', 'kind'], mutates: false, summary: 'recent operations, newest first',
    run: ({ config, options }) => commandHistory(config, options),
  },
  logs: {
    flags: [], options: ['lines', 'op'], mutates: false, summary: 'the tail of the agent log, or one operation\'s log',
    run: ({ options }) => commandLogs(options),
  },
  doctor: {
    flags: ['deep'], options: ['service'], mutates: false, summary: 'check everything that can be wrong with this machine',
    run: ({ loaded, flags, options }) => commandDoctor(loaded, flags, options),
  },
  bundle: {
    flags: [], options: [], mutates: false, summary: 'a diagnostic bundle with secrets removed',
    run: ({ loaded }) => commandBundle(loaded),
  },
  config: {
    flags: ['replace'],
    options: ['controller-id', 'revision'],
    mutates: true,
    changes: (positional) => positional[0] === 'set',
    // The setup document is a file of its own, which a broken config.json
    // leaves readable; only a replacement waits for a usable one.
    readOnly: (positional) => positional[0] !== 'set',
    withoutConfig: (positional) => positional[0] !== 'set',
    summary: 'the stored setup document; "config set" replaces it from stdin, "config meta" reads its identity',
    run: ({ positional, flags, options }) => commandConfig(positional, flags, options),
  },
  'self-update': {
    flags: ['stdin', 'install', 'rollback', 'check', 'allow-downgrade'],
    options: ['from', 'op'],
    mutates: true,
    changes: (positional, flags) => !flags.has('check'),
    // A bundle check reads only the bundle. The bootstrap install takes over
    // from an older agent, whose config.json this one may not accept yet, and
    // runs from the tree it was unpacked to, which says where it is to live.
    readOnly: (positional, flags) => flags.has('check'),
    withoutConfig: (positional, flags) => flags.has('check') || flags.has('install'),
    prepare: ({ flags }) => (flags.has('install') ? settleBootstrapBase() : null),
    summary: 'replace this agent from a signed bundle',
    run: ({ config, flags, options }) => commandSelfUpdate(config, flags, options),
  },
  // main() runs it before anything is parsed: the command it carries is in
  // SSH_ORIGINAL_COMMAND, and is parsed and loaded like any other.
  dispatch: {
    flags: [], options: [], mutates: true, summary: 'run one allowed command from SSH_ORIGINAL_COMMAND (restricted keys)',
    run: () => commandDispatch(),
  },
  'op-run': {
    flags: [], options: [], mutates: true, internal: true, summary: 'the detached worker for one operation',
    run: ({ config, positional }) => commandOpRun(config, positional),
  },
  // These two answer on any platform: neither touches the machine.
  version: {
    flags: ['check'], options: [], mutates: false, anyPlatform: true, summary: 'the agent version, and with --check a self-test',
    run: ({ loaded, flags }) => commandVersion(flags, loaded),
  },
  help: {
    flags: [], options: [], mutates: false, anyPlatform: true, summary: 'print this list',
    run: () => ({ payload: wrap(helpPayload(null)), exitCode: 0 }),
  },
};

/** The command an invocation names, or null when it names none this agent has. */
function commandNamed(name) {
  return typeof name === 'string' && Object.hasOwn(COMMANDS, name) ? COMMANDS[name] : null;
}

/**
 * Write the one reply. A restricted key reads it redacted (contract section
 * 13): the configured values of config.json and its last good copy are
 * replaced wherever the reply quotes them in free text. Records and the log on
 * disk keep everything, for the person who owns the machine.
 *
 * If redaction itself fails, the reply is replaced rather than sent as it
 * was: a restricted key gets an internal error, and a mutation's outcome is
 * unknown, so its client re-reads the operation instead of guessing.
 *
 * Only the first call writes anything. A failure that surfaces after the reply
 * has gone, an exception in a stray callback or a promise nobody awaited, must
 * not put a second object on stdout, which would make the first unreadable.
 */
let emitted = false;

function emit(payload, exitCode) {
  if (emitted) return;
  let reply = payload;
  let code = exitCode;
  if (isRestrictedSession()) {
    try {
      reply = redactReply(payload, restrictedRedactor());
    } catch (error) {
      log(`a reply could not be redacted for a restricted session: ${error?.message ?? error}`, 'main');
      const message = 'the reply could not be redacted for a restricted session';
      reply = {
        ok: false,
        contract: CONTRACT_VERSION,
        agentVersion: AGENT_VERSION,
        system: systemIdentity ?? { id: platformName(), name: platformName() },
        reasonCode: REASON.internal,
        message,
        ...(answeringMutation || 'outcome' in payload ? { outcome: 'unknown' } : {}),
        command: null,
        error: message,
      };
      code = 1;
    }
  }
  // Marked written only once there is something to write, so a reply that
  // cannot be serialised still leaves room for the failure that says so.
  const text = `${JSON.stringify(reply)}\n`;
  emitted = true;
  process.stdout.write(text);
  process.exitCode = code;
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

/** A refusal: the command did not do what was asked, and says which closed reason stopped it. */
function refuse(reasonCode, message, extra = {}) {
  return { payload: wrap({ ok: false, reasonCode, message, ...extra }), exitCode: 1 };
}

function badArgument(message, { command = null, argument = null, accepts = null } = {}) {
  return refuse(REASON.badArgument, message, {
    command,
    argument,
    ...(accepts ? { accepts } : {}),
    error: message,
  });
}

/**
 * The reply to a mutation, in the shape every mutating verb shares.
 *
 * Rule 7 decides `ok` and the exit code together: deferring, queueing, doing
 * nothing for a stated reason, being refused by a lock that somebody else
 * holds and being told the work is already under way are answers, not errors,
 * so they are `ok: true` and exit 0 whatever the work itself returned. A
 * client scripting against exit codes should not have to treat "the machine is
 * busy" as a failure.
 *
 * The outcome (contract section 5.6) is the reducer's for the reply itself,
 * lowered to unknown only where the work said it failed in a way that may
 * have changed the machine. A reply that changes nothing, a bundle check,
 * states none. And an --op id already bound to another request is not a
 * mutation reply at all: nothing was attempted, so it is refused about that
 * argument.
 */
function mutationReply(result, extra = {}, { changes = true } = {}) {
  if (result.refusal) return badArgument(result.message, result.refusal);
  const ok = result.ok === true || OK_REPLY_ACTIONS.has(result.action);
  const reasonCode = result.reasonCode ?? null;
  const payload = wrap({
    ok,
    reasonCode,
    message: result.message ?? '',
    ...(changes ? { outcome: producedOutcome({ ok, action: result.action, reasonCode, outcome: result.outcome }) } : {}),
    action: result.action,
    ...(result.replayed === true ? { replayed: true } : {}),
    ...(Array.isArray(result.notes) && result.notes.length > 0 ? { notes: result.notes } : {}),
    ...extra,
    // Work that is already running is followed exactly like work just
    // accepted, and the record says whether a detached worker owns it.
    ...(result.action === 'already-running' ? { detached: result.detached === true } : {}),
  });
  return { payload, exitCode: ok ? 0 : 1 };
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

/**
 * The keys a mutating reply carries whatever its outcome, per command, with
 * what the work reported where it has reported anything: a request that is
 * only queued has not, and a replay has what its record kept.
 */
function commandShape(kind, { service, target, actionId }, result = null) {
  if (kind === 'update') return { service, from: result?.from ?? null, to: result?.to ?? null };
  if (kind === 'restart') return { service };
  if (kind === 'boot') return { target, armed: ['armed', 'rebooting', 'rebooted'].includes(result?.action) };
  if (kind === 'run') return { id: actionId, exitCode: result?.exitCode ?? null, output: result?.output ?? '' };
  return {};
}

/**
 * Park a request until the machine is idle, and answer `queued`.
 *
 * `--when-idle` with `--force` is refused. One asks to wait until nothing is
 * busy, the other not to care whether anything is, and a queued request that
 * did both used to skip the wait and the busy gate alike: an interruption
 * nobody watching asked for, at a time nobody chose.
 */
function queueRequest({ kind, service = null, target = null, actionId = null, flags, options, config }) {
  if (flags.has('force')) {
    return badArgument(
      `${kind} --when-idle waits until the machine is idle, and --force does not wait for that; ask for one or the other`,
      { command: kind, argument: '--force' },
    );
  }
  const ttl = queueTtl(options);
  if (!ttl.ok) return badArgument(ttl.error, { command: kind, argument: '--expires' });

  const queued = queueOperation({
    id: options.get('op') ?? newOperationId(),
    kind,
    service,
    target,
    actionId,
    systemId: config.system.id,
    noReboot: flags.has('no-reboot'),
    ttlMs: ttl.ttlMs,
    expiresAt: ttl.expiresAt,
  });

  // An id already bound to another request is a bad --op, never a replay and
  // never a conflict to wait out (contract section 5.5).
  if (queued.conflict) return badArgument(queued.error, { command: kind, argument: '--op' });
  if (queued.badExpiry) return badArgument(queued.error, { command: kind, argument: '--expires' });
  if (!queued.ok) {
    return refuse(REASON.internal, `the request could not be recorded: ${queued.error}`, { command: kind, argument: null, error: queued.error });
  }
  if (queued.replay) {
    const record = queued.record;
    return mutationReply(replayOf(record), { ...commandShape(kind, { service, target, actionId }, record.result), op: wireRecord(record) });
  }

  const what = service ?? target ?? actionId ?? kind;
  const verb = kind === 'boot' && flags.has('no-reboot') ? 'armed without rebooting' : { update: 'updated', restart: 'restarted', run: 'run', boot: 'booted', sleep: 'suspended' }[kind] ?? kind;
  return ok({
    action: 'queued',
    message: `${what} will be ${verb} on the next idle cycle, or dropped at ${queued.record.expiresAt}`,
    // Every mutating reply keeps its command's shape, whatever the answer is, so
    // a client decodes one thing per verb rather than one per outcome.
    ...commandShape(kind, { service, target, actionId }),
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
  // about the machine, not about a service. Like a declared "none" it is
  // monitored: nothing is waiting for somebody to describe a probe.
  if (entries.length <= 1) {
    const only =
      entries.length === 1
        ? entries[0].busy
        : {
            busy: false,
            unknown: false,
            monitored: true,
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

  if (flags.has('when-idle')) return queueRequest({ kind: 'boot', target, flags, options, config });

  const result = await runBoot(config, target, { ...operationOptions(flags, options), noReboot: flags.has('no-reboot') });
  return mutationReply(result, {
    target: result.target ?? target ?? null,
    armed: result.armed === true,
    op: result.opId ? wireRecord(readOperation(result.opId)) : null,
    ...(result.conflict ? { conflict: result.conflict } : {}),
  });
}

async function commandSleep(config, flags, options) {
  if (flags.has('when-idle')) return queueRequest({ kind: 'sleep', flags, options, config });

  const result = await runSleep(config, operationOptions(flags, options));
  return mutationReply(result, {
    op: result.opId ? wireRecord(readOperation(result.opId)) : null,
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
  if (flags.has('when-idle')) return queueRequest({ kind: 'run', actionId: id, flags, options, config });

  const shared = operationOptions(flags, options);
  if (flags.has('detach')) return detachedRun(config, { kind: 'run', actionId: id, ...shared });

  const result = await runAction(config, id, shared);
  return mutationReply(result, {
    id: result.id ?? id ?? null,
    exitCode: result.exitCode ?? null,
    output: result.output ?? '',
    detached: false,
    op: result.opId ? wireRecord(readOperation(result.opId)) : null,
    ...(result.conflict ? { conflict: result.conflict } : {}),
  });
}

async function commandCycle(config, flags, options) {
  const result = await runCycle(config, {
    force: flags.has('force'),
    dryRun: flags.has('dry-run'),
    opId: options.get('op') ?? null,
  });

  // A dry run changes nothing and opens no record, so it is `skipped`: a
  // client must never read a preview as a cycle that ran.
  if (flags.has('dry-run')) {
    return ok({
      action: 'skipped',
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
        updates: policyUpdates(describeServiceUpdates(config, service), service),
      })),
    });
  }

  const service = config.services.find((entry) => entry.id === serviceId);
  if (!service) return missingService(config, serviceId);
  return ok({
    ...(message ? { message } : {}),
    service: serviceId,
    updates: policyUpdates(describeServiceUpdates(config, service), service),
    autoUpdate: config.updates.automatic === true,
    changed,
  });
}

/**
 * Write a patch of the update policy into config.json, where the policy
 * lives, the pause included. `policy set` and `auto-update` both write
 * through this, so they cannot come to mean different things by the same
 * setting. A service's policy is in its own updates block, where a null
 * removes the key so the machine's value is inherited again. The machine's is
 * in the top-level one, where there is nothing to inherit from, so a null
 * clears the setting instead.
 */
function saveUpdatesPatch(serviceId, patch) {
  return saveConfig((stored) => {
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
      {
        command: 'policy',
        // In the one problem shape every refusal uses, so a client lists them
        // the same way whichever command was refused.
        problems: validated.errors.map((entry) => ({ level: 'error', path: entry.path, message: entry.message, fix: null })),
        error: 'invalid policy patch',
      },
    );
  }

  const patch = validated.patch;
  const changed = Object.keys(patch);
  if (changed.length === 0) return policyReply(config, serviceId, { message: 'nothing in the patch changed anything' });

  const saved = saveUpdatesPatch(serviceId, patch);
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
    const saved = saveUpdatesPatch(serviceId, patch);
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
    // Not an error about the machine or the argument: the request never
    // arrived, or it was pruned. `op: null` is what a client keys on, and a
    // client that retries an id it was never told about would retry forever.
    if (!record) return refuse(REASON.notConfigured, missingOperation(id), { id, op: null });
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
  if (result.missing) {
    return refuse(REASON.notConfigured, result.message, { command: 'cancel', argument: id, error: `no operation ${id} on this machine` });
  }
  return mutationReply(result, { id, op: result.op ? wireRecord(readOperation(id)) : null });
}

function commandHistory(config, options) {
  const limit = integerOption(options, 'limit', 30, { max: 200 });
  const service = options.get('service') ?? null;
  const kind = options.get('kind') ?? null;
  const records = listOperations({ limit, service, kind }).map((record) => deriveState(record, { systemId: config?.system.id ?? null }));
  const retained = countOperations();
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
      return refuse(REASON.notConfigured, missingOperation(opId), {
        command: 'logs',
        argument: '--op',
        error: `no operation ${opId} on this machine`,
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
    // `raw` is the stored canonical text, the exact bytes `hash` covers, and it
    // is what a client hashes and adopts. `controller` is the same document
    // parsed, kept for older clients; re-serialising it can never be relied on
    // to give the same bytes back.
    const raw = stored.document ? stored.text : null;
    const payload = {
      controller: stored.document,
      raw,
      hash: stored.hash,
      bytes: raw ? Buffer.byteLength(raw, 'utf8') : 0,
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
  // Refused by the lock, the document was not stored: that is exit 1, with the
  // outcome `deferred` telling the client it can push again later.
  const lock = acquireOperationLock({ kind: 'config', target: 'config set' });
  if (!lock.ok) {
    return refuse(lock.reasonCode ?? REASON.operationInProgress, lock.message, {
      hash: null,
      bytes: 0,
      id,
      revision,
      error: lock.message,
      ...(lock.conflictDetail ? { conflict: lock.conflictDetail } : {}),
    });
  }

  try {
    const stored = storeController(bytes, { id, revision, replace: flags.has('replace'), restricted: isRestrictedSession() });
    if (!stored.ok) {
      log(`config set refused: ${stored.error}`, 'config');
      return refuse(stored.reasonCode ?? REASON.badArgument, stored.error, {
        hash: stored.hash ?? null,
        bytes: stored.bytes ?? 0,
        id,
        revision,
        // A controller-conflict is either two setups or two branches of one,
        // and only the second is a merge, so it always says which.
        ...(stored.reasonCode === REASON.controllerConflict ? { divergent: stored.divergent === true } : {}),
        ...(stored.current ? { current: stored.current } : {}),
        // Why a document was refused, as paths a client can point at.
        ...(stored.problems ? { problems: stored.problems } : {}),
        ...(stored.launchChanges ? { launchChanges: stored.launchChanges } : {}),
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
      // Stored all the same: what a client can use but would say something about.
      ...(stored.notes?.length > 0 ? { notes: stored.notes } : {}),
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

/**
 * Point this process at the base the bootstrap install belongs to, which is
 * the one it was unpacked into, before config.json or anything else is read
 * from there. A refusal when the tree is not where an install has to run from.
 */
function settleBootstrapBase() {
  const layout = resolveBootstrapLayout(import.meta.url, process.env.LEGIONCTL_HOME ?? null);
  if (!layout.ok) return badArgument(layout.error, { command: 'self-update', argument: '--install' });
  process.env.LEGIONCTL_HOME = layout.base;
  return null;
}

async function commandSelfUpdate(config, flags, options) {
  const modes = ['check', 'rollback', 'install'].filter((flag) => flags.has(flag));
  if (modes.length > 1 || (flags.has('install') && (flags.has('stdin') || options.has('from')))) {
    return badArgument('self-update --check, --rollback and --install are separate modes; --install reads its own staged tree', { command: 'self-update' });
  }
  if ((flags.has('stdin') && options.has('from')) || (flags.has('rollback') && (flags.has('stdin') || options.has('from')))) {
    return badArgument('self-update accepts one bundle source; --rollback does not take a bundle', { command: 'self-update' });
  }
  // Going below the installed agent or the version floor is a decision only an
  // unrestricted session makes (contract section 13). The dispatcher already
  // refuses the flag; this holds however else a restricted session got here.
  if (flags.has('allow-downgrade') && isRestrictedSession()) {
    const message = 'self-update --allow-downgrade takes the agent below the installed one or the version floor, which a restricted key may not do';
    return refuse(REASON.restricted, message, { command: 'self-update', argument: '--allow-downgrade', error: message });
  }
  const allowDowngrade = flags.has('allow-downgrade');

  if (flags.has('check')) {
    const result = await checkBundle({ from: options.get('from') ?? null, stdin: flags.has('stdin') });
    return mutationReply(result, {
      current: { version: AGENT_VERSION, contract: CONTRACT_VERSION },
      floor: reportedFloor(),
      staged: null,
      previous: null,
      manifest: result.manifest,
      selfTest: null,
      op: null,
      available: result.ok,
    }, { changes: false });
  }

  if (flags.has('rollback')) {
    const result = await runRollback(config, { ...operationOptions(flags, options), allowDowngrade });
    return mutationReply(result, {
      current: result.current,
      floor: reportedFloor(),
      staged: result.staged ?? null,
      previous: result.previous ?? null,
      manifest: result.manifest ?? null,
      selfTest: result.selfTest ?? null,
      op: result.opId ? wireRecord(readOperation(result.opId)) : null,
    });
  }

  if (flags.has('install')) {
    const result = await runBootstrapInstall(config, { ...operationOptions(flags, options), allowDowngrade });
    return mutationReply(result, {
      current: result.current,
      floor: reportedFloor(),
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

  const result = await runSelfUpdate(config, { from, stdin: flags.has('stdin'), ...operationOptions(flags, options), allowDowngrade });
  return mutationReply(result, {
    current: result.current,
    floor: reportedFloor(),
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

/**
 * Open the record, hand it to a detached worker, and answer `accepted`.
 *
 * Every answer keeps the command's own shape (`commandShape`), whatever it is,
 * so a client decodes one thing per verb: `--detach` is a way of asking, not a
 * different reply.
 */
async function detachedRun(config, { kind, service = null, target = null, actionId = null, opId, force, mode }) {
  const id = opId ?? newOperationId();
  const shape = commandShape(kind, { service, target, actionId });
  const opened = beginOperation({ id, kind, service, target, actionId, force, mode,
    systemId: config.system.id, detached: true, queueRequested: false });
  // An id already bound to another request is a bad --op (contract section 5.5).
  if (opened.conflict) return badArgument(opened.error, { command: kind, argument: '--op' });
  if (!opened.ok) {
    return refuse(REASON.internal, `the operation record could not be written (${opened.error}), so nothing was started`, {
      command: kind, argument: null, error: opened.error,
    });
  }
  if (opened.replay) {
    const existing = opened.record;
    return mutationReply(replayOf(existing), { ...commandShape(kind, { service, target, actionId }, existing.result), op: wireRecord(existing) });
  }

  updateOperation(id, { launchPendingUntil: new Date(Date.now() + 30000).toISOString() });
  const started = await detachOperation(opened.record);
  if (!started.ok) {
    if (started.uncertain) {
      return mutationReply({ ok: true, action: 'accepted', reasonCode: REASON.timedOut,
        message: `the worker launch could not be confirmed: ${started.error}; query operation ${id} before retrying` }, {
        ...shape, detached: true, op: wireRecord(readOperation(id)) });
    }
    log(`could not detach ${kind}: ${started.error}; running it in this session instead`, kind);
    updateOperation(id, { detached: false });
    return { synchronous: { id, kind, service, target, actionId, force, mode } };
  }
  // The worker started no later than now, which is what tells it from a later
  // process given its id should it die before it claims the record.
  updateOperation(id, (record) => record.state === 'running' && record.pid === process.pid
    ? { pid: started.pid, ownerStartedAt: new Date().toISOString(), ownerStartTicks: processStartTicks(started.pid) }
    : {});
  return mutationReply({ ok: true, action: 'accepted',
    message: `${kind} accepted as operation ${id}; ask "op ${id} --wait 20" for the result` }, {
    ...shape, detached: true, op: wireRecord(readOperation(id)) });
}

/**
 * Do the work an opened record asks for, in this process, exactly as the
 * request would have done it: what a detached worker does, and what a detach
 * that could not start one falls back to. `record` is the record, or the
 * request it was opened for. Null for a kind that is never run detached.
 *
 * A service or action removed from the setup since the request was made can
 * no longer be run, and whoever holds the record is the only one left to end
 * it: a refusal of its own would leave it running for good.
 */
async function runOpened(config, record) {
  const shared = { existingOpId: record.id, force: record.force === true, noReboot: record.noReboot === true, mode: record.mode ?? 'manual' };
  if (record.kind === 'update' || record.kind === 'restart') {
    const service = config.services.find((entry) => entry.id === record.service);
    if (!service) {
      return finishUnrunnable(record.id, {
        ok: false,
        action: 'failed',
        reasonCode: REASON.unknownService,
        message: `the ${record.kind} names ${record.service}, which this machine no longer has`,
      });
    }
    return record.kind === 'update' ? runUpdate(config, service, shared) : runRestart(config, service, shared);
  }
  if (record.kind === 'run') {
    if (!config.actions.some((action) => action.id === record.actionId)) {
      return finishUnrunnable(record.id, {
        ok: false,
        action: 'failed',
        reasonCode: REASON.unknownAction,
        message: `the run names the action ${record.actionId}, which this machine no longer has`,
      });
    }
    return runAction(config, record.actionId, shared);
  }
  if (record.kind === 'boot') return runBoot(config, record.target, shared);
  if (record.kind === 'sleep') return runSleep(config, shared);
  return null;
}

/** The detached worker. Never called by a client. */
async function commandOpRun(config, positional) {
  const id = positional[0];
  if (!id || !isValidOperationId(id)) return badArgument('op-run takes one operation id', { command: 'op-run' });
  const record = readOperation(id);
  if (!record) return badArgument(`there is no operation ${id}`, { command: 'op-run' });

  const result = await runOpened(config, record);
  if (!result) return badArgument(`a ${record.kind} operation cannot be run detached`, { command: 'op-run' });
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
    const imported = await importModules();
    const failed = imported.failed;
    let loadedCount = imported.loaded;
    const workers = checkWorkerModules(fileURLToPath(new URL('.', import.meta.url)));
    if (!workers.ok) failed.push(`worker modules: ${workers.error}`);
    else loadedCount += WORKER_ENTRIES.length;
    // The version every reply carries comes from package.json. A tree that
    // cannot say which version it is must not replace one that can.
    if (AGENT_METADATA_PROBLEM) failed.push(`package.json: ${AGENT_METADATA_PROBLEM}`);
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

function unknownCommand(command) {
  return refuse(REASON.badArgument, `unknown command: ${command}`, {
    command,
    argument: command,
    accepts: Object.keys(COMMANDS).filter((name) => !COMMANDS[name].internal),
    error: `unknown command: ${command}`,
  });
}

/**
 * Whether an invocation asks for a change (contract section 5.6). Its reply
 * carries `outcome` whatever happened, a refusal included, and nothing else's
 * does: a dry run, a bundle check and every read change nothing, so there is
 * no outcome to state.
 */
function mutatingInvocation(command, positional, flags) {
  if (flags.has('help')) return false;
  return commandNamed(command)?.changes?.(positional, flags) === true;
}

/** Whether the reply being built answers a mutating invocation, for the last-resort handler in main(). */
let answeringMutation = false;

/**
 * Run one command, having already decided what it is.
 *
 * Split out from main() because `dispatch` re-enters it with the argv it parsed
 * out of SSH_ORIGINAL_COMMAND, and both paths have to be validated identically.
 */
async function execute(argv) {
  const { flags, options, positional } = parseArgs(argv);
  const command = positional.shift();
  const mutating = mutatingInvocation(command, positional, flags);
  if (mutating) answeringMutation = true;
  const reply = await answer(command, positional, flags, options);
  // A refusal answers a mutating invocation too, and a client decides on the
  // outcome of every one, so the replies built without one get it here.
  if (mutating) reply.payload.outcome ??= outcomeOf(reply.payload);
  return reply;
}

/** The reply to one parsed invocation, before execute() adds what every reply to a mutation carries. */
async function answer(command, positional, flags, options) {
  if (!command || flags.has('help')) {
    return { payload: wrap(helpPayload(command ? null : 'no command given')), exitCode: command ? 0 : 1 };
  }

  const meta = commandNamed(command);
  if (!meta) return unknownCommand(command);

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

  if (meta.beforeConfig) return meta.run({ positional, flags, options });

  const prepared = meta.prepare?.({ positional, flags, options }) ?? null;
  if (prepared) return prepared;

  // Everything except version and help touches a real machine, so refuse early
  // and clearly on a platform this agent was never written for.
  if (!isSupportedPlatform() && !meta.anyPlatform) {
    return refuse(
      REASON.unsupportedPlatform,
      `legionctl runs on linux, windows and mac only; this host reports ${JSON.stringify(process.platform)}`,
      { command, os: platformName() },
    );
  }

  // Read-only invocations promise to change nothing, including the last-good copy.
  const loaded = loadConfig({ readOnly: meta.readOnly?.(positional, flags) ?? !meta.mutates });
  if (loaded.config) systemIdentity = loaded.config.system;

  // A machine whose configuration cannot be read can still be inspected, and
  // every inspection says why the rest is refused. Nothing that would change it
  // runs on a configuration nobody can read.
  if (meta.mutates && !loaded.ok && !meta.withoutConfig?.(positional, flags)) {
    const problems = loaded.problems ?? [];
    // The file is there and unusable. `config-missing` is reserved for a machine
    // that has no config at all, and that machine is inert rather than broken —
    // its mutations are refused by having nothing to act on, not by this gate.
    // Nothing was attempted, so this is a refusal in the refusal shape rather
    // than an action of the command.
    return refuse(
      REASON.configInvalid,
      `${command} was refused because config.json cannot be used: ${problems.map((problem) => `${problem.path}: ${problem.message}`).join('; ') || loaded.error}`,
      {
        command,
        argument: null,
        problems,
        error: loaded.error,
      },
    );
  }

  const result = await meta.run({ config: loaded.config, loaded, positional, flags, options });

  // A detach that could not spawn falls back to doing the work here, so the
  // caller gets a real answer rather than a promise nobody will keep. It keeps
  // the command's own shape, like every other answer to it.
  if (result.synchronous) {
    const fallback = result.synchronous;
    const ran = await runOpened(loaded.config, fallback);
    return mutationReply(ran, { ...commandShape(fallback.kind, fallback, ran), detached: false, op: wireRecord(readOperation(fallback.id)) });
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
  // Only a restricted key arrives here, so its reply is redacted whether or
  // not the forced command said so, a refusal included.
  process.env.LEGIONCTL_RESTRICTED = '1';
  const line = process.env.SSH_ORIGINAL_COMMAND ?? '';
  const authorized = authorize(line);
  if (!authorized.ok) {
    log(`restricted session refused: ${authorized.message}`, 'dispatch');
    const refused = refuse(authorized.reasonCode, authorized.message, {
      command: null,
      argument: authorized.argument ?? null,
      accepts: authorized.accepts,
      error: authorized.message,
    });
    // A mutation this key may not send is still a mutation that was asked for,
    // and its client decides on the outcome like any other.
    const tokens = parseCommandLine(line).tokens;
    if (tokens && tokens.length > 2) {
      const { flags, positional } = parseArgs(tokens.slice(3));
      if (mutatingInvocation(tokens[2], positional, flags)) refused.payload.outcome = outcomeOf(refused.payload);
    }
    return refused;
  }
  return execute(authorized.argv);
}

async function main() {
  const argv = process.argv.slice(2);
  const { payload, exitCode } = argv[0] === 'dispatch' ? await commandDispatch() : await execute(argv);
  emit(payload, exitCode);
}

/** The reply to a failure nothing else caught: internal, and unknown for a mutation. */
function emitUnexpected(error) {
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
      // Whatever a mutation had done before this, nobody observed it.
      ...(answeringMutation ? { outcome: 'unknown' } : {}),
      command: null,
      error: message,
    },
    1,
  );
}

// stdout and stderr are pipes to an ssh session, and the other end can go at
// any moment: a client that gave up, a phone that slept. Writing to a closed
// pipe fails asynchronously with EPIPE, and an `error` event nobody listens for
// ends the process there and then, which used to kill the agent half way
// through a rollback for the sake of a progress line. The reply cannot reach a
// reader who has gone, and the work is worth finishing anyway.
for (const stream of [process.stdout, process.stderr]) {
  stream.on('error', () => { /* the reader has gone; the log still has everything */ });
}

// Anything thrown where main() cannot catch it still gets its one reply. The
// process then ends as soon as that reply is written: the state such a failure
// leaves behind is unknown, and a mutation in progress is better left to
// recovery than continued on it. process.exit() would drop whatever stdout has
// not yet written to its pipe, so it waits for that first.
function failUnexpectedly(error) {
  emitUnexpected(error);
  const exit = () => process.exit();
  setTimeout(exit, 2000).unref();
  try {
    process.stdout.write('', exit);
  } catch {
    exit();
  }
}
process.on('uncaughtException', failUnexpectedly);
process.on('unhandledRejection', failUnexpectedly);

main().catch(emitUnexpected);
