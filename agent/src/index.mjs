#!/usr/bin/env node
// legionctl — the Legion Control agent.
//
// Contract: every invocation prints exactly ONE JSON object to stdout and
// nothing else, including on failure. Progress and diagnostics go to stderr and
// to <base>/legionctl.log. Exit code 0 when the command did what was asked,
// 1 otherwise.

import os from 'node:os';
import {
  AGENT_VERSION,
  isSupportedPlatform,
  loadConfig,
  loadState,
  platformName,
  saveConfig,
  serviceState,
} from './config.mjs';
import { listActions, runAction } from './actions.mjs';
import { armNextBoot, bootTargets, findBootTarget, scheduleReboot, suspendMachine } from './boot.mjs';
import { checkAllBusy } from './probes/busy.mjs';
import { checkHealth } from './probes/health.mjs';
import * as appProvider from './providers/app.mjs';
import * as commandProvider from './providers/command.mjs';
import * as npmProvider from './providers/npm.mjs';
import { canRestart, probeService, selectService } from './service.mjs';
import { runRestart, runUpdate } from './update.mjs';
import { log } from './log.mjs';

const COMMANDS = {
  status: { flags: [], options: [], summary: 'read-only snapshot: system, services, busy, boot targets, actions' },
  busy: { flags: [], options: [], summary: 'the aggregated busy state of every service on this system' },
  update: { flags: ['force'], options: ['service'], summary: 'run one update cycle for one service' },
  restart: { flags: ['force'], options: ['service'], summary: 'restart one service (refuses while busy without --force)' },
  'auto-update': { flags: [], options: [], summary: 'turn the automatic update cycle on or off' },
  boot: { flags: ['force', 'no-reboot'], options: [], summary: 'arm a boot target and reboot into it' },
  sleep: { flags: ['force'], options: [], summary: 'suspend this machine (refuses while busy without --force)' },
  run: { flags: ['force'], options: [], summary: 'run a configured action' },
  version: { flags: [], options: [], summary: 'print the agent version' },
  help: { flags: [], options: [], summary: 'print this list' },
};

// Flags that take a value rather than standing on their own.
const VALUE_OPTIONS = new Set(['service']);

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
      // The value is the next argument, and it is consumed here so it cannot be
      // mistaken for the command's own positional argument.
      options.set(body, argv[index + 1] ?? '');
      index += 1;
      continue;
    }
    flags.add(body);
  }
  return { flags, options, positional };
}

function helpPayload(ok, error) {
  const payload = {
    ok,
    agentVersion: AGENT_VERSION,
    usage: 'node index.mjs <command> [flags]',
    commands: Object.entries(COMMANDS).map(([name, meta]) => ({
      name,
      flags: [...meta.flags.map((flag) => `--${flag}`), ...meta.options.map((option) => `--${option} <value>`)],
      summary: meta.summary,
    })),
  };
  if (error) payload.error = error;
  return payload;
}

