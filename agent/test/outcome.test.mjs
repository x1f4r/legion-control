// Outcome: the one field a client decides on (contract section 5.6).
//
// The agent states it on every mutating reply, every record and every summary,
// computed with the same reducer every client ports, which vectors.test.mjs
// holds to the shared vectors. What matters most here is that a producer can
// move an outcome in exactly one direction, from failed to unknown, and never
// towards anything more optimistic.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { ACTIONS, MUTATING_PHASES, OUTCOMES, producedOutcome } from '../src/contract.mjs';
import { cli, commandService, fixtureConfig, fixtureData, fresh, REPO, withHome, writeConfig } from './helpers.mjs';

test('a producer may lower failed to unknown, and nothing else', () => {
  const failed = { ok: false, action: 'failed', reasonCode: 'internal' };
  assert.equal(producedOutcome(failed), 'failed');
  assert.equal(producedOutcome({ ...failed, outcome: 'unknown' }), 'unknown');
  // Any other stated value is recomputed from the fields beside it, so a
  // producer cannot talk a reply into looking better than it is.
  assert.equal(producedOutcome({ ...failed, outcome: 'succeeded' }), 'failed');
  assert.equal(producedOutcome({ ok: true, action: 'updated', reasonCode: null, outcome: 'unknown' }), 'succeeded');
  assert.equal(producedOutcome({ ok: true, action: 'deferred', reasonCode: 'busy', outcome: 'failed' }), 'deferred');
  assert.deepEqual(OUTCOMES, ['succeeded', 'failed', 'deferred', 'pending', 'unknown']);
});

test('a record and its summary are pending until finished, then carry the outcome stored with the result', async () => {
  await withHome(async () => {
    const ops = await fresh('operations.mjs');
    const id = 'outcome-record-0001';
    const opened = ops.beginOperation({ id, kind: 'update', service: 'demo', systemId: 'test' });
    assert.equal(opened.ok, true);

    // A record opened a moment ago is running and has not reached a phase.
    const fresh0 = ops.wireRecord(ops.readOperation(id));
    assert.equal(fresh0.outcome, 'pending');
    assert.equal(fresh0.phase, 'resolving', 'it reads as the first phase of its kind, never null');
    assert.equal(ops.summarize(ops.readOperation(id)).outcome, 'pending');
    assert.equal(ops.summarize(ops.readOperation(id)).phase, 'resolving');

    ops.finishOperation(id, { ok: true, action: 'deferred', reasonCode: 'busy', message: 'busy' });
    const stored = ops.readOperation(id);
    // A record's ok is "finished without a reason", whatever the producer said.
    assert.equal(stored.result.ok, false);
    assert.equal(stored.result.outcome, 'deferred');
    const wire = ops.wireRecord(stored);
    assert.equal(wire.outcome, 'deferred');
    assert.equal(wire.result.outcome, 'deferred');
    assert.equal(ops.summarize(stored).outcome, 'deferred');

    const queued = ops.queueOperation({ id: 'outcome-queued-0001', kind: 'restart', service: 'demo', systemId: 'test' });
    assert.equal(ops.wireRecord(queued.record).outcome, 'pending');
    assert.equal(ops.wireRecord(queued.record).phase, 'queued');
  });
});

test('a record finished before 3.1.0 is reduced, and states the same outcome in both places', async () => {
  await withHome(async (home) => {
    const ops = await fresh('operations.mjs');
    const id = 'outcome-legacy-0001';
    ops.beginOperation({ id, kind: 'run', actionId: 'say', systemId: 'test' });
    const file = path.join(home, 'ops', `${id}.json`);
    const record = JSON.parse(fs.readFileSync(file, 'utf8'));
    record.state = 'finished';
    record.phase = 'done';
    record.pid = null;
    record.finishedAt = record.updatedAt;
    record.result = { ok: false, action: 'failed', reasonCode: 'timed-out', message: 'too slow', from: null, to: null, exitCode: null, output: null };
    fs.writeFileSync(file, JSON.stringify(record));

    const wire = ops.wireRecord(ops.readOperation(id));
    assert.equal(wire.outcome, 'unknown', 'a run stopped at its time limit may have done its work');
    assert.equal(wire.result.outcome, 'unknown');
    assert.equal(ops.summarize(ops.readOperation(id)).outcome, 'unknown');
  });
});

test('a producer that fails without a reason is stored as internal, never as a silent failure', async () => {
  await withHome(async (home) => {
    const ops = await fresh('operations.mjs');
    const id = 'outcome-silent-0001';
    ops.beginOperation({ id, kind: 'restart', service: 'demo', systemId: 'test' });
    ops.finishOperation(id, { ok: false, action: 'failed', message: 'it did not work' });
    const { result } = ops.readOperation(id);
    assert.equal(result.reasonCode, 'internal');
    assert.equal(result.ok, false);
    assert.equal(result.outcome, 'failed');
    assert.match(fs.readFileSync(path.join(home, 'legionctl.log'), 'utf8'), /no reason code; recorded as internal/);
  });
});

