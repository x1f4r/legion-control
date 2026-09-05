// Everything that can be wrong with a machine, checked in one go.
//
// The value of this command is that a person who cannot make the app work has
// somewhere to look that is not "read the source". So every check answers three
// things: what was tested, what the answer was, and — when it is not ok — what to
// type to fix it. A check with no fix line is a check that leaves someone stuck.
//
// It never changes anything. Not the config, not the state, not the service.

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createRequire } from 'node:module';
import {
  AGENT_VERSION,
  basePath,
  configPath,
  detectPlatform,
  lastGoodInfo,
  operationsDir,
  readJsonDocument,
  runCommand,
  stateDir,
  userHome,
} from './config.mjs';
import { CONTRACT_VERSION } from './contract.mjs';
import { selectSleepCommand } from './boot.mjs';
import { readController } from './controller.mjs';
import { checkEndpoint } from './endpoint.mjs';
import { currentHolder } from './lock.mjs';
import { checkServiceBusy } from './probes/busy.mjs';
import { checkHealth } from './probes/health.mjs';
import { listOperations } from './operations.mjs';
import { describeSystemUpdates, scheduledEligibility } from './policy.mjs';
import { livenessOf, probeService } from './service.mjs';
import { trustFingerprint } from './trust.mjs';
import * as commandProvider from './providers/command.mjs';
import * as npmProvider from './providers/npm.mjs';
import * as appProvider from './providers/app.mjs';

/** The oldest Node this agent is tested on. Below it, node:sqlite may be absent. */
export const MIN_NODE_MAJOR = 24;

function check(id, level, summary, detail = null, fix = null) {
  return { id, level, summary, detail, fix };
}

function nodeMajor() {
  return Number.parseInt(process.versions.node.split('.')[0], 10);
}

function configChecks(loaded) {
  const checks = [];
  const file = configPath();

  if (loaded.source === 'defaults' && !fs.existsSync(file)) {
    checks.push(
      check(
        'config.parse',
        'warn',
        'there is no config.json',
        `${file} does not exist, so this machine looks after nothing and automatic updates are off`,
        'write a config.json describing the services this machine should look after; examples/agent-config-*.json are a starting point',
      ),
    );
    checks.push(check('config.validate', 'ok', 'nothing to validate', 'an unconfigured machine is inert on purpose', null));
    return checks;
  }

  if (!loaded.ok && !loaded.config) {
    const lastGood = lastGoodInfo();
    checks.push(
      check(
        'config.parse',
        'fail',
        'config.json cannot be read',
        loaded.error,
        lastGood
          ? `the last copy that loaded cleanly is at ${lastGood.path} (${lastGood.at}); compare it and copy it back if it is still right`
          : 'fix the JSON; every mutating command is refused until it parses',
      ),
    );
    checks.push(check('config.validate', 'fail', 'not reached', 'the file has to parse first', null));
    return checks;
  }

  let size = null;
  try {
    size = fs.statSync(file).size;
  } catch {
    /* a defaults-sourced config has no file */
  }
  checks.push(
    check('config.parse', 'ok', 'config.json parses', size === null ? null : `${size} bytes`, null),
  );

  const config = loaded.config;
  const problems = [...(loaded.problems ?? [])];
  if (problems.length > 0) {
    checks.push(
      check(
        'config.validate',
        'fail',
        `${problems.length} problem${problems.length === 1 ? '' : 's'} in config.json`,
        problems.map((problem) => `${problem.path}: ${problem.message}`).join(' | '),
        problems.map((problem) => problem.fix).filter(Boolean).join(' | ') || 'fix the paths listed above',
      ),
    );
  } else {
    const notes = [...(loaded.warnings ?? []), ...(loaded.migrations ?? [])];
    checks.push(
      check(
        'config.validate',
        notes.length > 0 ? 'warn' : 'ok',
        `${config.services.length} service${config.services.length === 1 ? '' : 's'}, ${
          Object.keys(config.boot.targets).length
        } boot target${Object.keys(config.boot.targets).length === 1 ? '' : 's'}, ${config.actions.length} action${
          config.actions.length === 1 ? '' : 's'
        }`,
        notes.length > 0 ? notes.map((note) => `${note.path}: ${note.message}`).join(' | ') : null,
        notes.map((note) => note.fix).filter(Boolean).join(' | ') || null,
      ),
    );
  }
  return checks;
}

