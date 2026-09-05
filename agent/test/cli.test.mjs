// The whole agent, driven the way a client drives it: one process per command,
// one JSON object out, against a real service it can start, stop and update.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { cli, FIXTURE, fixtureConfig, fixtureData, readFixture, withHome, writeConfig, writeFixture } from './helpers.mjs';

/** A home with the fixture service configured and running. */
async function withService(body, { config = null, initial = {} } = {}) {
  return withHome(async (home) => {
    const dir = fixtureData(home, initial);
    writeConfig(home, config ? config(dir) : fixtureConfig(dir));
    return body({ home, dir });
  });
}

test('status describes the service, its process, health and busy evidence apart', async () => {
  await withService(async ({ home }) => {
    const { payload, exitCode } = cli(['status'], { home });
    assert.equal(exitCode, 0);
    assert.equal(payload.contract, 3);
    assert.equal(payload.agentVersion, '3.0.1');

    const service = payload.services[0];
    assert.equal(service.installed, '1.0.0');
    assert.equal(service.process.running, true);
    assert.equal(service.health.ok, true);
    assert.equal(service.busy.evidence, 'command');
    assert.equal(service.busy.busy, false);
    assert.equal(service.endpoint.reachable, null, 'status never probes the outside world');
    assert.equal(payload.timing.partial, false);
    assert.ok(payload.timing.elapsedMs >= 0);
  });
});

test('an update runs, and reports the version it actually read', async () => {
  await withService(
    async ({ home, dir }) => {
      const { payload, exitCode } = cli(['update'], { home });
      assert.equal(exitCode, 0);
      assert.equal(payload.action, 'updated');
      assert.equal(payload.from, '1.0.0');
      assert.equal(payload.to, '2.0.0');
      assert.equal(payload.verified, true);
      assert.equal(readFixture(dir, 'installed'), '2.0.0');
      assert.equal(payload.op.state, 'finished');
      assert.equal(payload.op.result.action, 'updated');
    },
    { initial: { latest: '2.0.0' } },
  );
});

test('a busy machine defers, and --force goes ahead', async () => {
  await withService(
    async ({ home, dir }) => {
      const deferred = cli(['update'], { home });
      assert.equal(deferred.exitCode, 0, 'deferring is an answer, not an error');
      assert.equal(deferred.payload.action, 'deferred');
      assert.equal(deferred.payload.reasonCode, 'busy');
      assert.equal(readFixture(dir, 'installed'), '1.0.0');

      const forced = cli(['update', '--force'], { home });
      assert.equal(forced.payload.action, 'updated');
      assert.equal(readFixture(dir, 'installed'), '2.0.0');
    },
    { initial: { latest: '2.0.0', busy: 'on' } },
  );
});

test('a drain runs before the stop, and work started during it still defers', async () => {
  await withService(
    async ({ home, dir }) => {
      const { payload } = cli(['update'], { home });
      assert.equal(readFixture(dir, 'drain-runs'), '1', 'the drain has to run');
      // The drain made the machine busy, which is exactly the window the second
      // check exists for: work that began after the first check said idle.
      assert.equal(payload.action, 'deferred');
      assert.equal(readFixture(dir, 'installed'), '1.0.0', 'nothing may be installed after that');
    },
    {
      initial: { latest: '2.0.0', 'drain-makes-busy': 'on' },
      config: (dir) => {
        const document = fixtureConfig(dir);
        document.services[0].drain = { command: [process.execPath, FIXTURE, 'drain', dir], timeoutSeconds: 10 };
        return document;
      },
    },
  );
});

test('an updater that does nothing is caught, rolled back, and --force does not rescue it', async () => {
  await withService(
    async ({ home, dir }) => {
      const { payload, exitCode } = cli(['update'], { home });
      assert.equal(exitCode, 1);
      assert.equal(payload.action, 'rolled-back');
      assert.equal(payload.verified, false);
      assert.match(payload.message, /still on 1\.0\.0, not 2\.0\.0/);
      assert.equal(payload.to, '1.0.0', 'the version reported is the one actually installed');
      assert.equal(readFixture(dir, 'rollback-runs'), '1');

      // --force overrides the busy gate and nothing else.
      const forced = cli(['update', '--force'], { home });
      assert.equal(forced.payload.action, 'rolled-back');
      assert.equal(forced.payload.verified, false);
    },
    { initial: { latest: '2.0.0', 'update-noop': 'on', 'rollback-to': '1.0.0' } },
  );
});