test('a body that throws while changing the machine is unknown; one that throws before is failed', async () => {
  await withHome(async () => {
    const { withOperation } = await fresh('operate.mjs');
    const ops = await fresh('operations.mjs');
    assert.ok(MUTATING_PHASES.has('running'));

    const during = await withOperation({ kind: 'run', actionId: 'say', opId: 'outcome-throw-during', systemId: 'test' }, async ({ opId }) => {
      ops.recordPhase(opId, 'running');
      throw new Error('the action runner fell over');
    });
    assert.equal(during.outcome, 'unknown');
    const record = ops.wireRecord(ops.readOperation('outcome-throw-during'));
    assert.equal(record.result.outcome, 'unknown');
    assert.equal(record.outcome, 'unknown');

    const before = await withOperation({ kind: 'run', actionId: 'say', opId: 'outcome-throw-before', systemId: 'test' }, async () => {
      throw new Error('nothing was started');
    });
    assert.equal(before.outcome, undefined);
    assert.equal(ops.wireRecord(ops.readOperation('outcome-throw-before')).outcome, 'failed');
  });
});

test('a cycle that throws once it reached its services is unknown, and failed before that', async () => {
  await withHome(async (home) => {
    const { runCycle } = await fresh('scheduler.mjs');
    const ops = await fresh('operations.mjs');
    const lockFile = path.join(home, 'demo.lock');
    const throwing = (key) => ({
      id: 'demo', name: 'Demo', lockFile, updates: {},
      get [key]() { throw new Error(`reading ${key} failed`); },
    });
    const config = (service) => ({ system: { id: 'test', name: 'Test' }, updates: { automatic: true }, services: [service] });

    const late = await runCycle(config(throwing('updates')), { opId: 'outcome-cycle-late' });
    assert.equal(late.action, 'failed');
    assert.equal(late.outcome, 'unknown');
    assert.equal(ops.wireRecord(ops.readOperation('outcome-cycle-late')).outcome, 'unknown');

    const early = await runCycle(config(throwing('lockFile')), { opId: 'outcome-cycle-early' });
    assert.equal(early.action, 'failed');
    assert.equal(ops.wireRecord(ops.readOperation('outcome-cycle-early')).outcome, 'failed');
  });
});

test('an update whose state could not be saved after it ran is unknown', { skip: process.platform === 'win32' || process.getuid?.() === 0 }, async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home, { latest: '2.0.0' });
    const state = path.join(home, 'state');
    // The updater installs, then takes away the state directory, so the update
    // ran but its completion cannot be written down.
    const update = [process.execPath, '-e', [
      `const fs = require('node:fs');`,
      `fs.writeFileSync(${JSON.stringify(path.join(dir, 'installed'))}, '2.0.0');`,
      `fs.chmodSync(${JSON.stringify(state)}, 0o500);`,
    ].join(' ')];
    writeConfig(home, fixtureConfig(dir, { services: [commandService(dir, { update })] }));
    try {
      const { payload, exitCode } = cli(['update'], { home });
      assert.equal(payload.action, 'failed', JSON.stringify(payload));
      assert.equal(payload.reasonCode, 'internal');
      assert.equal(payload.outcome, 'unknown', 'the update may have happened, so it must not be resent as if it had not');
      assert.equal(payload.op.outcome, 'unknown');
      assert.equal(payload.op.result.outcome, 'unknown');
      assert.equal(exitCode, 1);
    } finally {
      fs.chmodSync(state, 0o700);
    }
  });
});

test('a cycle with a service nobody can say the end of is unknown, and counts it apart from the failures', { skip: process.platform === 'win32' || process.getuid?.() === 0 }, async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home, { latest: '2.0.0' });
    const state = path.join(home, 'state');
    // The same update as above, which ran but could not be written down: its
    // record is failed with internal and states unknown, while a child on the
    // wire carries only failed and internal, which reduce to failed.
    const update = [process.execPath, '-e', [
      `const fs = require('node:fs');`,
      `fs.writeFileSync(${JSON.stringify(path.join(dir, 'installed'))}, '2.0.0');`,
      `fs.chmodSync(${JSON.stringify(state)}, 0o500);`,
    ].join(' ')];
    writeConfig(home, fixtureConfig(dir, { services: [commandService(dir, { update })] }));
    try {
      const { payload, exitCode } = cli(['cycle'], { home });
      assert.equal(payload.action, 'failed', JSON.stringify(payload));
      assert.equal(payload.outcome, 'unknown', 'a cycle that may have updated a service must not read as failed or as done');
      assert.equal(payload.op.outcome, 'unknown');
      assert.deepEqual(payload.children.map((child) => [child.action, child.reasonCode]), [['failed', 'internal']]);
      assert.match(payload.message, /0 updated, 0 left alone, 0 failed, 1 may or may not have updated/);
      assert.equal(exitCode, 1);
    } finally {
      fs.chmodSync(state, 0o700);
    }
  });
});

test('the reducer copy here and the reference have the same tables', async () => {
  const reference = await import(new URL('../../contract/tools/outcome.mjs', import.meta.url).href);
  const { OK_REPLY_ACTIONS } = await import('../src/contract.mjs');
  assert.deepEqual(OUTCOMES, [...reference.OUTCOMES]);
  assert.deepEqual([...OK_REPLY_ACTIONS].sort(), [...reference.OK_REPLY_ACTIONS].sort());
});

test('each kind may report exactly the actions its schema allows', () => {
  // The table mirrors the schemas, and a value one gains, as boot and sleep
  // gained interrupted, has to reach both.
  for (const [kind, actions] of Object.entries(ACTIONS)) {
    const schema = JSON.parse(fs.readFileSync(path.join(REPO, 'contract', 'schemas', `${kind}.schema.json`), 'utf8'));
    assert.deepEqual([...actions].sort(), [...schema.properties.action.enum].sort(), kind);
  }
});