function environmentChecks() {
  const checks = [];
  const base = basePath();

  let writable = false;
  let writeError = null;
  try {
    fs.mkdirSync(base, { recursive: true });
    const probe = path.join(base, `.doctor-${process.pid}`);
    fs.writeFileSync(probe, 'x');
    fs.rmSync(probe, { force: true });
    writable = true;
  } catch (err) {
    writeError = err.message;
  }
  checks.push(
    writable
      ? check('base.writable', 'ok', `${base} is writable`, null, null)
      : check(
          'base.writable',
          'fail',
          `${base} cannot be written`,
          writeError,
          'the agent keeps its state, operation records and locks here; fix the ownership or permissions of that directory',
        ),
  );

  const major = nodeMajor();
  checks.push(
    major >= MIN_NODE_MAJOR
      ? check('node.version', 'ok', `node ${process.versions.node}`, `the agent is tested on ${MIN_NODE_MAJOR} and newer`, null)
      : check(
          'node.version',
          'fail',
          `node ${process.versions.node} is older than ${MIN_NODE_MAJOR}`,
          'node:sqlite and the async runner both need a newer Node',
          `install Node ${MIN_NODE_MAJOR} or newer and point the scheduler at it`,
        ),
  );

  let sqlite = false;
  try {
    sqlite = typeof createRequire(import.meta.url)('node:sqlite').DatabaseSync === 'function';
  } catch {
    sqlite = false;
  }
  checks.push(
    sqlite
      ? check('node.sqlite', 'ok', 'node:sqlite is available', 'needed by the t3-sqlite busy probe', null)
      : check(
          'node.sqlite',
          'warn',
          'node:sqlite is not available in this Node build',
          'a t3-sqlite busy probe will report "busy state unknown", which blocks disruptive work',
          `use Node ${MIN_NODE_MAJOR} or newer, or configure a different busy probe`,
        ),
  );

  // Both directories are reported under ops.dir: they fail for the same reason
  // and have the same fix, and a person reading this wants one answer about
  // whether the agent can write down what it did.
  const unwritable = [];
  for (const dir of [operationsDir(), stateDir()]) {
    try {
      fs.mkdirSync(dir, { recursive: true });
      fs.accessSync(dir, fs.constants.W_OK);
    } catch {
      unwritable.push(dir);
    }
  }
  checks.push(
    unwritable.length === 0
      ? check('ops.dir', 'ok', 'the operation and state directories are writable', `${operationsDir()}, ${stateDir()}`, null)
      : check(
          'ops.dir',
          'fail',
          `${unwritable.join(' and ')} cannot be written`,
          'operations, their outcomes and the per-service state all live here',
          'fix the ownership or permissions; without them nothing can be recorded and every mutation is refused',
        ),
  );

  // The lock is held by the operating system, so this reports what the mutex
  // says rather than what a pid in a file claims. The case worth catching here is
  // a lock database that cannot provide exclusion at all — left root-owned by a
  // run under sudo, or replaced by something that is not a database — because an
  // operation would otherwise discover it at the worst moment.
  const holder = currentHolder();
  if (holder?.lockError) {
    checks.push(
      check('locks', 'fail', 'the operation lock cannot be used', holder.lockError, holder.lockHint ?? 'fix the ownership of the lock database'),
    );
  } else if (!holder) {
    checks.push(check('locks', 'ok', 'no operation is running', 'the machine-wide lock is free', null));
  } else if (holder.held) {
    checks.push(
      check(
        'locks',
        'ok',
        `a ${holder.kind ?? ''} operation is running`.replace('  ', ' '),
        `pid ${holder.pid ?? 'unknown'}, phase ${holder.phase ?? 'unknown'}`,
        null,
      ),
    );
  } else {
    checks.push(
      check(
        'locks',
        'warn',
        'a leftover lock note is present, but nothing holds the lock',
        `${holder.kind ?? 'an operation'} from pid ${holder.pid ?? 'unknown'} ended without tidying up`,
        'harmless; the next operation or cycle removes it',
      ),
    );
  }

  checks.push(
    check(
      'session.restricted',
      'ok',
      process.env.LEGIONCTL_RESTRICTED === '1' ? 'this is a restricted session' : 'this is an ordinary session',
      [
        process.env.LEGIONCTL_CLIENT ? `client: ${process.env.LEGIONCTL_CLIENT}` : null,
        `signed bundles are verified against release key ${trustFingerprint().slice(0, 8)} and nothing else`,
      ]
        .filter(Boolean)
        .join('; '),
      null,
    ),
  );

  // A clock that is badly wrong makes every timestamp, every expiry and every
  // maintenance window meaningless, and it is invisible until it bites.
  const drift = Math.abs(Date.now() - Date.parse(new Date().toISOString()));
  checks.push(
    check(
      'clock',
      Number.isFinite(drift) ? 'ok' : 'warn',
      `the machine clock reads ${new Date().toISOString()}`,
      `local offset ${-new Date().getTimezoneOffset() / 60} h; maintenance windows are local time`,
      null,
    ),
  );

  return checks;
}