test('an update whose result cannot be confirmed at all is refused before it touches anything', async () => {
  await withService(
    async ({ home, dir }) => {
      const { payload } = cli(['update', '--force'], { home });
      assert.equal(payload.action, 'noop');
      assert.equal(payload.reasonCode, 'postcondition-failed');
      assert.match(payload.message, /--force overrides the busy gate, not the evidence/);
      assert.equal(readFixture(dir, 'update-runs', '0'), '0', 'nothing may run when nothing could confirm it');
    },
    {
      config: (dir) => {
        const document = fixtureConfig(dir);
        delete document.services[0].latestVersion;
        delete document.services[0].installedVersion;
        return document;
      },
    },
  );
});

test('a version lookup that fails is a no-op that --force cannot push past', async () => {
  await withService(
    async ({ home, dir }) => {
      const { payload } = cli(['update', '--force'], { home });
      assert.equal(payload.action, 'noop');
      assert.equal(payload.reasonCode, 'latest-unknown');
      assert.equal(readFixture(dir, 'update-runs', '0'), '0', 'installing an unnamed version is a guess, not an update');
    },
    { initial: { 'latest-fails': 'on' } },
  );
});

test('the same operation id twice does the work once', async () => {
  await withService(
    async ({ home, dir }) => {
      const id = 'aaaaaaaa-2222-4222-8222-222222222222';
      const first = cli(['update', '--op', id], { home });
      assert.equal(first.payload.action, 'updated');
      assert.equal(readFixture(dir, 'update-runs'), '1');

      const second = cli(['update', '--op', id], { home });
      assert.equal(second.payload.replayed, true);
      assert.equal(second.payload.action, 'updated');
      assert.equal(readFixture(dir, 'update-runs'), '1', 'a retry must not run the update again');
    },
    { initial: { latest: '2.0.0' } },
  );
});

test('an operation id names one intent; reusing it for another is a conflict', async () => {
  await withService(async ({ home }) => {
    const id = 'bbbbbbbb-2222-4222-8222-222222222222';
    cli(['restart', '--op', id], { home });
    const clash = cli(['update', '--op', id], { home });
    assert.equal(clash.payload.action, 'conflict');
    assert.match(clash.payload.message, /binds one intent/);
  });
});

test('a queued request waits, is visible, and the cycle runs it when idle', async () => {
  await withService(
    async ({ home, dir }) => {
      const queued = cli(['update', '--when-idle', '--expires', '30m'], { home });
      assert.equal(queued.payload.action, 'queued');
      assert.ok(queued.payload.op.expiresAt);

      const status = cli(['status'], { home });
      assert.equal(status.payload.operations.queued.length, 1);

      // Still busy: the cycle leaves it queued rather than forcing it.
      const waiting = cli(['cycle'], { home });
      assert.equal(waiting.payload.queued[0].action, 'queued');
      assert.equal(readFixture(dir, 'installed'), '1.0.0');

      writeFixture(dir, 'busy', 'off');
      const ran = cli(['cycle'], { home });
      assert.equal(ran.payload.queued[0].action, 'updated');
      assert.equal(readFixture(dir, 'installed'), '2.0.0');
      assert.equal(cli(['status'], { home }).payload.operations.queued.length, 0);
    },
    { initial: { latest: '2.0.0', busy: 'on' } },
  );
});

test('a queued request can be cancelled before it runs', async () => {
  await withService(async ({ home }) => {
    const queued = cli(['restart', '--when-idle'], { home });
    const id = queued.payload.op.id;
    const cancelled = cli(['cancel', id], { home });
    assert.equal(cancelled.payload.action, 'cancelled');
    assert.equal(cli(['status'], { home }).payload.operations.queued.length, 0);
  });
});

test('with the scheduler off, a person can still update', async () => {
  await withService(
    async ({ home, dir }) => {
      cli(['auto-update', 'off'], { home });
      const dry = cli(['cycle', '--dry-run'], { home });
      assert.equal(dry.payload.plan[0].wouldRun, false);
      assert.equal(dry.payload.plan[0].reasonCode, 'policy-off');
      assert.equal(readFixture(dir, 'installed'), '1.0.0');

      // The whole of finding 06: turning the schedule off must not remove the
      // safe manual path and leave --force as the only way to update.
      const manual = cli(['update'], { home });
      assert.equal(manual.payload.action, 'updated');
      assert.equal(readFixture(dir, 'installed'), '2.0.0');
    },
    { initial: { latest: '2.0.0' } },
  );
});

