// The agent's replies, held to the contract they claim to follow.
//
// The fixtures in contract/fixtures are modelled by hand in contract/tools,
// and for a long time nothing checked that the agent really sends what they
// show. It did not: `op` on an unknown id answered bad-argument with keys no
// schema allows, a machine with no services called itself unmonitored, and a
// replayed running update said `accepted`. Every client was written against
// the fixtures, so each one disagreed with the agent in its own way.
//
// So every reply in this file comes from the real agent, run the way a client
// runs it: one process per command, in a home of its own, against the fixture
// service it can start, stop and update. Every one of them, the setup calls
// included, goes through checkReply() and schemaForCommand() from
// contract/validate.mjs, the same checks every fixture passes, and through
// rule 7's exit code and section 5.6's outcome rule. A reply the contract
// refuses fails here, whatever the scenario around it was asserting.

import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { setTimeout as delay } from 'node:timers/promises';
import { checkReply, schemaForCommand } from '../../contract/validate.mjs';
import { OK_REPLY_ACTIONS } from '../../contract/tools/outcome.mjs';
import { canonicalBytes, canonicalHash } from '../src/controller.mjs';
import { acquireOperationLock } from '../src/lock.mjs';
import {
  cli, commandService, ENTRY, FIXTURE, fixtureConfig, fixtureData, withHome, writeConfig, writeFixture,
} from './helpers.mjs';

// ---------------------------------------------------------------------------
// One invocation, checked
// ---------------------------------------------------------------------------

/** The command a reply answers, as contract/fixtures/index.json names it. */
function commandOf(argv) {
  const [command, sub] = argv;
  if (command === 'config' && (sub === 'set' || sub === 'meta')) return `config ${sub}`;
  if (command === 'policy' && sub === 'set') return 'policy set';
  if (command === 'service-config') return `service-config ${sub}`;
  return command;
}

/**
 * Whether the invocation asks for a change, read from the argv as a client
 * sends it and written apart from the agent's own list, so the two are
 * checked against each other (contract section 5.6).
 */
function asksForChange(argv) {
  const [command, sub] = argv;
  if (['update', 'restart', 'boot', 'sleep', 'run', 'cancel', 'auto-update'].includes(command)) return true;
  if (command === 'cycle') return !argv.includes('--dry-run');
  if (command === 'self-update') return !argv.includes('--check');
  if (['config', 'policy', 'service-config'].includes(command)) return sub === 'set';
  return false;
}

/** The dispatcher's own quoting: POSIX single quotes (contract section 15). */
const quote = (value) => `'${String(value).replaceAll("'", "'\\''")}'`;

const OK_ACTIONS = new Set(OK_REPLY_ACTIONS);

/** Hold one finished run of the agent to the contract, and return its reply. */
function conforms(argv, run, { restricted = false } = {}) {
  const where = `${restricted ? 'restricted ' : ''}${argv.join(' ')}`;
  const output = `${run.stdout}${run.stderr}`;
  assert.equal(run.stdout.indexOf('\n'), run.stdout.length - 1, `${where}: stdout is exactly one JSON object\n${output}`);
  const reply = JSON.parse(run.stdout);

  const problems = checkReply(schemaForCommand(commandOf(argv), reply), reply, { where });
  assert.deepEqual(problems, [], `${where} does not hold:\n${JSON.stringify(reply, null, 2)}`);
  assert.equal(run.exitCode, reply.ok === true || OK_ACTIONS.has(reply.action) ? 0 : 1, `${where}: rule 7 gives the exit code\n${output}`);
  if (asksForChange(argv)) {
    assert.ok('outcome' in reply, `${where}: a reply to a mutating invocation states its outcome`);
    if (reply.op !== null && typeof reply.op === 'object') assert.ok('outcome' in reply.op, `${where}: the record states its outcome`);
  } else {
    assert.ok(!('outcome' in reply), `${where}: a reply that changes nothing carries no outcome`);
  }
  return reply;
}

/**
 * Run the agent once and check the reply. With `restricted`, the argv arrives
 * the way a restricted key sends it: as SSH_ORIGINAL_COMMAND, through the
 * dispatcher, behind the agent's own interpreter and entry point.
 */