/**
 * Whether the scheduler on this machine actually runs a cycle, and when it last
 * did. This is the check that catches the single most common silent failure: a
 * timer still pointing at the 2.x `update` command, which updates one service.
 */
function schedulerChecks(config) {
  const checks = [];
  const entry = path.join(basePath(), 'agent', 'src', 'index.mjs');

  if (process.platform === 'linux') {
    const listed = runCommand('systemctl', ['--user', 'is-enabled', 'legion-control-update.timer'], { timeoutMs: 10000 });
    const state = listed.stdout.trim();
    checks.push(
      state === 'enabled'
        ? check('scheduler.present', 'ok', 'legion-control-update.timer is enabled', null, null)
        : check(
            'scheduler.present',
            'fail',
            `legion-control-update.timer is ${state || 'not installed'}`,
            'nothing will update or recover this machine on its own',
            'run agent/install/install-linux.sh, then: systemctl --user enable --now legion-control-update.timer',
          ),
    );

    const unit = runCommand('systemctl', ['--user', 'cat', 'legion-control-update.service'], { timeoutMs: 10000 });
    const execLine = unit.stdout.split('\n').find((line) => line.startsWith('ExecStart=')) ?? '';
    const runsCycle = /\bcycle\b/.test(execLine);
    const runsThisAgent = execLine.includes(entry);
    checks.push(
      runsCycle && runsThisAgent
        ? check('scheduler.command', 'ok', 'the timer runs this agent\'s cycle', execLine.trim(), null)
        : check(
            'scheduler.command',
            'warn',
            runsCycle ? 'the timer runs a different agent' : 'the timer still runs the 2.x "update" command',
            execLine.trim() || 'ExecStart could not be read',
            runsCycle
              ? `point ExecStart at ${entry}`
              : '"update" only touches the first service and never runs recovery or the queue; re-run agent/install/install-linux.sh to switch it to "cycle"',
          ),
    );
  } else if (process.platform === 'win32') {
    const task = runCommand('schtasks', ['/query', '/tn', 'Legion Control Update', '/xml'], { timeoutMs: 20000 });
    const runsCycle = /\bcycle\b/.test(task.stdout);
    checks.push(
      task.ok
        ? check('scheduler.present', 'ok', 'the "Legion Control Update" task exists', null, null)
        : check(
            'scheduler.present',
            'fail',
            'the "Legion Control Update" task is missing',
            'nothing will update or recover this machine on its own',
            'run agent/install/install-windows.ps1',
          ),
    );
    checks.push(
      !task.ok
        ? check('scheduler.command', 'fail', 'not reached', 'the task has to exist first', 'run agent/install/install-windows.ps1')
        : runsCycle
          ? check('scheduler.command', 'ok', 'the task runs this agent\'s cycle', null, null)
          : check(
              'scheduler.command',
              'warn',
              'the task still runs the 2.x "update" command',
              null,
              're-run agent/install/install-windows.ps1 to switch it to "cycle"',
            ),
    );
  } else if (process.platform === 'darwin') {
    const label = 'com.x1f4r.legion-control.update';
    const listed = runCommand('launchctl', ['list', label], { timeoutMs: 10000 });
    checks.push(
      listed.ok
        ? check('scheduler.present', 'ok', `the ${label} job is loaded`, null, null)
        : check(
            'scheduler.present',
            'fail',
            `the ${label} job is not loaded`,
            'nothing will update or recover this machine on its own',
            `load it with: launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/${label}.plist`,
          ),
    );

    const plist = path.join(userHome(), 'Library', 'LaunchAgents', `${label}.plist`);
    let contents = '';
    try {
      contents = fs.readFileSync(plist, 'utf8');
    } catch {
      contents = '';
    }
    checks.push(
      !contents
        ? check(
            'scheduler.command',
            'warn',
            'the launch agent could not be read',
            `${plist} is not there`,
            'reinstall the launch agent; without it nothing runs on a schedule',
          )
        : /<string>cycle<\/string>/.test(contents)
          ? check('scheduler.command', 'ok', "the launch agent runs this agent's cycle", null, null)
          : check(
              'scheduler.command',
              'warn',
              'the launch agent still runs the 2.x "update" command',
              plist,
              '"update" only touches the first service and never runs recovery or the queue; reinstall the launch agent to switch it to "cycle"',
            ),
    );
  } else {
    checks.push(
      check(
        'scheduler.present',
        'warn',
        `this agent does not know how ${process.platform} schedules work`,
        'nothing here can confirm that a maintenance cycle runs on its own',
        'run "legionctl cycle" from whatever scheduler this system uses, every 15 minutes',
      ),
    );
    checks.push(
      check(
        'scheduler.command',
        'warn',
        'not checked',
        `there is no scheduler this agent recognises on ${process.platform}`,
        'confirm by hand that something runs "legionctl cycle" regularly',
      ),
    );
  }

  // What the scheduler would actually do right now, per service, alongside when
  // it last ran. Together these answer "why has this machine not updated", which
  // is the question this check exists for.
  const updates = describeSystemUpdates(config, {});
  const wouldDo = config.services
    .map((service) => {
      const policy = scheduledEligibility(config, service, {});
      return policy.eligible ? `${service.id}: would update now` : `${service.id}: ${policy.reason}`;
    })
    .join(' | ');
  const policyDetail = [
    updates.automatic ? 'automatic updates are on' : 'automatic updates are OFF for this machine',
    updates.maintenanceWindows.length > 0
      ? `windows ${updates.maintenanceWindows.map((window) => `${window.days.join('/')} ${window.from}-${window.to}`).join(', ')}, in one now: ${updates.inWindowNow}`
      : 'no maintenance windows, so any time is allowed',
    wouldDo,
  ]
    .filter(Boolean)
    .join(' | ');

  const lastCycle = listOperations({ limit: 50, kind: 'cycle' })[0] ?? null;
  checks.push(
    lastCycle
      ? check(
          'scheduler.lastRun',
          updates.automatic ? 'ok' : 'warn',
          `the last cycle ran at ${lastCycle.finishedAt ?? lastCycle.requestedAt}`,
          `${lastCycle.result?.action ?? lastCycle.state}: ${lastCycle.result?.message ?? 'still running'} | ${policyDetail}`,
          updates.automatic ? null : 'a person can still update on request; "legionctl auto-update on" turns the schedule back on',
        )
      : check(
          'scheduler.lastRun',
          'warn',
          'no cycle has ever been recorded on this machine',
          `either the scheduler has not fired yet, or it is not running this agent | ${policyDetail}`,
          'run "legionctl cycle" once by hand and check that a record appears in "legionctl history"',
        ),
  );

  return checks;
}