test('the cycle walks every service, not just the first', async () => {
  await withHome(async (home) => {
    const first = fixtureData(home, { latest: '2.0.0' });
    const second = path.join(home, 'fixture-two');
    fs.mkdirSync(second, { recursive: true });
    for (const [name, value] of Object.entries({ installed: '1.0.0', latest: '3.0.0', running: 'on' })) {
      fs.writeFileSync(path.join(second, name), value);
    }
    const document = fixtureConfig(first);
    const clone = JSON.parse(JSON.stringify(document.services[0]));
    for (const key of ['installedVersion', 'latestVersion', 'update', 'rollback']) {
      clone[key] = clone[key].map((part) => (part === first ? second : part));
    }
    for (const key of ['running', 'start', 'stop']) {
      clone.process[key] = clone.process[key].map((part) => (part === first ? second : part));
    }
    clone.health.command = clone.health.command.map((part) => (part === first ? second : part));
    clone.busy.command = clone.busy.command.map((part) => (part === first ? second : part));
    clone.id = 'second';
    clone.name = 'Second';
    document.services.push(clone);
    writeConfig(home, document);

    const { payload } = cli(['cycle'], { home });
    assert.equal(payload.action, 'cycled');
    assert.equal(payload.children.length, 2, 'a machine with two services must have both walked');
    assert.deepEqual(payload.children.map((child) => child.action), ['updated', 'updated']);
    assert.equal(readFixture(first, 'installed'), '2.0.0');
    assert.equal(readFixture(second, 'installed'), '3.0.0');
  });
});

test('a second mutation while one is running answers conflict rather than racing it', async () => {
  await withService(async ({ home }) => {
    // Hold the machine lock the way a running operation does.
    const { acquireOperationLock } = await import(`../src/lock.mjs?cli=${Date.now()}`);
    const previous = process.env.LEGIONCTL_HOME;
    process.env.LEGIONCTL_HOME = home;
    const held = acquireOperationLock({ kind: 'update', opId: 'cccccccc-2222-4222-8222-222222222222', service: 'demo' });
    try {
      assert.equal(held.ok, true);
      const blocked = cli(['restart'], { home });
      assert.equal(blocked.payload.action, 'conflict');
      assert.equal(blocked.payload.reasonCode, 'operation-in-progress');
      assert.equal(blocked.payload.conflict.kind, 'update');
      assert.equal(blocked.exitCode, 0, 'a conflict is an answer, not a crash');

      // --force is about the busy gate, never about another operation's lock.
      const forced = cli(['restart', '--force'], { home });
      assert.equal(forced.payload.action, 'conflict');
    } finally {
      held.lock.release();
      if (previous === undefined) delete process.env.LEGIONCTL_HOME;
      else process.env.LEGIONCTL_HOME = previous;
    }
  });
});

test('an interrupted operation is recovered by the next cycle, and the service put back', async () => {
  await withService(async ({ home, dir }) => {
    const ops = await import(`../src/operations.mjs?cli=${Date.now()}`);
    const previous = process.env.LEGIONCTL_HOME;
    process.env.LEGIONCTL_HOME = home;
    try {
      // Exactly what a run killed by its scheduler's time limit leaves behind:
      // a record stuck in "stopping", and a service that never came back.
      const id = 'dddddddd-2222-4222-8222-222222222222';
      ops.beginOperation({ id, kind: 'update', service: 'demo', systemId: 'test' });
      ops.recordPhase(id, 'stopping');
      ops.updateOperation(id, { pid: 99999999 });
      writeFixture(dir, 'running', 'off');

      const { payload } = cli(['cycle'], { home });
      assert.equal(payload.recovered.length, 1);
      assert.equal(payload.recovered[0].action, 'interrupted');
      assert.equal(readFixture(dir, 'running'), 'on', 'a service left stopped has to be put back');

      const record = cli(['op', id], { home });
      assert.equal(record.payload.op.state, 'finished');
      assert.equal(record.payload.op.result.action, 'interrupted');
      assert.match(record.payload.op.result.message, /stopping/);
    } finally {
      if (previous === undefined) delete process.env.LEGIONCTL_HOME;
      else process.env.LEGIONCTL_HOME = previous;
    }
  });
});

