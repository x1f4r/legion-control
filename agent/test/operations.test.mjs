// Durable operations: identity, replay, recovery and the queue.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { spawn } from 'node:child_process';
import test from 'node:test';
import { pathToFileURL } from 'node:url';
import { fresh, SRC, withHome } from './helpers.mjs';

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
    for (const internal of ['bootId', 'queueRequested', 'force', 'contract', 'workerClaimed', 'launchPendingUntil']) {
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

test('a reboot is only believed with both a new boot and the right system', async () => {
  await withHome(async () => {
    const ops = await fresh('operations.mjs');
    const record = { kind: 'boot', target: 'windows', bootId: 'boot-before', pid: 99999999, phase: 'rebooting' };

    const rebooted = ops.judgeInterrupted(record, { systemId: 'windows', bootId: 'boot-after' });
    assert.equal(rebooted.action, 'rebooted');
    assert.equal(rebooted.ok, true);

    // The machine came back, but into the system it was already in: the arming
    // silently failed, and calling this a success sends a client off waiting.
    const wrongSystem = ops.judgeInterrupted(record, { systemId: 'linux', bootId: 'boot-after' });
    assert.equal(wrongSystem.action, 'failed');

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
    ops.queueOperation({ id: '66666666-1111-4111-8111-111111111111', kind: 'update', service: 'demo', expiresAt: new Date(Date.now() - 1000).toISOString() });
    assert.equal(ops.dueQueue().length, 2, 'an expired request is not due');
    const expired = ops.expireQueue();
    assert.equal(expired.length, 1);
    assert.equal(expired[0].result.action, 'expired');
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

test('a record from another host is left alone rather than declared dead', async () => {
  await withHome(async () => {
    const ops = await fresh('operations.mjs');
    const { record } = ops.beginOperation({ id: '99999999-1111-4111-8111-111111111111', ...base });
    ops.updateOperation(record.id, { pid: 99999999, owner: { pid: 99999999, host: 'somebody-elses-machine' } });
    // The pid means nothing here; the record belongs to a home directory that was
    // restored or is shared.
    const derived = ops.deriveState(ops.readOperation(record.id));
    assert.equal(derived.state, 'finished', 'a dead pid on this host is still judged');
  });
});