function privilegeChecks(config) {
  const checks = [];
  if (process.platform !== 'linux') {
    checks.push(check('privileges.sudo', 'ok', `no sudo rules are needed on ${process.platform}`, null, null));
    checks.push(check('privileges.boot', 'ok', 'not applicable', null, null));
    return checks;
  }

  const sudo = runCommand('sudo', ['-n', '-l', '/usr/bin/systemctl', 'reboot'], { timeoutMs: 10000 });
  checks.push(
    sudo.ok
      ? check('privileges.sudo', 'ok', 'passwordless sudo allows "systemctl reboot"', null, null)
      : check(
          'privileges.sudo',
          'warn',
          'passwordless sudo will not run "systemctl reboot"',
          'boot and sleep will fail rather than silently doing nothing',
          'add exactly: <user> ALL=(root) NOPASSWD: /usr/bin/systemctl reboot, /usr/bin/systemctl suspend',
        ),
  );

  const targets = Object.values(config?.boot?.targets ?? {});
  const needsEfi = targets.some((target) => target.method === 'efi-bootnext');
  if (!needsEfi) {
    checks.push(check('privileges.boot', 'ok', 'no firmware boot target is configured', null, null));
  } else {
    const efi = runCommand('sudo', ['-n', '-l', '/usr/bin/efibootmgr'], { timeoutMs: 10000 });
    checks.push(
      efi.ok
        ? check('privileges.boot', 'ok', 'passwordless sudo allows efibootmgr', null, null)
        : check(
            'privileges.boot',
            'warn',
            'passwordless sudo will not run efibootmgr',
            'arming a BootNext target will fail, and the machine will not be rebooted',
            'add: <user> ALL=(root) NOPASSWD: /usr/bin/efibootmgr',
          ),
    );
  }
  return checks;
}