function invoke(home, argv, { stdin = null, env = {}, restricted = false } = {}) {
  const run = restricted
    ? cli(['dispatch'], {
      home,
      stdin,
      env: { ...env, LEGIONCTL_RESTRICTED: '1', SSH_ORIGINAL_COMMAND: [process.execPath, ENTRY, ...argv].map(quote).join(' ') },
    })
    : cli(argv, { home, stdin, env });
  return conforms(argv, run, { restricted });
}

/** The same, for a run that is left going while the test does something else. */
function invokeLater(home, argv) {
  const child = spawn(process.execPath, [ENTRY, ...argv], { env: { ...process.env, LEGIONCTL_HOME: home } });
  let stdout = '';
  let stderr = '';
  child.stdout.on('data', (chunk) => { stdout += chunk; });
  child.stderr.on('data', (chunk) => { stderr += chunk; });
  return new Promise((resolve, reject) => {
    child.on('error', reject);
    child.on('close', (exitCode) => {
      try {
        resolve(conforms(argv, { stdout, stderr, exitCode }));
      } catch (error) {
        reject(error);
      }
    });
  });
}

/** Wait until an operation's record says `state`, without asking the agent. */
async function recordReaches(home, id, state) {
  const file = path.join(home, 'ops', `${id}.json`);
  const deadline = Date.now() + 30000;
  while (Date.now() < deadline) {
    try {
      if (JSON.parse(fs.readFileSync(file, 'utf8')).state === state) return;
    } catch {
      /* not written yet, or mid-rename */
    }
    await delay(50);
  }
  throw new Error(`operation ${id} never reached ${state}`);
}

/**
 * Follow an operation the way a client does, with `op ID --wait 20`, until it
 * finishes. Every poll is held to the contract like any other reply, and a
 * slow worker launch (Windows starts it through WMI) only costs another poll.
 */
function untilFinished(home, id) {
  for (let poll = 0; poll < 4; poll += 1) {
    const reply = invoke(home, ['op', id, '--wait', '20']);
    if (reply.op.state === 'finished') return reply;
  }
  throw new Error(`operation ${id} did not finish within four polls`);
}

/** Hold the machine lock the way a running update does, for the length of `body`. */
async function whileLocked(body) {
  const held = acquireOperationLock({ kind: 'update', opId: 'cccccccc-2222-4222-8222-222222222222', service: 'demo' });
  assert.equal(held.ok, true, held.message);
  try {
    return await body();
  } finally {
    held.lock.release();
  }
}

// ---------------------------------------------------------------------------
// Machines to run against
// ---------------------------------------------------------------------------

const node = (source) => [process.execPath, '-e', source];
const call = (dir, verb) => [process.execPath, FIXTURE, verb, dir];

/** One fixture service, with whatever the scenario needs it to do. */
function oneService(home, { initial = {}, service = {} } = {}) {
  const dir = fixtureData(home, initial);
  writeConfig(home, fixtureConfig(dir, { services: [commandService(dir, service)] }));
  return dir;
}

/**
 * A setup document every client accepts (contract section 9.7), with its
 * lineage, and with `systems` when it should carry a launch value.
 */