/** Everything `status` says about one service. */
async function describeService(config, service, { isFirst, busy, notes }) {
  const state = serviceState(loadState(), service, isFirst);

  const probe = probeService(service);
  if (probe.error) notes.push(`${service.name}: ${probe.error}`);

  // status must stay fast and must not change anything, so it never runs prefix
  // discovery (up to 15 s, and it writes the probe result to state) and every
  // "what is the newest version" lookup goes through the 10 minute cache.
  let installed = null;
  let latest = { version: null, error: null };
  let staged = null;
  if (service.kind === 'app') {
    installed = appProvider.installedVersion(service);
    latest = await appProvider.cachedLatestVersion(service, { timeoutMs: 4000 });
    // The pair that actually answers "is there an update waiting here": staged is
    // what the app has downloaded, and it is waiting only when it differs from
    // installed. Equal is the normal state.
    staged = appProvider.stagedVersion(service);
  } else if (service.kind === 'npm') {
    installed = npmProvider.installedVersion(service, { allowProbe: false });
    latest = await npmProvider.cachedLatestVersion(service, { timeoutMs: 4000 });
  } else {
    installed = commandProvider.installedVersion(service, { timeoutMs: 4000 });
    latest = await commandProvider.cachedLatestVersion(service, { timeoutMs: 4000 });
  }
  if (!latest.version && latest.error) notes.push(`${service.name}: ${latest.error}`);

  // Skip the health probe when nothing is running: it keeps status inside its
  // budget, and a service that is down cannot be healthy anyway.
  const health = probe.running
    ? await checkHealth(service, { running: true, timeoutMs: 5000 })
    : { ok: false, status: 0, error: `${service.name} is not running` };
  if (!health.ok && health.error) notes.push(`${service.name}: ${health.error}`);

  // A configured relay that is down makes the service unreachable from outside,
  // so it counts against health. An absent relay does not.
  const relayOk = !probe.relay.configured || probe.relay.running;
  if (!relayOk) notes.push(`${service.name}: the relay is configured but not running`);

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
    pendingRestart: Boolean(state.pendingRestart) && (!state.pendingVersion || state.pendingVersion !== installed),
    lastUpdate: state.lastUpdate ?? null,
    canUpdate: service.kind === 'command' ? Boolean(service.update) : true,
    canRestart: canRestart(service),
  };
}

async function commandStatus() {
  const config = loadConfig();
  const notes = [];

  const { entries, busy } = await checkAllBusy(config);
  for (const entry of entries) {
    if (entry.busy.unknown && entry.busy.error) notes.push(`${entry.service.name} busy: ${entry.busy.error}`);
  }

  const services = [];
  for (const [index, entry] of entries.entries()) {
    services.push(
      await describeService(config, entry.service, { isFirst: index === 0, busy: entry.busy, notes }),
    );
  }

  const first = services[0] ?? null;
  // Kept for one release so older apps keep reading the first service. Nothing
  // new should be built on it.
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

  return {
    payload: {
      ok: true,
      os: platformName(),
      system: config.system,
      hostname: os.hostname(),
      agentVersion: AGENT_VERSION,
      services,
      busy,
      bootTargets: bootTargets(config),
      actions: listActions(config),
      autoUpdate: config.autoUpdate === true,
      notes,
      t3,
      pendingRestart: first ? first.pendingRestart : false,
      lastUpdate: first ? first.lastUpdate : null,
      connect: first ? first.relay : { configured: false, running: false },
    },
    exitCode: 0,
  };
}

async function commandBusy() {
  const config = loadConfig();
  const { entries, busy } = await checkAllBusy(config);
  // One service is the common case, and its own busy object carries far more
  // detail than the aggregate ever could, so it is passed straight through
  // rather than repeated under a second key.
  if (entries.length === 1) return { payload: { ok: true, ...entries[0].busy, ...busy }, exitCode: 0 };
  return {
    payload: {
      ok: true,
      ...busy,
      services: entries.map((entry) => ({ id: entry.service.id, name: entry.service.name, ...entry.busy })),
    },
    exitCode: 0,
  };
}

function missingService(config, id) {
  const known = config.services.map((service) => service.id).join(', ') || 'none are configured';
  return {
    payload: {
      ok: false,
      action: 'failed',
      service: id ?? null,
      message: `unknown service: ${id ?? '(none given)'}; this system has ${known}`,
    },
    exitCode: 1,
  };
}

async function commandUpdate(flags, options) {
  const config = loadConfig();
  const service = selectService(config, options.get('service'));
  if (!service) return missingService(config, options.get('service'));

  const result = await runUpdate(config, service, { force: flags.has('force') });
  return {
    payload: {
      ok: result.ok,
      action: result.action,
      service: service.id,
      from: result.from ?? null,
      to: result.to ?? null,
      message: result.message,
    },
    exitCode: result.ok ? 0 : 1,
  };
}