async function serviceChecks(config, service, { deep }) {
  const checks = [];
  const id = service.id;

  const probe = probeService(service);
  checks.push(
    probe.error
      ? check(`service.${id}.process`, 'fail', `the process of ${service.name} could not be probed`, probe.error, 'fix the process block in config.json, or the permissions it needs')
      : probe.running
        ? check(`service.${id}.process`, 'ok', `${service.name} is running`, `state ${probe.unitState}`, null)
        : check(`service.${id}.process`, 'warn', `${service.name} is not running`, `state ${probe.unitState}`, `start it with "legionctl restart --service ${id}"`),
  );

  const health = probe.running
    ? await checkHealth(service, { running: true, useAsync: true })
    : { ok: false, status: 0, error: 'the process is not running' };
  checks.push(
    health.ok
      ? check(`service.${id}.health`, 'ok', `${service.name} answers`, health.status ? `HTTP ${health.status}` : null, null)
      : check(
          `service.${id}.health`,
          probe.running ? 'fail' : 'warn',
          `${service.name} does not answer`,
          health.error,
          probe.running ? 'check the service log; the process is up but not serving' : null,
        ),
  );

  const busy = await checkServiceBusy(service, { liveness: livenessOf(service) });
  checks.push(
    busy.monitored === false && busy.evidence === 'unmonitored'
      ? check(
          `service.${id}.busy`,
          'warn',
          `${service.name} has no busy probe`,
          'every disruptive action on this machine is refused while a service cannot say whether it is busy',
          `add "busy": {"type": "none"} to services[${id}] if it is never busy, or a real probe if it is`,
        )
      : busy.unknown
        ? check(`service.${id}.busy`, 'fail', `the busy state of ${service.name} cannot be read`, busy.error, 'fix the busy probe; until then disruptive actions are refused')
        : check(`service.${id}.busy`, 'ok', `busy probe answers (${busy.evidence})`, busy.reason, null),
  );

  if (service.relay) {
    checks.push(
      probe.relay.running
        ? check(`service.${id}.relay`, 'ok', 'the relay is running', null, null)
        : check(
            `service.${id}.relay`,
            'warn',
            'the relay is configured but not running',
            'the service is reachable on this machine and not from outside it',
            'start the tunnel, or remove the relay block if it is no longer used',
          ),
    );
  }

  if (deep) {
    let latest = { version: null, error: null };
    if (service.kind === 'npm') latest = npmProvider.fetchLatestVersion(service, { timeoutMs: 20000 });
    else if (service.kind === 'app') latest = await appProvider.fetchLatestVersion(service, { timeoutMs: 20000 });
    else latest = commandProvider.fetchLatestVersion(service, { timeoutMs: 20000 });

    checks.push(
      latest.version
        ? check(`service.${id}.latest`, 'ok', `the newest version is ${latest.version}`, null, null)
        : check(
            `service.${id}.latest`,
            'fail',
            'the newest version cannot be resolved',
            latest.error ?? 'nothing was returned',
            service.kind === 'command'
              ? 'check the latestVersion command; without it an update can never be confirmed'
              : 'check that this machine can reach the registry or release feed',
          ),
    );

    if (service.endpoint) {
      const reached = await checkEndpoint(service.endpoint);
      checks.push(
        reached.reachable
          ? check(`service.${id}.endpoint`, 'ok', `${service.endpoint.url} answers`, `HTTP ${reached.status} in ${reached.elapsedMs} ms`, null)
          : check(
              `service.${id}.endpoint`,
              'fail',
              `${service.endpoint.url} is not reachable from this machine`,
              reached.error,
              'the service is running locally; the tunnel, DNS or firewall between it and the outside is what to look at',
            ),
      );
    }
  }

  return checks;
}