function setupText({ id = 'setup-conformance', revision, lineage = [], name = 'Beacon', systems = undefined }) {
  const document = {
    version: 1,
    controller: { id, revision, lineage, source: 'cli' },
    machines: [{ id: 'pi', name, ...(systems ? { systems } : {}), endpoints: [{ id: 'lan', host: '192.0.2.40', user: 'me' }] }],
  };
  return `${JSON.stringify(document, null, 2)}\n`;
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

test('status conforms with no config, every busy kind, a broken config and a restricted key', async () => {
  await withHome(async (home) => {
    invoke(home, ['status']);
    invoke(home, ['busy']);
  });

  await withHome(async (home) => {
    const dir = fixtureData(home);
    writeConfig(home, fixtureConfig(dir, {
      services: [
        commandService(dir, { id: 'declared', name: 'Declared', busy: { type: 'none' } }),
        commandService(dir),
        commandService(dir, { id: 'nobody', name: 'Nobody', busy: undefined }),
      ],
    }));
    const status = invoke(home, ['status']);
    const evidence = Object.fromEntries(status.services.map((service) => [service.id, service.busy]));
    // Declaring "none" is an answer, so it counts as monitored; only the
    // service nobody described blocks for want of a probe.
    assert.equal(evidence.declared.monitored, true);
    assert.equal(evidence.nobody.monitored, false);
    assert.equal(status.busy.monitoredServices, 2);
    assert.equal(status.busy.unmonitoredServices, 1);
    invoke(home, ['status'], { restricted: true });
  });

  await withHome(async (home) => {
    fs.writeFileSync(path.join(home, 'config.json'), '{ "services": [ broken');
    invoke(home, ['status']);
    invoke(home, ['doctor']);
    // Nothing that would change the machine runs on a config nobody can read,
    // and the refusal is a refusal, not an update that pretends to have failed.
    const refused = invoke(home, ['update']);
    assert.equal(refused.reasonCode, 'config-invalid');
    assert.equal(refused.outcome, 'failed');
  });
});

test('a process probe that fails is reported as unknown, never as stopped', async () => {
  await withHome(async (home) => {
    oneService(home, { service: { process: { type: 'command', running: node('setTimeout(() => {}, 10000)') } } });
    // The running probe never answers inside the budget. Section 8: a process
    // reading is evidence only when its error is null.
    const status = invoke(home, ['status', '--budget-ms', '2000']);
    const { process: probe } = status.services[0];
    assert.notEqual(probe.error, null, 'a probe that did not answer says why');
    assert.equal(probe.state, 'unknown');
  });
});

test('busy conforms with no services, one, and several of every kind', async () => {
  await withHome(async (home) => {
    writeConfig(home, fixtureConfig(fixtureData(home), { services: [] }));
    const none = invoke(home, ['busy']);
    assert.equal(none.monitored, true, 'nothing here is waiting for a probe to be described');
    assert.equal(none.evidence, 'none');
    assert.equal(none.busy, false);
    assert.equal(none.monitoredServices + none.unmonitoredServices, 0);
  });

  await withHome(async (home) => {
    oneService(home, { service: { busy: { type: 'none' } } });
    const declared = invoke(home, ['busy']);
    assert.equal(declared.monitored, true);
    assert.equal(declared.monitoredServices, 1);
    assert.equal(declared.unmonitoredServices, 0);
  });

  await withHome(async (home) => {
    const dir = fixtureData(home, { busy: 'on' });
    writeConfig(home, fixtureConfig(dir, {
      services: [
        commandService(dir),
        commandService(dir, { id: 'declared', name: 'Declared', busy: { type: 'none' } }),
        commandService(dir, { id: 'nobody', name: 'Nobody', busy: undefined }),
      ],
    }));
    const several = invoke(home, ['busy']);
    assert.equal(several.busy, true);
    assert.equal(several.monitoredServices, 2);
    assert.equal(several.unmonitoredServices, 1);
    invoke(home, ['busy'], { restricted: true });
  });
});

// ---------------------------------------------------------------------------
// Mutations
// ---------------------------------------------------------------------------

test('update conforms through every ending a fixture service can produce', async () => {
  await withHome(async (home) => {
    oneService(home, { initial: { latest: '2.0.0' } });
    const id = 'conform-update-0001';
    const updated = invoke(home, ['update', '--op', id]);
    assert.equal(updated.action, 'updated');
    assert.equal(updated.outcome, 'succeeded');
    assert.equal(updated.op.outcome, 'succeeded');
    assert.equal(updated.op.result.outcome, 'succeeded');

    const replayed = invoke(home, ['update', '--op', id]);
    assert.equal(replayed.replayed, true);
    assert.equal(replayed.outcome, 'succeeded');

    assert.equal(invoke(home, ['update']).action, 'noop');

    // An id that already names a different request is a bad --op, refused
    // before anything else, never replayed and never a lock conflict.
    const reused = invoke(home, ['restart', '--op', id]);
    assert.equal(reused.reasonCode, 'bad-argument');
    assert.equal(reused.argument, '--op');
    assert.equal(reused.outcome, 'failed');

    assert.equal(invoke(home, ['update', '--service', 'nowhere']).reasonCode, 'unknown-service');
    const queued = invoke(home, ['update', '--when-idle', '--expires', '30m']);
    assert.equal(queued.outcome, 'pending');
    assert.equal(queued.op.outcome, 'pending');
  });

  await withHome(async (home) => {
    oneService(home, { initial: { latest: '2.0.0', busy: 'on' } });
    const deferred = invoke(home, ['update']);
    assert.equal(deferred.action, 'deferred');
    assert.equal(deferred.outcome, 'deferred');
    assert.equal(deferred.ok, true);
    // The record says the same thing in its own terms: it finished with a
    // reason, so its result is not the plain success `ok` promises there.
    assert.equal(deferred.op.result.ok, false);
    assert.equal(deferred.op.outcome, 'deferred');
    assert.equal(invoke(home, ['update', '--force']).action, 'updated');
  });

  await withHome(async (home) => {
    const dir = fixtureData(home, { latest: '2.0.0', 'update-noop': 'on', 'rollback-to': '1.0.0' });
    writeConfig(home, fixtureConfig(dir, { services: [commandService(dir, { verify: call(dir, 'verify') })] }));
    const rolled = invoke(home, ['update']);
    assert.equal(rolled.action, 'rolled-back');
    assert.equal(rolled.outcome, 'failed');
  });

  await withHome(async (home) => {
    oneService(home, { initial: { 'latest-fails': 'on' } });
    const unknownLatest = invoke(home, ['update', '--force']);
    assert.equal(unknownLatest.reasonCode, 'latest-unknown');
    assert.equal(unknownLatest.outcome, 'failed', 'nobody can say the service is current, so it was not a no-op');
  });

  await withHome(async (home) => {
    oneService(home, { initial: { latest: '2.0.0' } });
    await whileLocked(async () => {
      const conflict = invoke(home, ['update']);
      assert.equal(conflict.action, 'conflict');
      assert.equal(conflict.ok, true);
      assert.equal(conflict.outcome, 'deferred');
    });
  });
});

test('a replay says what the operation is doing, and a detached one keeps its shape', async () => {
  await withHome(async (home) => {
    // An updater that takes a few seconds, so there is time to ask again.
    const dir = fixtureData(home, { latest: '2.0.0', 'slow-seconds': '3' });
    writeConfig(home, fixtureConfig(dir, { services: [commandService(dir, { update: call(dir, 'slow') })] }));

    // A run in its own session, still going: the same id answers
    // already-running, which a client follows exactly as it follows accepted.
    const spawned = 'conform-spawned-0001';
    const first = invokeLater(home, ['update', '--op', spawned]);
    await recordReaches(home, spawned, 'running');
    const running = invoke(home, ['update', '--op', spawned]);
    assert.equal(running.action, 'already-running');
    assert.equal(running.reasonCode, 'already-running');
    assert.equal(running.replayed, true);
    assert.equal(running.detached, false);
    assert.equal(running.outcome, 'pending');
    await first;

    const detached = 'conform-detached-0001';
    const accepted = invoke(home, ['update', '--detach', '--op', detached]);
    assert.equal(accepted.action, 'accepted');
    assert.equal(accepted.outcome, 'pending');
    assert.equal(accepted.service, 'demo');
    assert.ok('from' in accepted && 'to' in accepted, '--detach answers in the update shape');
    assert.notEqual(accepted.op.phase, null, 'a record that has not reached a phase yet is in its first');

    const again = invoke(home, ['update', '--detach', '--op', detached]);
    assert.equal(again.action, 'already-running');
    assert.equal(again.detached, true);
    assert.equal(invoke(home, ['update', '--op', detached]).action, 'already-running');
    assert.equal(invoke(home, ['op', detached]).op.outcome, 'pending');

    const cancel = invoke(home, ['cancel', detached]);
    assert.equal(cancel.action, 'conflict');
    assert.equal(cancel.reasonCode, 'already-running');

    const waited = untilFinished(home, detached);
    assert.notEqual(waited.op.outcome, 'pending');
    assert.equal(waited.op.outcome, waited.op.result.outcome);
    const finished = invoke(home, ['update', '--detach', '--op', detached]);
    assert.equal(finished.replayed, true);
    assert.equal(finished.outcome, waited.op.outcome);
  });
});

test('restart conforms when it runs, defers, queues and meets a lock', async () => {
  await withHome(async (home) => {
    const dir = oneService(home);
    assert.equal(invoke(home, ['restart']).action, 'restarted');
    writeFixture(dir, 'busy', 'on');
    assert.equal(invoke(home, ['restart']).action, 'deferred');
    assert.equal(invoke(home, ['restart', '--when-idle']).action, 'queued');
    await whileLocked(async () => {
      assert.equal(invoke(home, ['restart']).action, 'conflict');
    });
  });
});

test('run conforms when it runs, fails, times out, is unknown and is detached', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home, { 'slow-seconds': '5' });
    writeConfig(home, fixtureConfig(dir, {
      actions: [
        { id: 'say', command: call(dir, 'action') },
        { id: 'broken', command: call(dir, 'no-such-verb') },
        { id: 'slow', command: call(dir, 'slow'), timeoutSeconds: 1 },
      ],
    }));
    assert.equal(invoke(home, ['run', 'say']).action, 'ran');
    const broken = invoke(home, ['run', 'broken']);
    assert.equal(broken.exitCode, 9);
    assert.equal(broken.outcome, 'failed');
    const slow = invoke(home, ['run', 'slow']);
    assert.equal(slow.reasonCode, 'timed-out');
    assert.equal(slow.outcome, 'unknown', 'a run stopped at its time limit may have done its work');
    assert.equal(invoke(home, ['run', 'nothing']).reasonCode, 'unknown-action');

    const id = 'conform-run-0001';
    const accepted = invoke(home, ['run', 'say', '--detach', '--op', id]);
    assert.equal(accepted.action, 'accepted');
    assert.equal(accepted.id, 'say');
    assert.equal(untilFinished(home, id).op.outcome, 'succeeded');
    const replayed = invoke(home, ['run', 'say', '--detach', '--op', id]);
    assert.equal(replayed.action, 'ran');
    assert.equal(replayed.replayed, true);
  });
});