async function commandRestart(flags, options) {
  const config = loadConfig();
  const service = selectService(config, options.get('service'));
  if (!service) return missingService(config, options.get('service'));

  const result = await runRestart(config, service, { force: flags.has('force') });
  return {
    payload: { ok: result.ok, action: result.action, service: service.id, message: result.message },
    exitCode: result.ok ? 0 : 1,
  };
}

function commandAutoUpdate(positional) {
  const value = positional[0];
  if (value !== 'on' && value !== 'off') {
    return {
      payload: { ok: false, error: 'auto-update takes exactly one argument: on or off' },
      exitCode: 1,
    };
  }
  const saved = saveConfig({ autoUpdate: value === 'on' });
  if (!saved.ok) {
    return {
      payload: { ok: false, autoUpdate: loadConfig().autoUpdate, error: 'could not write config.json' },
      exitCode: 1,
    };
  }
  log(`auto-update set to ${value}`, 'auto-update');
  return { payload: { ok: true, autoUpdate: saved.config.autoUpdate === true }, exitCode: 0 };
}

async function commandBoot(positional, flags) {
  const config = loadConfig();
  const target = positional[0];
  const force = flags.has('force');
  const noReboot = flags.has('no-reboot');

  if (target && target === config.system.id) {
    return {
      payload: { ok: true, action: 'noop', target, message: `already running ${config.system.name}` },
      exitCode: 0,
    };
  }

  const configured = bootTargets(config);
  if (configured.length === 0) {
    // A single system machine has nothing to point at, and reaching efibootmgr
    // or bcdedit here would fail with something unreadable instead.
    return {
      payload: {
        ok: false,
        action: 'failed',
        target: target ?? null,
        message: `${config.system.name} has no boot targets configured, so there is no other system to point it at`,
      },
      exitCode: 1,
    };
  }

  if (!target || !findBootTarget(config, target)) {
    return {
      payload: {
        ok: false,
        action: 'failed',
        target: target ?? null,
        message: `boot takes one target: ${configured.map((entry) => entry.id).join(' or ')}`,
      },
      exitCode: 1,
    };
  }

  const { busy } = await checkAllBusy(config);
  if (busy.busy && !force) {
    log(`boot to ${target} deferred: ${busy.reason}`, 'boot');
    return {
      payload: {
        ok: true,
        action: 'deferred',
        target,
        message: `the machine is busy (${busy.reason}); pass --force to reboot anyway`,
      },
      exitCode: 0,
    };
  }

  // Arming is always read back from the firmware. If that verification did not
  // pass we must NOT reboot: a reboot on an unarmed machine just returns to the
  // system we are already on, and reporting that as success sends the controller
  // off waiting for a machine that is never going to appear. Failing here costs
  // nothing.
  const armed = armNextBoot(config, target);
  if (!armed.ok) {
    log(`boot to ${target} failed: ${armed.message}`, 'boot');
    return {
      payload: {
        ok: false,
        action: 'failed',
        target,
        message: `${armed.message}; the machine was left running ${config.system.name} and was not rebooted`,
      },
      exitCode: 1,
    };
  }

  if (noReboot) {
    log(`boot to ${target} armed without rebooting: ${armed.message}`, 'boot');
    return { payload: { ok: true, action: 'armed', target, message: armed.message }, exitCode: 0 };
  }

  const reboot = scheduleReboot(config);
  if (!reboot.ok) {
    log(`boot to ${target} armed but the reboot failed: ${reboot.message}`, 'boot');
    return {
      payload: {
        ok: false,
        action: 'failed',
        target,
        message: `armed (${armed.message}) but the reboot could not be scheduled: ${reboot.message}`,
      },
      exitCode: 1,
    };
  }

  log(`boot to ${target}: ${armed.message}; ${reboot.message}`, 'boot');
  return {
    payload: { ok: true, action: 'rebooting', target, message: `${armed.message}; ${reboot.message}` },
    exitCode: 0,
  };
}