test('a broken config refuses every mutation and explains every refusal', async () => {
  await withService(async ({ home }) => {
    fs.writeFileSync(path.join(home, 'config.json'), '{"services": [');
    for (const command of [['update'], ['restart'], ['cycle'], ['run', 'x'], ['sleep']]) {
      const { payload, exitCode } = cli(command, { home });
      assert.equal(exitCode, 1, `${command[0]} should have been refused`);
      assert.equal(payload.reasonCode, 'config-invalid');
      assert.match(payload.message, /cannot be used/);
    }
    // ...while everything read-only still answers, and says why.
    const status = cli(['status'], { home });
    assert.equal(status.payload.config.ok, false);
    assert.equal(status.payload.busy.busy, true, 'a machine nobody can read is not a machine to install on');
    assert.equal(cli(['doctor'], { home }).payload.checks.some((c) => c.id === 'config.parse' && c.level === 'fail'), true);
  });
});

test('every argument that arrives over ssh is validated', async () => {
  await withService(async ({ home }) => {
    for (const args of [
      ['update', '--service', 't3; rm -rf ~'],
      ['run', '$(whoami)'],
      ['op', 'not a uuid'],
      ['update', '--op', 'sh ort'],
      ['update', '--expires', 'whenever'],
    ]) {
      const { payload } = cli(args, { home });
      assert.equal(payload.reasonCode, 'bad-argument', `${args.join(' ')} should have been refused`);
    }
    const unknownFlag = cli(['status', '--wharever'], { home });
    assert.equal(unknownFlag.payload.reasonCode, 'bad-argument');
    assert.ok(unknownFlag.payload.accepts.length > 0, 'a refusal has to say what would work');
  });
});

test('an unknown service, action and boot target each say what does exist', async () => {
  await withService(async ({ home }) => {
    const service = cli(['update', '--service', 'nope'], { home });
    assert.equal(service.payload.reasonCode, 'unknown-service');
    assert.deepEqual(service.payload.accepts, ['demo']);

    const action = cli(['run', 'nope'], { home });
    assert.equal(action.payload.reasonCode, 'unknown-action');

    const target = cli(['boot', 'nope'], { home });
    assert.equal(target.payload.reasonCode, 'not-configured');
  });
});

test('history, logs and the diagnostic bundle answer without changing anything', async () => {
  await withService(
    async ({ home }) => {
      cli(['update'], { home });

      const history = cli(['history', '--limit', '5'], { home });
      assert.ok(history.payload.operations.length >= 1);
      assert.deepEqual(history.payload.filter, { service: null, kind: null });

      const filtered = cli(['history', '--kind', 'update'], { home });
      assert.ok(filtered.payload.operations.every((entry) => entry.kind === 'update'));

      const logs = cli(['logs', '--lines', '10'], { home });
      assert.equal(logs.payload.source, 'agent');
      assert.ok(logs.payload.lines.every((entry) => 'at' in entry && 'command' in entry && 'line' in entry));

      const opLogs = cli(['logs', '--op', history.payload.operations[0].id], { home });
      assert.equal(opLogs.payload.source, 'operation');

      const bundle = cli(['bundle'], { home });
      assert.ok(bundle.payload.doctor && bundle.payload.status && bundle.payload.history && bundle.payload.logs);
      assert.ok(bundle.payload.redacted.length > 0);
    },
    { initial: { latest: '2.0.0' } },
  );
});

test('a diagnostic bundle does not carry anything that looks like a credential', async () => {
  await withService(
    async ({ home }) => {
      const { payload } = cli(['bundle'], { home });
      const text = JSON.stringify(payload.config);
      assert.equal(text.includes('hunter2-the-actual-token'), false, 'a token in an argv array must not be exported');
      assert.match(text, /<redacted>/);
    },
    {
      config: (dir) => {
        const document = fixtureConfig(dir);
        document.actions = [
          { id: 'deploy', name: 'Deploy', command: ['/usr/bin/curl', '--token', 'hunter2-the-actual-token', 'https://example.com'] },
        ];
        return document;
      },
    },
  );
});