test('boot and sleep conform without ever rebooting or suspending anything', async () => {
  await withHome(async (home) => {
    const marker = path.join(home, 'reboot-requested');
    writeConfig(home, fixtureConfig(fixtureData(home), {
      services: [],
      boot: {
        targets: { other: { method: 'command', arm: node('process.exit(0)'), verify: node('process.exit(0)') } },
        reboot: node(`require('node:fs').writeFileSync(${JSON.stringify(marker)}, 'requested')`),
      },
    }));
    assert.equal(invoke(home, ['boot', 'other', '--no-reboot']).action, 'armed');
    assert.equal(invoke(home, ['boot', 'test']).action, 'noop');
    assert.equal(invoke(home, ['boot', 'elsewhere']).reasonCode, 'unknown-target');

    // A queued boot names no service, and the cycle that runs it still has to
    // be able to say so.
    assert.equal(invoke(home, ['boot', 'other', '--when-idle', '--no-reboot']).action, 'queued');
    const drained = invoke(home, ['cycle']);
    assert.deepEqual(drained.queued.map((entry) => [entry.service, entry.action]), [[null, 'armed']]);

    // A real sleep is never invoked: it leaves a record waiting out a power
    // transition. Queued is as far as it goes, and cancelling it is a mutation
    // of its own.
    const sleep = invoke(home, ['sleep', '--when-idle']);
    assert.equal(sleep.action, 'queued');
    const cancelled = invoke(home, ['cancel', sleep.op.id]);
    assert.equal(cancelled.action, 'cancelled');
    assert.equal(cancelled.outcome, 'succeeded');
    assert.equal(cancelled.op.outcome, 'deferred', 'the request itself was dropped by design');
    assert.equal(fs.existsSync(marker), false);
  });
});

