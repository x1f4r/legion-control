// Durable operations: identity, replay, recovery and the queue.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { spawn } from 'node:child_process';
import test from 'node:test';
import { pathToFileURL } from 'node:url';
import { cli, fresh, SRC, withHome } from './helpers.mjs';

const base = { kind: 'update', service: 'demo', systemId: 'linux' };

function concurrentWorkers(source, argumentsByWorker, home) {
  return Promise.all(argumentsByWorker.map((args) => new Promise((resolve, reject) => {
    const child = spawn(process.execPath, ['--input-type=module', '-e', source, ...args], {
      env: { ...process.env, LEGIONCTL_HOME: home },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', (chunk) => { stdout += chunk; });
    child.stderr.on('data', (chunk) => { stderr += chunk; });
    child.on('error', reject);
    child.on('exit', (code) => {
      if (code !== 0) reject(new Error(`worker exited ${code}: ${stderr}`));
      else resolve(stdout.trim());
    });
  })));
}

test('an operation is answerable from the moment it is opened', async () => {
  await withHome(async () => {
    const ops = await fresh('operations.mjs');
    const opened = ops.beginOperation({ id: 'aaaaaaaa-1111-4111-8111-111111111111', ...base });
    assert.equal(opened.ok, true);
    assert.equal(opened.replay, false);
    const record = ops.readOperation(opened.record.id);
    assert.equal(record.state, 'running');
    assert.equal(record.pid, process.pid);
    assert.ok(record.bootId, 'a boot identity is captured up front, for judging a reboot later');
  });
});

test('the same id twice is a replay, not a second run', async () => {
  await withHome(async () => {
    const ops = await fresh('operations.mjs');
    const id = 'bbbbbbbb-1111-4111-8111-111111111111';
    ops.beginOperation({ id, ...base });
    const again = ops.beginOperation({ id, ...base });
    assert.equal(again.ok, true);
    assert.equal(again.replay, true);
  });
});

test('concurrent processes bind the same id exactly once and reject the other intent', async () => {
  await withHome(async (home) => {
    const id = 'abababab-1111-4111-8111-111111111111';
    const source = `
      import { beginOperation } from ${JSON.stringify(pathToFileURL(path.join(SRC, 'operations.mjs')).href)};
      const kind = process.argv[1];
      const result = beginOperation({ id: ${JSON.stringify(id)}, kind, service: 'demo', systemId: 'linux' });
      process.stdout.write(JSON.stringify({ kind, ok: result.ok, replay: result.replay, conflict: result.conflict }));
    `;
    const kinds = ['update', 'restart', 'update', 'restart', 'update', 'restart', 'update', 'restart'];
    const outputs = await concurrentWorkers(source, kinds.map((kind) => [kind]), home);
    const results = outputs.map((output) => JSON.parse(output));
    const ops = await fresh('operations.mjs');
    const stored = ops.readOperation(id);

    assert.ok(stored);
    assert.equal(results.filter((result) => result.ok && result.replay === false).length, 1, 'only one process may create the record');
    assert.equal(results.filter((result) => result.ok).length, 4, 'all requests with the winning intent replay the same record');
    assert.equal(results.filter((result) => result.conflict).length, 4, 'the other intent always conflicts');
    assert.ok(results.filter((result) => result.ok).every((result) => result.kind === stored.kind));
  });
});

test('an id binds one intent; a different one is a conflict', async () => {
  await withHome(async () => {
    const ops = await fresh('operations.mjs');
    const id = 'cccccccc-1111-4111-8111-111111111111';
    ops.beginOperation({ id, ...base });
    for (const different of [
      { ...base, kind: 'restart' },
      { ...base, service: 'other' },
      { ...base, force: true },
    ]) {
      const clash = ops.beginOperation({ id, ...different });
      assert.equal(clash.ok, false, `${JSON.stringify(different)} should have clashed`);
      assert.equal(clash.conflict, true);
      assert.match(clash.error, /binds one intent/);
    }
  });
});

test('an id outside the grammar is refused', async () => {
  await withHome(async () => {
    const ops = await fresh('operations.mjs');
    assert.equal(ops.beginOperation({ id: 'sh ort', ...base }).ok, false);
    assert.equal(ops.beginOperation({ id: '../escape/../../etc/passwd', ...base }).ok, false);
  });
});

test('phases and the log are recorded, and the log is capped', async () => {
  await withHome(async () => {
    const ops = await fresh('operations.mjs');
    const { record } = ops.beginOperation({ id: 'dddddddd-1111-4111-8111-111111111111', ...base });
    ops.recordPhase(record.id, 'stopping', { step: 3, of: 8, note: 'stopping the unit' });
    for (let index = 0; index < 300; index += 1) ops.appendLog(record.id, `line ${index}`);
    const stored = ops.readOperation(record.id);
    assert.equal(stored.phase, 'stopping');
    assert.deepEqual(stored.progress, { step: 3, of: 8, note: 'stopping the unit' });
    assert.equal(stored.log.length, ops.LOG_LIMIT);
    assert.match(stored.log.at(-1).line, /line 299/);
  });
});

test('concurrent read-modify-write patches keep every update', async () => {
  await withHome(async (home) => {
    const ops = await fresh('operations.mjs');
    const id = 'acacacac-1111-4111-8111-111111111111';
    assert.equal(ops.beginOperation({ id, ...base }).ok, true);
    const writesEach = 15;
    const source = `
      import { updateOperation } from ${JSON.stringify(pathToFileURL(path.join(SRC, 'operations.mjs')).href)};
      const worker = process.argv[1];
      for (let index = 0; index < ${writesEach}; index += 1) {
        updateOperation(${JSON.stringify(id)}, (record) => {
          Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 3);
          return { [worker + '-' + index]: index };
        });
      }
    `;
    const workers = Array.from({ length: 8 }, (_, index) => `w${index}`);
    await concurrentWorkers(source, workers.map((worker) => [worker]), home);
    const stored = ops.readOperation(id);
    for (const worker of workers) {
      for (let index = 0; index < writesEach; index += 1) assert.equal(stored[`${worker}-${index}`], index);
    }
  });
});

test('the wire record carries only what the contract names', async () => {
  await withHome(async () => {
    const ops = await fresh('operations.mjs');
    const { record } = ops.beginOperation({ id: 'eeeeeeee-1111-4111-8111-111111111111', ...base });
    const wire = ops.wireRecord(ops.readOperation(record.id));
    for (const internal of ['bootId', 'queueRequested', 'force', 'contract', 'workerClaimed', 'launchPendingUntil', 'ownerStartedAt', 'lockOwner']) {
      assert.equal(internal in wire, false, `${internal} is internal and must not reach a client`);
    }
    for (const required of ['id', 'kind', 'service', 'target', 'actionId', 'mode', 'initiator', 'state', 'phase', 'progress', 'requestedAt', 'startedAt', 'updatedAt', 'finishedAt', 'expiresAt', 'pid', 'detached', 'result', 'log', 'agentVersion', 'systemId']) {
      assert.ok(required in wire, `${required} is required by the contract`);
    }
  });
});

test('an operation whose process is gone is finished as interrupted', async () => {
  await withHome(async () => {
    const ops = await fresh('operations.mjs');
    const { record } = ops.beginOperation({ id: 'ffffffff-1111-4111-8111-111111111111', ...base });
    ops.recordPhase(record.id, 'installing');
    ops.updateOperation(record.id, { pid: 99999999 });

    const recovered = ops.recoverOperations({ systemId: 'linux' });
    assert.equal(recovered.length, 1);
    assert.equal(recovered[0].state, 'finished');
    assert.equal(recovered[0].result.action, 'interrupted');
    assert.match(recovered[0].result.message, /installing/);
    assert.deepEqual(ops.servicesNeedingRecovery(recovered).map((entry) => entry.service), ['demo']);
  });
});

/**
 * A process that stays up until it is killed, started now, for a record to
 * name whose pid is plainly alive. With `lock`, it also takes the machine lock
 * for that operation id, as an operation's own process would.
 */
async function liveProcess(home, { lock = null } = {}) {
  const source = lock
    ? `import { acquireOperationLock } from ${JSON.stringify(pathToFileURL(path.join(SRC, 'lock.mjs')).href)};
       const held = acquireOperationLock({ kind: 'update', opId: ${JSON.stringify(lock)}, service: 'demo', waitMs: 5000 });
       process.stdout.write(held.ok ? 'ready\\n' : 'failed: ' + held.message + '\\n');
       setInterval(() => {}, 1000);`
    : `process.stdout.write('ready\\n'); setInterval(() => {}, 1000);`;
  const child = spawn(process.execPath, ['--input-type=module', '-e', source], {
    env: { ...process.env, LEGIONCTL_HOME: home },
    stdio: ['ignore', 'pipe', 'inherit'],
  });
  await new Promise((resolve, reject) => {
    let out = '';
    child.stdout.on('data', (chunk) => {
      out += chunk;
      if (out.includes('ready')) resolve();
      if (out.includes('failed')) reject(new Error(out));
    });
    child.once('exit', () => reject(new Error(`the live process exited early: ${out}`)));
  });
  return {
    pid: child.pid,
    async stop() {
      if (child.exitCode !== null || child.signalCode !== null) return;
      const exited = new Promise((resolve) => child.once('exit', resolve));
      child.kill('SIGKILL');
      await exited;
    },
  };
}

const anHourAgo = () => new Date(Date.now() - 60 * 60 * 1000).toISOString();

test('a pid another process holds now does not keep a record running', async () => {
  await withHome(async (home) => {
    const ops = await fresh('operations.mjs');
    const other = await liveProcess(home);
    try {
      // Its owner started an hour ago, and the process with that id now started
      // a moment ago: the id was handed out again, and the owner is gone.
      const reused = '12121212-1111-4111-8111-111111111111';
      ops.beginOperation({ id: reused, ...base });
      ops.recordPhase(reused, 'installing');
      ops.updateOperation(reused, { pid: other.pid, ownerStartedAt: anHourAgo() });
      assert.equal(ops.deriveState(ops.readOperation(reused)).state, 'finished');

      // The same live pid, with an owner that started before it was written
      // down, is that owner still at work.
      const owned = '13131313-1111-4111-8111-111111111111';
      ops.beginOperation({ id: owned, ...base });
      ops.updateOperation(owned, { pid: other.pid, ownerStartedAt: new Date().toISOString() });
      assert.equal(ops.deriveState(ops.readOperation(owned)).state, 'running');

      // A record from before 3.1.0 names no start time, and a live pid is
      // taken at its word as it always was.
      const legacy = '14141414-1111-4111-8111-111111111111';
      ops.beginOperation({ id: legacy, ...base });
      ops.updateOperation(legacy, { pid: other.pid, ownerStartedAt: undefined });
      assert.equal(ops.deriveState(ops.readOperation(legacy)).state, 'running');

      const recovered = ops.recoverOperations({ systemId: 'linux' });
      assert.deepEqual(recovered.map((record) => record.id), [reused]);
      assert.equal(recovered[0].result.action, 'interrupted');
      assert.deepEqual(ops.servicesNeedingRecovery(recovered).map((entry) => entry.service), ['demo'], 'what it stopped is still put back');
      assert.equal(ops.readOperation(owned).state, 'running');
    } finally {
      await other.stop();
    }
  });
});

test('this process is not the owner of a record an earlier process with its pid left', async () => {
  await withHome(async () => {
    const ops = await fresh('operations.mjs');
    const left = '15151515-1111-4111-8111-111111111111';
    ops.beginOperation({ id: left, ...base });
    ops.updateOperation(left, { pid: process.pid, ownerStartedAt: anHourAgo() });
    const mine = '16161616-1111-4111-8111-111111111111';
    ops.beginOperation({ id: mine, ...base });
    assert.deepEqual(ops.recoverOperations({ systemId: 'linux' }).map((record) => record.id), [left]);
    assert.equal(ops.readOperation(mine).state, 'running', 'a record this process opened is its own');
  });
});

test('a record that took the machine lock lives exactly as long as that hold', async () => {
  await withHome(async (home) => {
    const ops = await fresh('operations.mjs');
    const { acquireOperationLock } = await import('../src/lock.mjs');
    const id = '17171717-1111-4111-8111-111111111111';
    const holder = await liveProcess(home, { lock: id });
    const bystander = await liveProcess(home);
    try {
      ops.beginOperation({ id, ...base });
      ops.updateOperation(id, { pid: holder.pid, ownerStartedAt: new Date().toISOString(), lockOwner: id });
      assert.equal(ops.deriveState(ops.readOperation(id)).state, 'running', 'the lock is held for it, by its own process');

      // A live pid whose process says it took the lock, while another
      // operation's sidecar is all there is to read: that sidecar may be one
      // its holder failed to replace, so the pid is taken at its word.
      const orphan = '18181818-1111-4111-8111-111111111111';
      ops.beginOperation({ id: orphan, ...base });
      ops.updateOperation(orphan, { pid: bystander.pid, ownerStartedAt: new Date().toISOString(), lockOwner: orphan });
      assert.equal(ops.deriveState(ops.readOperation(orphan)).state, 'running');

      // Free, the lock says its owner is gone whatever its pid says now.
      await holder.stop();
      assert.equal(ops.deriveState(ops.readOperation(orphan)).state, 'finished', 'the lock is free');
      ops.updateOperation(id, { pid: bystander.pid });
      assert.equal(ops.deriveState(ops.readOperation(id)).state, 'finished', 'its hold ended with its process');

      // And held by this process, as a cycle's recovery holds it, no other
      // process can be holding it for a record of its own.
      const held = acquireOperationLock({ kind: 'cycle', opId: '19191919-2222-4222-8222-222222222222' });
      assert.equal(held.ok, true);
      try {
        const elsewhere = '1b1b1b1b-1111-4111-8111-111111111111';
        ops.beginOperation({ id: elsewhere, ...base });
        ops.updateOperation(elsewhere, { pid: bystander.pid, ownerStartedAt: new Date().toISOString(), lockOwner: elsewhere });
        assert.equal(ops.deriveState(ops.readOperation(elsewhere)).state, 'finished', 'its live pid is not holding the lock this process holds');
        const mine = '1a1a1a1a-1111-4111-8111-111111111111';
        ops.beginOperation({ id: mine, ...base });
        ops.updateOperation(mine, { lockOwner: '19191919-2222-4222-8222-222222222222' });
        assert.deepEqual(ops.recoverOperations({ systemId: 'linux' }).map((record) => record.id).sort(), [id, orphan, elsewhere].sort());
        assert.equal(ops.readOperation(mine).state, 'running', 'a record of this process is its own to finish');
      } finally {
        held.lock.release();
      }
    } finally {
      await holder.stop();
      await bystander.stop();
    }
  });
});

test('recovery finds a running record however many newer ones there are', async () => {
  await withHome(async () => {
    const ops = await fresh('operations.mjs');
    const stuck = '19191919-1111-4111-8111-111111111111';
    ops.beginOperation({ id: stuck, ...base });
    ops.updateOperation(stuck, { pid: 99999999, requestedAt: '2026-01-01T00:00:00.000Z' });
    for (let index = 0; index < ops.HISTORY_LIMIT + 5; index += 1) {
      const id = `${String(index).padStart(8, '0')}-2222-4222-8222-222222222222`;
      ops.beginOperation({ id, ...base });
      ops.finishOperation(id, { ok: true, action: 'noop', reasonCode: null, message: `run ${index}` });
    }
    assert.equal(ops.listOperations({ limit: 500 }).some((record) => record.id === stuck), false, 'it is past the newest the history lists');
    assert.deepEqual(ops.recoverOperations({ systemId: 'linux' }).map((record) => record.id), [stuck]);
  });
});

test('an operation interrupted before it touched anything needs no service recovery', async () => {
  await withHome(async () => {
    const ops = await fresh('operations.mjs');
    const { record } = ops.beginOperation({ id: '11111111-1111-4111-8111-111111111111', ...base });
    ops.recordPhase(record.id, 'resolving');
    ops.updateOperation(record.id, { pid: 99999999 });
    const recovered = ops.recoverOperations({ systemId: 'linux' });
    assert.deepEqual(ops.servicesNeedingRecovery(recovered), [], 'a run that never stopped anything must not be "recovered"');
  });
});

test('the records owing a service its recovery are oldest first by when each started, not when each was asked for', async () => {
  const ops = await fresh('operations.mjs');
  const owing = (id, requestedAt, startedAt) => ({ id, ...base, phase: 'installing', requestedAt, startedAt });
  // Newest asked for first, as a listing gives them. The queued one was asked
  // for first and taken up last.
  const listed = [
    owing('direct', '2026-01-01T10:05:00.000Z', '2026-01-01T10:05:00.000Z'),
    owing('queued', '2026-01-01T10:00:00.000Z', '2026-01-01T10:20:00.000Z'),
    owing('older', '2026-01-01T09:00:00.000Z', null),
  ];
  assert.deepEqual(ops.servicesNeedingRecovery(listed)[0].records.map((record) => record.id), ['older', 'direct', 'queued']);
});

test('a reboot is only believed with both a new boot and the right system', async () => {
  await withHome(async () => {
    const ops = await fresh('operations.mjs');
    const record = { kind: 'boot', target: 'windows', bootId: 'boot-before', pid: 99999999, phase: 'rebooting' };

    const rebooted = ops.judgeInterrupted(record, { systemId: 'windows', bootId: 'boot-after' });
    assert.equal(rebooted.action, 'rebooted');
    assert.equal(rebooted.ok, true);

    // A new boot into another system is not the one asked for, but it is not a
    // failure either: on a dual-boot machine the record is judged by the system
    // that wrote it, after the machine came back from the target. Calling it a
    // success would send a client off waiting; calling it failed would tell it
    // a reboot that most likely worked did not.
    const otherSystem = ops.judgeInterrupted(record, { systemId: 'linux', bootId: 'boot-after' });
    assert.equal(otherSystem.action, 'interrupted');
    assert.equal(otherSystem.ok, false);
    assert.equal(otherSystem.reasonCode, 'interrupted');
    assert.match(otherSystem.message, /running linux now; whether it reached windows in between cannot be told/);
    const unknownSystem = ops.judgeInterrupted(record, { systemId: null, bootId: 'boot-after' });
    assert.equal(unknownSystem.action, 'interrupted');
    assert.match(unknownSystem.message, /an unknown system/);

    // No reboot happened at all.
    const noReboot = ops.judgeInterrupted(record, { systemId: 'linux', bootId: 'boot-before' });
    assert.equal(noReboot.action, 'interrupted');
    assert.match(noReboot.message, /has not rebooted since/);
  });
});

test('an interrupted sleep is never guessed at', async () => {
  await withHome(async () => {
    const ops = await fresh('operations.mjs');
    const verdict = ops.judgeInterrupted({ kind: 'sleep', bootId: 'a', pid: 99999999 }, { systemId: 'linux', bootId: 'b' });
    assert.equal(verdict.action, 'interrupted');
    assert.match(verdict.message, /cannot be observed after the fact/);
  });
});

test('status derives an interrupted state without writing to the record', async () => {
  await withHome(async () => {
    const ops = await fresh('operations.mjs');
    const { record } = ops.beginOperation({ id: '22222222-1111-4111-8111-111111111111', ...base });
    ops.updateOperation(record.id, { pid: 99999999 });
    const derived = ops.deriveState(ops.readOperation(record.id));
    assert.equal(derived.state, 'finished');
    assert.equal(derived.derived, true);
    assert.equal(ops.readOperation(record.id).state, 'running', 'a read-only command must not fix the record');
  });
});

test('a queued request expires, and one per slot replaces the last', async () => {
  await withHome(async () => {
    const ops = await fresh('operations.mjs');
    const first = ops.queueOperation({ id: '33333333-1111-4111-8111-111111111111', kind: 'restart', service: 'demo', ttlMs: 60000 });
    assert.equal(first.ok, true);
    assert.equal(ops.dueQueue().length, 1);

    const second = ops.queueOperation({ id: '44444444-1111-4111-8111-111111111111', kind: 'restart', service: 'demo', ttlMs: 60000 });
    assert.equal(second.replaced, first.record.id, 'pressing it twice must not run it twice');
    assert.equal(ops.dueQueue().length, 1);

    // A different service is a different slot.
    ops.queueOperation({ id: '55555555-1111-4111-8111-111111111111', kind: 'restart', service: 'other', ttlMs: 60000 });
    assert.equal(ops.dueQueue().length, 2);

    // And an expiry is not optional.
    ops.queueOperation({ id: '66666666-1111-4111-8111-111111111111', kind: 'update', service: 'demo', ttlMs: 60000 });
    const later = Date.now() + 2 * 60000;
    assert.equal(ops.dueQueue({ now: later }).length, 0, 'an expired request is not due');
    const expired = ops.expireQueue({ now: later });
    assert.equal(expired.length, 3, 'the one that was replaced is not waiting any more');
    assert.ok(expired.every((record) => record.result.action === 'expired'));
  });
});

test('an expiry lands between a minute and a week from now, however it is written', async () => {
  const ops = await fresh('operations.mjs');
  const now = Date.parse('2026-10-29T12:00:00.000Z');
  const at = (input) => ops.queueExpiry(input, { now }).expiresAt;
  assert.equal(at({}), '2026-10-29T16:00:00.000Z', 'four hours when nothing is named');
  assert.equal(at({ ttlMs: 30 * 60000 }), '2026-10-29T12:30:00.000Z');
  assert.equal(at({ ttlMs: 30 * 24 * 60 * 60000 }), '2026-11-05T12:00:00.000Z', 'a duration past the week is the week');
  assert.equal(at({ expiresAt: '2027-01-01T00:00:00.000Z' }), '2026-11-05T12:00:00.000Z', 'so is a time past it');
  assert.equal(at({ expiresAt: '2026-10-29T12:00:10.000Z' }), '2026-10-29T12:01:00.000Z', 'a time in the next minute is the minute');
  assert.equal(at({ expiresAt: '2026-10-30T09:15:00.000Z' }), '2026-10-30T09:15:00.000Z');
  for (const past of ['2026-10-29T12:00:00.000Z', '2026-10-28T12:00:00.000Z', '2001-05-01T00:00:00.000Z']) {
    const refused = ops.queueExpiry({ expiresAt: past }, { now });
    assert.equal(refused.ok, false, past);
    assert.match(refused.error, /already past/);
  }
  assert.equal(ops.queueExpiry({ expiresAt: 'whenever' }, { now }).ok, false);
});

test('a request whose expiry has passed is refused without touching the one that waits', async () => {
  await withHome(async () => {
    const ops = await fresh('operations.mjs');
    const waiting = '36363636-1111-4111-8111-111111111111';
    ops.queueOperation({ id: waiting, kind: 'restart', service: 'demo', ttlMs: 60 * 60000 });
    const stale = ops.queueOperation({ id: '37373737-1111-4111-8111-111111111111', kind: 'restart', service: 'demo', expiresAt: new Date(Date.now() - 1000).toISOString() });
    assert.equal(stale.ok, false);
    assert.equal(stale.badExpiry, true);
    assert.equal(ops.readOperation('37373737-1111-4111-8111-111111111111'), null, 'nothing was written');
    assert.deepEqual(ops.dueQueue().map((record) => record.id), [waiting]);

    // A retry of the request that waits is answered with it, whatever the
    // expiry it was first sent with says by now.
    const retry = ops.queueOperation({ id: waiting, kind: 'restart', service: 'demo', expiresAt: new Date(Date.now() - 1000).toISOString() });
    assert.equal(retry.ok, true);
    assert.equal(retry.replay, true);
    assert.equal(ops.readOperation(waiting).state, 'queued');
  });
});

test('boot and sleep wait in one slot, whatever the target', async () => {
  await withHome(async () => {
    const ops = await fresh('operations.mjs');
    const boot = ops.queueOperation({ id: '38383838-1111-4111-8111-111111111111', kind: 'boot', target: 'windows', ttlMs: 60000 });
    const otherTarget = ops.queueOperation({ id: '39393939-1111-4111-8111-111111111111', kind: 'boot', target: 'linux', ttlMs: 60000 });
    assert.equal(otherTarget.replaced, boot.record.id, 'the machine makes one power transition');
    const sleep = ops.queueOperation({ id: '40404040-1111-4111-8111-111111111111', kind: 'sleep', ttlMs: 60000 });
    assert.equal(sleep.replaced, otherTarget.record.id);
    assert.match(ops.readOperation(otherTarget.record.id).result.message, /one power transition at a time/);
    assert.deepEqual(ops.dueQueue().map((record) => record.kind), ['sleep']);

    // Everything else still has a slot of its own beside it.
    ops.queueOperation({ id: '41414141-1111-4111-8111-111111111111', kind: 'restart', service: 'demo', ttlMs: 60000 });
    assert.deepEqual(ops.dueQueue().map((record) => record.kind).sort(), ['restart', 'sleep']);
  });
});

test('replaying a replaced queue id does not cancel the newer request', async () => {
  await withHome(async () => {
    const ops = await fresh('operations.mjs');
    const oldId = '34343434-1111-4111-8111-111111111111';
    const newId = '45454545-1111-4111-8111-111111111111';
    ops.queueOperation({ id: oldId, kind: 'restart', service: 'demo', ttlMs: 60000 });
    const newer = ops.queueOperation({ id: newId, kind: 'restart', service: 'demo', ttlMs: 60000 });
    assert.equal(newer.replaced, oldId);

    const replay = ops.queueOperation({ id: oldId, kind: 'restart', service: 'demo', ttlMs: 60000 });
    assert.equal(replay.ok, true);
    assert.equal(replay.replay, true);
    assert.equal(replay.replaced, null);
    assert.deepEqual(ops.dueQueue().map((record) => record.id), [newId]);
    assert.equal(ops.readOperation(newId).state, 'queued');
  });
});

test('a conflicting queue id is rejected before the current slot is changed', async () => {
  await withHome(async () => {
    const ops = await fresh('operations.mjs');
    const conflictingId = '46464646-1111-4111-8111-111111111111';
    const queuedId = '47474747-1111-4111-8111-111111111111';
    ops.beginOperation({ id: conflictingId, kind: 'update', service: 'other', systemId: 'linux' });
    ops.queueOperation({ id: queuedId, kind: 'restart', service: 'demo', ttlMs: 60000 });

    const conflict = ops.queueOperation({ id: conflictingId, kind: 'restart', service: 'demo', ttlMs: 60000 });
    assert.equal(conflict.ok, false);
    assert.equal(conflict.conflict, true);
    assert.deepEqual(ops.dueQueue().map((record) => record.id), [queuedId]);
    assert.equal(ops.readOperation(queuedId).state, 'queued');
  });
});

test('only one concurrent worker can claim a queued operation', async () => {
  await withHome(async (home) => {
    const ops = await fresh('operations.mjs');
    const id = '56565656-1111-4111-8111-111111111111';
    assert.equal(ops.queueOperation({ id, kind: 'restart', service: 'demo', ttlMs: 60000 }).ok, true);
    const source = `
      import { claimOperation } from ${JSON.stringify(pathToFileURL(path.join(SRC, 'operations.mjs')).href)};
      const result = claimOperation(${JSON.stringify(id)});
      process.stdout.write(JSON.stringify({ ok: result.ok, claimed: result.claimed, pid: process.pid }));
    `;
    const outputs = await concurrentWorkers(source, Array.from({ length: 8 }, () => []), home);
    const results = outputs.map((output) => JSON.parse(output));
    const winner = results.find((result) => result.ok);
    assert.ok(winner);
    assert.equal(results.filter((result) => result.ok).length, 1);
    assert.equal(ops.readOperation(id).pid, winner.pid);
    assert.equal(ops.readOperation(id).workerClaimed, true);
  });
});

test('the launcher process can claim a synchronous fallback exactly once', async () => {
  await withHome(async () => {
    const ops = await fresh('operations.mjs');
    const id = '57575757-1111-4111-8111-111111111111';
    ops.beginOperation({ id, ...base, detached: true });
    ops.updateOperation(id, { detached: false, launchPendingUntil: new Date(Date.now() + 30000).toISOString() });
    assert.equal(ops.claimOperation(id).ok, true);
    assert.equal(ops.claimOperation(id).ok, false, 'even the same pid must not execute the body twice');
  });
});

test('launch grace keeps an unclaimed detached worker recoverable until its deadline', async () => {
  await withHome(async () => {
    const ops = await fresh('operations.mjs');
    const id = '67676767-1111-4111-8111-111111111111';
    ops.beginOperation({ id, ...base, detached: true });
    ops.updateOperation(id, { pid: 99999999, launchPendingUntil: new Date(Date.now() + 30000).toISOString() });
    assert.deepEqual(ops.recoverOperations({ systemId: 'linux' }), []);
    assert.equal(ops.deriveState(ops.readOperation(id)).state, 'running');

    ops.updateOperation(id, { launchPendingUntil: new Date(Date.now() - 1).toISOString() });
    assert.equal(ops.recoverOperations({ systemId: 'linux' })[0].result.action, 'interrupted');
  });
});

test('an update write failure throws instead of returning an unsaved record', { skip: process.platform === 'win32' || process.getuid?.() === 0 }, async () => {
  await withHome(async (home) => {
    const ops = await fresh('operations.mjs');
    const id = '68686868-1111-4111-8111-111111111111';
    ops.beginOperation({ id, ...base });
    const dir = path.join(home, 'ops');
    fs.chmodSync(dir, 0o500);
    try {
      assert.throws(() => ops.updateOperation(id, { phase: 'installing' }), (err) => {
        assert.equal(err.name, 'OperationStoreError');
        assert.match(err.message, /not writable|could not be written/);
        return true;
      });
      assert.notEqual(ops.readOperation(id).phase, 'installing');
    } finally {
      fs.chmodSync(dir, 0o700);
    }
  });
});

test('a queued operation can be cancelled; a running one cannot', async () => {
  await withHome(async () => {
    const ops = await fresh('operations.mjs');
    const queued = ops.queueOperation({ id: '77777777-1111-4111-8111-111111111111', kind: 'restart', service: 'demo', ttlMs: 60000 });
    const cancelled = ops.cancelOperation(queued.record.id);
    assert.equal(cancelled.ok, true);
    assert.equal(cancelled.action, 'cancelled');

    const { record } = ops.beginOperation({ id: '88888888-1111-4111-8111-111111111111', ...base });
    const refused = ops.cancelOperation(record.id);
    assert.equal(refused.ok, false);
    assert.equal(refused.action, 'conflict');
    assert.match(refused.message, /never aborted from outside/);
  });
});

test('history is bounded, and pruning keeps the newest', async () => {
  await withHome(async () => {
    const ops = await fresh('operations.mjs');
    for (let index = 0; index < 210; index += 1) {
      const id = `${String(index).padStart(8, '0')}-1111-4111-8111-111111111111`;
      ops.beginOperation({ id, ...base });
      ops.finishOperation(id, { ok: true, action: 'noop', reasonCode: null, message: `run ${index}` });
    }
    assert.equal(ops.listOperations({ limit: 500 }).length, ops.HISTORY_LIMIT);
    const pruned = ops.pruneOperations();
    assert.equal(pruned.removed.length, 10);
    assert.equal(ops.listOperations({ limit: 500 }).length, 200);
  });
});

test('a failure outlives the newest records by a week, and nothing outlives thirty days', async () => {
  await withHome(async (home) => {
    const ops = await fresh('operations.mjs');
    const day = 24 * 60 * 60 * 1000;
    const finished = (id, result, daysAgo) => {
      ops.beginOperation({ id, ...base });
      ops.finishOperation(id, result);
      const at = new Date(Date.now() - daysAgo * day).toISOString();
      ops.updateOperation(id, { requestedAt: at, finishedAt: at });
    };
    const failed = { ok: false, action: 'failed', reasonCode: 'latest-unknown', message: 'the registry did not answer' };
    finished('aaaaaaa1-3333-4333-8333-333333333333', failed, 3);
    finished('aaaaaaa2-3333-4333-8333-333333333333', { ok: false, action: 'interrupted', reasonCode: 'interrupted', message: 'cut off' }, 6);
    finished('aaaaaaa3-3333-4333-8333-333333333333', failed, 8);
    finished('aaaaaaa4-3333-4333-8333-333333333333', { ok: true, action: 'noop', reasonCode: null, message: 'already there' }, 3);
    // A quarter-hourly cycle's worth of routine records, every one newer.
    for (let index = 0; index < ops.HISTORY_LIMIT; index += 1) {
      finished(`${String(index).padStart(8, '0')}-3333-4333-8333-333333333333`, { ok: true, action: 'noop', reasonCode: null, message: 'nothing new' }, index / (4 * 24));
    }
    // And a failure past thirty days, which nothing keeps.
    finished('aaaaaaa5-3333-4333-8333-333333333333', failed, 31);

    const pruned = ops.pruneOperations();
    assert.deepEqual(pruned.removed.sort(), [
      'aaaaaaa3-3333-4333-8333-333333333333',
      'aaaaaaa4-3333-4333-8333-333333333333',
      'aaaaaaa5-3333-4333-8333-333333333333',
    ]);
    assert.ok(ops.readOperation('aaaaaaa1-3333-4333-8333-333333333333'), 'a failure three days old is kept');
    assert.ok(ops.readOperation('aaaaaaa2-3333-4333-8333-333333333333'), 'so is an outcome nobody knows');
    assert.equal(ops.countOperations(), ops.HISTORY_LIMIT + 2);
    const history = cli(['history', '--limit', '5'], { home }).payload;
    assert.equal(history.returned, 5);
    assert.equal(history.retained, ops.HISTORY_LIMIT + 2, 'what the machine holds, past the newest it lists');
  });
});

test('a record written on another machine is judged like any other, never left running', async () => {
  await withHome(async () => {
    const ops = await fresh('operations.mjs');
    // A home directory restored from another machine: its boot and its pid
    // mean nothing here. What it was doing stopped when that machine stopped
    // running it, so it is interrupted rather than left running for good.
    const { record } = ops.beginOperation({ id: '99999999-1111-4111-8111-111111111111', ...base });
    ops.recordPhase(record.id, 'installing');
    ops.updateOperation(record.id, { pid: process.pid, bootId: 'a-boot-of-another-machine' });
    assert.equal(ops.deriveState(ops.readOperation(record.id)).state, 'finished', 'a pid alive here is not its owner');
    const recovered = ops.recoverOperations({ systemId: 'linux' });
    assert.deepEqual(recovered.map((entry) => entry.id), [record.id]);
    assert.equal(recovered[0].result.action, 'interrupted');
  });
});