test('policy is read and set, and only the keys named change', async () => {
  await withService(async ({ home }) => {
    const read = cli(['policy'], { home });
    assert.equal(read.payload.service, null);
    assert.equal(read.payload.updates.automatic, true);
    assert.equal(read.payload.services.length, 1, 'a machine read carries every service, to save a round trip each');

    const windows = cli(['policy', 'set'], {
      home,
      stdin: JSON.stringify({ maintenanceWindows: [{ days: ['mon'], from: '02:00', to: '06:00' }] }),
    });
    assert.deepEqual(windows.payload.changed, ['maintenanceWindows']);

    const paused = cli(['policy', 'set', '--service', 'demo'], { home, stdin: JSON.stringify({ pauseUntil: '2030-01-01T00:00:00Z' }) });
    assert.equal(paused.payload.updates.pauseUntil, '2030-01-01T00:00:00.000Z');
    assert.equal(paused.payload.updates.deferredReason, 'policy-paused');
    assert.equal(paused.payload.updates.inheritedKeys.automatic, true, 'setting a pause must not also pin the automatic switch');

    // The windows set earlier are still there.
    assert.equal(cli(['policy'], { home }).payload.updates.maintenanceWindows.length, 1);

    const rubbish = cli(['policy', 'set'], { home, stdin: '{"nonsense":1}' });
    assert.equal(rubbish.payload.reasonCode, 'bad-argument');
  });
});

test('auto-update pause and resume are the same setting the policy reports', async () => {
  await withService(async ({ home }) => {
    const paused = cli(['auto-update', 'pause', '4h'], { home });
    assert.ok(paused.payload.updates.pauseUntil);
    const resumed = cli(['auto-update', 'resume'], { home });
    assert.equal(resumed.payload.updates.pauseUntil, null);
    assert.equal(cli(['auto-update', 'nonsense'], { home }).payload.reasonCode, 'bad-argument');
  });
});

test('a configured action runs and its output comes back', async () => {
  await withService(async ({ home, dir }) => {
    const { payload } = cli(['run', 'count'], { home });
    assert.equal(payload.action, 'ran');
    assert.equal(payload.exitCode, 0);
    assert.match(payload.output, /action output line/);
    assert.equal(readFixture(dir, 'action-runs'), '1');
  }, {
    config: (dir) => {
      const document = fixtureConfig(dir);
      document.actions = [{ id: 'count', name: 'Count', command: [process.execPath, FIXTURE, 'action', dir], busyGated: true }];
      return document;
    },
  });
});

test('version --check loads every module without touching the machine', async () => {
  await withService(async ({ home }) => {
    const { payload, exitCode } = cli(['version', '--check'], { home });
    assert.equal(exitCode, 0);
    assert.equal(payload.selfTest.ok, true);
    assert.equal(payload.contract, 3);
  });
});

test('doctor reports the scheduler, the busy source and the lock, with fixes', async () => {
  await withService(async ({ home }) => {
    const { payload } = cli(['doctor'], { home });
    const ids = payload.checks.map((check) => check.id);
    for (const id of ['config.parse', 'config.validate', 'base.writable', 'node.version', 'locks', 'ops.dir', 'clock', 'session.restricted', 'controller.copy', 'sleep.tool', 'scheduler.present', 'scheduler.command', 'scheduler.lastRun', 'service.demo.process', 'service.demo.health', 'service.demo.busy']) {
      assert.ok(ids.includes(id), `doctor should check ${id}`);
    }
    for (const check of payload.checks) {
      if (check.level !== 'ok') assert.ok(check.fix || check.detail, `${check.id} has to say what to do about it`);
    }
    assert.equal(typeof payload.counts.ok, 'number');
    assert.equal(payload.counts.ok + payload.counts.warn + payload.counts.fail, payload.checks.length);
    // Every check id has to be one the contract names, or a client cannot render it.
    const allowed = /^(?:config\.(?:parse|validate)|base\.writable|node\.(?:version|sqlite)|npm\.(?:available|prefix)|service\.[A-Za-z0-9][A-Za-z0-9._-]*\.(?:process|health|busy|relay|latest|endpoint)|scheduler\.(?:present|command|lastRun)|privileges\.(?:sudo|boot)|sleep\.tool|locks|ops\.dir|controller\.copy|session\.restricted|clock)$/;
    for (const check of payload.checks) assert.match(check.id, allowed, `${check.id} is not a contract check id`);
  });
});