/**
 * Run every check. `service` narrows it to one; `deep` adds the checks that go
 * out to the network.
 */
export async function runDoctor(loaded, { service = null, deep = false } = {}) {
  const startedAt = Date.now();
  const raw = readJsonDocument(loaded.path ?? configPath());
  const sanitize = diagnosticRedactor(loaded.config, raw.state === 'ok' ? raw.value : null);
  const checks = [...configChecks(loaded), ...environmentChecks()];

  const config = loaded.config;
  if (config) {
    checks.push(...schedulerChecks(config));
    checks.push(...privilegeChecks(config));

    const selected = service ? config.services.filter((entry) => entry.id === service) : config.services;
    if (service && selected.length === 0) {
      checks.push(
        check(
          `service.${service}.process`,
          'fail',
          `this machine has no service called ${service}`,
          `it has: ${config.services.map((entry) => entry.id).join(', ') || 'none'}`,
          null,
        ),
      );
    }
    for (const entry of selected) checks.push(...(await serviceChecks(config, entry, { deep })));

    if (config.services.some((entry) => entry.kind === 'npm')) {
      const npm = npmProvider.runNpm(['--version'], { timeoutMs: 20000 });
      checks.push(
        npm.ok
          ? check('npm.available', 'ok', `npm ${npm.stdout.trim()}`, null, null)
          : check('npm.available', 'fail', 'npm cannot be run', npm.stderr.trim() || npm.error, 'install npm, or correct the npmPrefix'),
      );
      for (const entry of config.services.filter((service) => service.kind === 'npm')) {
        const prefix = npmProvider.resolveNpmPrefix(entry, { allowProbe: false });
        const installed = npmProvider.installedVersionAt(entry, prefix);
        checks.push(
          installed
            ? check('npm.prefix', 'ok', `${entry.package} ${installed} is installed under ${prefix}`, null, null)
            : check(
                'npm.prefix',
                'warn',
                `${entry.package} is not installed under ${prefix}`,
                'the agent looks here for the installed version',
                `set npmPrefix on services[${entry.id}] if npm installs somewhere else`,
              ),
        );
      }
    }

    // The suspend tool, because a sleep button that silently does nothing is the
    // worst failure this agent has: the command exits 0 and the machine stays on.
    const chosen = selectSleepCommand(detectPlatform(), config.sleep ?? {});
    checks.push(
      chosen.ok
        ? check('sleep.tool', 'ok', `sleep uses ${chosen.command[0]}`, chosen.command.join(' '), null)
        : check(
            'sleep.tool',
            'warn',
            'no suspend tool was found',
            chosen.message,
            'install one of the tools listed above, or set sleep.command in config.json',
          ),
    );

    const stored = readController();
    checks.push(
      !stored.hash
        ? check('controller.copy', 'warn', 'this machine carries no setup document', 'a client will push one the first time it connects', null)
        : stored.error
          ? check('controller.copy', 'fail', 'the stored setup document cannot be read', stored.error, 'a client will push over it; until then this machine cannot answer setup questions')
          : check(
              'controller.copy',
              'ok',
              `setup ${stored.meta.id ?? 'without an identity'} at revision ${stored.meta.revision}`,
              `${stored.meta.lineage?.length ?? 0} ancestors recorded, last written by ${stored.meta.source ?? 'an unknown client'}`,
              null,
            ),
    );
  }

  const counts = { ok: 0, warn: 0, fail: 0 };
  for (const entry of checks) counts[entry.level] += 1;

  return {
    ok: counts.fail === 0,
    deep,
    checks: checks.map((entry) => ({ ...entry, summary: sanitize(entry.summary), detail: sanitize(entry.detail), fix: sanitize(entry.fix) })),
    counts,
    elapsedMs: Date.now() - startedAt,
  };
}