test('cycle conforms with a deferred child, as a dry run, and against a lock', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home, { latest: '2.0.0' });
    writeConfig(home, fixtureConfig(dir, {
      services: [
        commandService(dir),
        commandService(dir, { id: 'paused', name: 'Paused', updates: { automatic: false } }),
      ],
    }));
    const dry = invoke(home, ['cycle', '--dry-run']);
    assert.equal(dry.action, 'skipped', 'a preview is not a cycle that ran');

    const cycled = invoke(home, ['cycle']);
    assert.equal(cycled.action, 'cycled');
    const deferredChild = cycled.children.find((child) => child.service === 'paused');
    assert.equal(deferredChild.opId, null);
    assert.equal(deferredChild.action, 'deferred');

    await whileLocked(async () => {
      assert.equal(invoke(home, ['cycle']).action, 'conflict');
    });
  });
});

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

test('op, cancel, history, logs and doctor conform for queued, finished and unknown ids', async () => {
  await withHome(async (home) => {
    const dir = oneService(home, { initial: { busy: 'on' } });
    const queued = invoke(home, ['restart', '--when-idle']).op.id;
    assert.equal(invoke(home, ['op', queued]).op.state, 'queued');

    const unknown = '11111111-2222-4333-8444-555555555555';
    const missing = invoke(home, ['op', unknown]);
    assert.equal(missing.reasonCode, 'not-configured');
    assert.equal(missing.op, null);
    const missingCancel = invoke(home, ['cancel', unknown]);
    assert.equal(missingCancel.reasonCode, 'not-configured');
    assert.equal(missingCancel.outcome, 'failed');
    const missingLogs = invoke(home, ['logs', '--op', unknown]);
    assert.equal(missingLogs.reasonCode, 'not-configured');
    assert.equal(missingLogs.argument, '--op');

    assert.equal(invoke(home, ['cancel', queued]).action, 'cancelled');
    const finished = invoke(home, ['op', queued, '--wait', '1']);
    assert.equal(finished.op.state, 'finished');
    assert.equal(invoke(home, ['cancel', queued]).action, 'conflict');

    const history = invoke(home, ['history']);
    assert.ok(history.operations.every((summary) => typeof summary.outcome === 'string'));
    invoke(home, ['logs']);
    invoke(home, ['logs', '--op', queued]);
    invoke(home, ['doctor']);
    invoke(home, ['bundle']);

    // Every check that is not ok offers a fix, the ones a narrowed doctor and
    // a stopped service produce as much as any other.
    invoke(home, ['doctor', '--service', 'nowhere']);
    writeFixture(dir, 'running', 'off');
    const stopped = invoke(home, ['doctor']);
    assert.equal(stopped.checks.find((entry) => entry.id === 'service.demo.health').level, 'warn');
  });
});