/**
 * Suspend the machine this agent is running on.
 *
 * The busy gate is the same one boot and restart use, and for the same reason:
 * a machine that is asleep is a machine that dropped whatever it was in the
 * middle of. Sleeping is cheap to defer and expensive to get wrong, so without
 * --force a busy machine wins.
 *
 * The success wording is careful on purpose. All we ever learn is that the
 * suspend command was accepted, since the machine goes down underneath us and
 * there is nothing left running to confirm it. Reporting "it slept" on the
 * strength of an exit code would be the exact lie this command must not tell.
 */
async function commandSleep(flags) {
  const config = loadConfig();
  const force = flags.has('force');

  const { busy } = await checkAllBusy(config);
  if (busy.busy && !force) {
    log(`sleep deferred: ${busy.reason}`, 'sleep');
    return {
      payload: {
        ok: true,
        action: 'deferred',
        message: `the machine is busy (${busy.reason}); pass --force to sleep anyway`,
      },
      exitCode: 0,
    };
  }

  const suspended = await suspendMachine(config);
  if (!suspended.ok) {
    log(`sleep failed: ${suspended.message}`, 'sleep');
    return { payload: { ok: false, action: 'failed', message: suspended.message }, exitCode: 1 };
  }

  log(`sleep: ${suspended.message}`, 'sleep');
  return { payload: { ok: true, action: 'sleeping', message: suspended.message }, exitCode: 0 };
}

async function commandRun(positional, flags) {
  const config = loadConfig();
  const result = await runAction(config, positional[0], { force: flags.has('force') });
  return {
    payload: {
      ok: result.ok,
      action: result.action,
      id: result.id,
      exitCode: result.exitCode,
      output: result.output,
      message: result.message,
    },
    exitCode: result.ok ? 0 : 1,
  };
}

async function dispatch(command, positional, flags, options) {
  switch (command) {
    case 'status':
      return commandStatus();
    case 'busy':
      return commandBusy();
    case 'update':
      return commandUpdate(flags, options);
    case 'restart':
      return commandRestart(flags, options);
    case 'auto-update':
      return commandAutoUpdate(positional);
    case 'boot':
      return commandBoot(positional, flags);
    case 'sleep':
      return commandSleep(flags);
    case 'run':
      return commandRun(positional, flags);
    case 'version':
      return { payload: { ok: true, agentVersion: AGENT_VERSION }, exitCode: 0 };
    case 'help':
      return { payload: helpPayload(true), exitCode: 0 };
    default:
      return { payload: helpPayload(false, `unknown command: ${command}`), exitCode: 1 };
  }
}

async function main() {
  const { flags, options, positional } = parseArgs(process.argv.slice(2));
  const command = positional.shift();

  if (!command || flags.has('help')) {
    emit(helpPayload(Boolean(command), command ? undefined : 'no command given'), command ? 0 : 1);
    return;
  }

  const meta = COMMANDS[command];
  if (meta) {
    const unknownFlag = [...flags].find((flag) => !meta.flags.includes(flag));
    const unknownOption = [...options.keys()].find((option) => !meta.options.includes(option));
    if (unknownFlag || unknownOption) {
      emit(
        {
          ok: false,
          error: `${command} does not take --${unknownFlag ?? unknownOption}`,
          accepts: [...meta.flags.map((flag) => `--${flag}`), ...meta.options.map((option) => `--${option} <value>`)],
        },
        1,
      );
      return;
    }
  }

  // Everything except version and help touches a real machine, so refuse early
  // and clearly on a platform this agent was never written for.
  if (!isSupportedPlatform() && command !== 'version' && command !== 'help') {
    emit(
      {
        ok: false,
        os: platformName(),
        agentVersion: AGENT_VERSION,
        error: `legionctl runs on linux, windows and mac only; this host reports "${process.platform}"`,
      },
      1,
    );
    return;
  }

  const { payload, exitCode } = await dispatch(command, positional, flags, options);
  emit(payload, exitCode);
}

main().catch((error) => {
  const message = error?.stack ? String(error.stack).split('\n')[0] : String(error);
  log(`unhandled failure: ${message}`, 'main');
  emit({ ok: false, error: message, agentVersion: AGENT_VERSION }, 1);
});