const REDACTED = '<redacted>';
const sensitiveKey = (key) => /token|secret|password|passwd|apikey|authorization|credential|identityfile|privatekey|^auth$|^key$/i.test(key.replace(/[_-]/g, ''));

/** Collect configured values only; this cannot identify arbitrary output secrets. */
function diagnosticRedactor(...configs) {
  const values = new Set();
  const remember = (value) => {
    if (typeof value === 'string' && value.length > 0) {
      values.add(value);
      // Both raw text and JSON-escaped echoes can occur in captured output.
      values.add(JSON.stringify(value).slice(1, -1));
    }
  };
  const collect = (value, sensitive = false) => {
    if (typeof value === 'string') {
      if (sensitive) remember(value);
      try {
        const url = new URL(value);
        for (const credential of [url.username, url.password]) {
          remember(credential);
          try { remember(decodeURIComponent(credential)); } catch { /* retain encoded value */ }
        }
        for (const [key, inner] of url.searchParams) if (sensitiveKey(key)) remember(inner);
        for (const pair of url.search.slice(1).split('&')) {
          const separator = pair.indexOf('=');
          if (separator < 0) continue;
          try {
            if (sensitiveKey(decodeURIComponent(pair.slice(0, separator).replace(/\+/g, ' ')))) remember(pair.slice(separator + 1));
          } catch { /* malformed query encoding is retained as unknown text */ }
        }
      } catch { /* most config strings are not URLs */ }
    } else if (Array.isArray(value)) {
      const argv = value.every((item) => typeof item === 'string');
      value.forEach((item, index) => {
        collect(item, sensitive || (argv && index > 0));
        if (argv && index > 0) {
          const assignment = /^[^=\s]+=(.*)$/s.exec(item);
          if (assignment) remember(assignment[1]);
        }
      });
    } else if (value && typeof value === 'object') {
      for (const [key, inner] of Object.entries(value)) collect(inner, sensitive || sensitiveKey(key));
    }
  };
  configs.forEach((config) => collect(config));
  const escaped = [...values].sort((a, b) => b.length - a.length).map((value) => value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'));
  const pattern = escaped.length ? new RegExp(escaped.join('|'), 'g') : null;
  const walk = (value) => {
    if (typeof value === 'string') return pattern ? value.split(REDACTED).map((part) => part.replace(pattern, () => REDACTED)).join(REDACTED) : value;
    if (Array.isArray(value)) return value.map(walk);
    if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).map(([key, inner]) => [key, walk(inner)]));
    return value;
  };
  return walk;
}

/** Keep executable names for diagnosis; every subsequent argument is private. */
export function redactConfig(config) {
  if (!config) return null;
  const walk = (value, key = '') => {
    if (sensitiveKey(key)) return REDACTED;
    if (Array.isArray(value)) {
      if (value.every((item) => typeof item === 'string')) return value.map((item, index) => index === 0 ? item : REDACTED);
      return value.map((item) => walk(item));
    }
    if (value && typeof value === 'object') {
      const out = {};
      for (const [name, inner] of Object.entries(value)) out[name] = walk(inner, name);
      return out;
    }
    return value;
  };

  return diagnosticRedactor(config)(walk(config));
}

/** The diagnostic bundle: doctor, status, history, logs and a sanitized config. */
export async function buildBundle({ doctor, status, history, logs, config, rawConfig = null }) {
  void AGENT_VERSION;
  void CONTRACT_VERSION;
  void os;
  const sanitize = diagnosticRedactor(config, rawConfig);
  const evidence = sanitize({ doctor, status, history, logs });
  return {
    generatedAt: new Date().toISOString(),
    ...evidence,
    config: sanitize(redactConfig(config)),
    redacted: [
      'configured command arguments after the executable',
      'configured credential and identity-file values, including their exact echoes in diagnostic evidence',
      'unrecognized secrets originating only in command output may remain; inspect the export before sharing',
    ],
  };
}