// ---------------------------------------------------------------------------
// The setup document
// ---------------------------------------------------------------------------

test('config conforms through a whole lineage, and serves exactly the bytes it hashes', async () => {
  await withHome(async (home) => {
    oneService(home);
    const set = (text, ...flags) => invoke(home, ['config', 'set', ...flags], { stdin: text });
    const first = setupText({ revision: 1 });
    const firstHash = canonicalHash(first);

    const empty = invoke(home, ['config']);
    assert.equal(empty.raw, null);
    assert.equal(empty.bytes, 0);
    invoke(home, ['config', 'meta']);

    assert.equal(set(first, '--controller-id', 'setup-conformance', '--revision', '1').action, 'stored');
    const read = invoke(home, ['config']);
    assert.equal(read.raw, canonicalBytes(first).toString('utf8'), 'raw is the canonical text, not a re-serialisation');
    assert.equal(read.hash, firstHash);
    assert.equal(read.bytes, Buffer.byteLength(read.raw, 'utf8'));
    invoke(home, ['config', 'meta']);

    assert.equal(set(first).action, 'noop');
    const second = setupText({ revision: 2, lineage: [firstHash] });
    assert.equal(set(second).action, 'stored');
    assert.equal(set(first).reasonCode, 'stale-revision');
    const diverged = set(setupText({ revision: 2, lineage: [firstHash], name: 'Elsewhere' }));
    assert.equal(diverged.reasonCode, 'controller-conflict');
    assert.equal(diverged.divergent, true);
    const other = setupText({ id: 'setup-other', revision: 1 });
    assert.equal(set(other).reasonCode, 'controller-conflict');
    assert.equal(set(other, '--replace').action, 'replaced');

    assert.equal(set(first, '--controller-id', 'setup-conformance').reasonCode, 'bad-argument');
    assert.equal(set('{ not json').reasonCode, 'bad-argument');

    // Refused by the lock, the document was not stored: exit 1, and the
    // outcome tells the client to push again later.
    await whileLocked(async () => {
      const locked = set(setupText({ id: 'setup-other', revision: 2, lineage: [canonicalHash(other)] }));
      assert.equal(locked.reasonCode, 'operation-in-progress');
      assert.equal(locked.ok, false);
      assert.equal(locked.outcome, 'deferred');
    });

    // These answer according to the document checks a restricted session and
    // section 9.7 add; whichever way they go, the reply has to hold.
    set('{"version": 1}\n');
    set('{"version": 1, "version": 1, "machines": []}\n');
    const launcher = { id: 'linux', agent: ['/usr/bin/node', '/home/me/.legion-control/bin/launcher.mjs'] };
    invoke(home, ['config', 'set'], {
      restricted: true,
      stdin: setupText({ id: 'setup-other', revision: 2, lineage: [canonicalHash(other)], name: 'Restricted', systems: [launcher] }),
    });
  });
});

test('a hand-placed setup file is served as its canonical text', async () => {
  await withHome(async (home) => {
    oneService(home);
    const handWritten = '﻿{\r\n  "version": 1,\r\n  "machines": []\r\n}\r\n\r\n';
    fs.writeFileSync(path.join(home, 'controller.json'), handWritten);
    const read = invoke(home, ['config']);
    assert.deepEqual(read.controller, { version: 1, machines: [] }, 'a byte order mark does not stop the document parsing');
    assert.equal(read.raw, '{\n  "version": 1,\n  "machines": []\n}\n');
    assert.equal(read.hash, canonicalHash(handWritten));
  });
});

// ---------------------------------------------------------------------------
// Policy, self-update, administration and the rest
// ---------------------------------------------------------------------------

test('policy and auto-update conform for every switch', async () => {
  await withHome(async (home) => {
    oneService(home);
    invoke(home, ['policy']);
    invoke(home, ['policy', '--service', 'demo']);
    assert.equal(invoke(home, ['policy', 'set'], { stdin: '{"automatic": false}' }).outcome, 'succeeded');
    invoke(home, ['policy', 'set', '--service', 'demo'], { stdin: '{"automatic": true}' });
    assert.equal(invoke(home, ['policy', 'set'], { stdin: '{"nonsense": 1}' }).outcome, 'failed');
    for (const switched of [['on'], ['off'], ['pause', '4h'], ['resume']]) {
      assert.equal(invoke(home, ['auto-update', ...switched]).outcome, 'succeeded');
    }
    assert.equal(invoke(home, ['auto-update']).outcome, 'failed');
    invoke(home, ['policy'], { restricted: true });
  });
});

test('self-update conforms for a bad bundle, an empty rollback and restricted shapes', async () => {
  await withHome(async (home) => {
    oneService(home);
    const junk = path.join(home, 'junk.tar.gz');
    fs.writeFileSync(junk, 'not a signed bundle');
    invoke(home, ['self-update', '--check', '--from', junk]);
    assert.equal(invoke(home, ['self-update', '--from', junk]).outcome, 'failed');
    assert.equal(invoke(home, ['self-update', '--rollback']).outcome, 'failed');
    assert.equal(invoke(home, ['self-update', '--from', junk], { restricted: true }).outcome, 'failed');
    invoke(home, ['self-update', '--check', '--stdin', '--install=1'], { restricted: true, stdin: 'not a bundle' });
  });
});

test('service-config, version and help conform', async () => {
  await withHome(async (home) => {
    oneService(home);
    const current = invoke(home, ['service-config', 'get']);
    const edited = { ...current.document, updates: { ...current.document.updates, automatic: false } };
    const request = JSON.stringify({ expectedHash: current.hash, document: edited });
    invoke(home, ['service-config', 'validate', '--stdin'], { stdin: request });
    assert.equal(invoke(home, ['service-config', 'set', '--stdin'], { stdin: request }).outcome, 'succeeded');
    const stale = invoke(home, ['service-config', 'set', '--stdin'], { stdin: request });
    assert.equal(stale.reasonCode, 'stale-revision');
    assert.equal(stale.outcome, 'failed');
    invoke(home, ['service-config', 'get'], { restricted: true });
    invoke(home, ['version']);
    invoke(home, ['version', '--check']);
    invoke(home, ['help']);
  });
});

test('a restricted key gets the same shapes as an ordinary session', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home, { latest: '2.0.0' });
    writeConfig(home, fixtureConfig(dir, { actions: [{ id: 'say', command: call(dir, 'action') }] }));
    const ran = invoke(home, ['run', 'say'], { restricted: true });
    invoke(home, ['update'], { restricted: true });
    invoke(home, ['status'], { restricted: true });
    invoke(home, ['busy'], { restricted: true });
    invoke(home, ['op', ran.op.id], { restricted: true });
    invoke(home, ['history'], { restricted: true });
    invoke(home, ['logs'], { restricted: true });
    invoke(home, ['logs', '--op', ran.op.id], { restricted: true });
  });
});
