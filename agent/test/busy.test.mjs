// The busy gate: where its answers come from, and what each of them permits.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { aggregateBusy, checkAllBusy, checkServiceBusy, readCommandBusy } from '../src/probes/busy.mjs';
import { checkT3Sqlite } from '../src/probes/t3-sqlite.mjs';
import { FIXTURE, fixtureData, withHome, writeFixture } from './helpers.mjs';

const call = (dir, verb) => [process.execPath, FIXTURE, verb, dir];

test('a command probe answers from its exit code', () => {
  assert.equal(readCommandBusy({ ok: true, code: 0, stdout: '', stderr: '', command: 'x' }).busy, false);
  assert.equal(readCommandBusy({ ok: false, code: 1, stdout: '', stderr: 'a turn', command: 'x' }).busy, true);
});

test('a probe that timed out blocks, and says it timed out', () => {
  const answer = readCommandBusy({ ok: false, code: null, timedOut: true, stdout: '', stderr: '', command: 'x' });
  assert.equal(answer.busy, true);
  assert.equal(answer.unknown, true);
  assert.equal(answer.evidence, 'timed-out');
});

test('a command reporting unknown state cannot be mistaken for observed busy or idle', () => {
  for (const busy of [true, false]) {
    const answer = readCommandBusy({ ok: true, code: 0, stdout: JSON.stringify({ unknown: true, busy, reason: 'Process inspection unavailable.' }), stderr: '', command: 'x' });
    assert.equal(answer.busy, true);
    assert.equal(answer.unknown, true);
    assert.equal(answer.evidence, 'command');
    assert.equal(answer.error, 'Process inspection unavailable.');
  }
});

test('a service nobody described blocks, and says exactly what to write', async () => {
  const answer = await checkServiceBusy({ id: 'x', name: 'Edge', busy: { type: 'unmonitored' } });
  assert.equal(answer.busy, true, 'an unmonitored service must not permit a disruptive action');
  assert.equal(answer.monitored, false);
  assert.equal(answer.evidence, 'unmonitored');
  assert.match(answer.error, /"busy": \{"type": "none"\}/);
});

test('a service declared never busy permits action and is not called unmonitored', async () => {
  const answer = await checkServiceBusy({ id: 'x', name: 'X', busy: { type: 'none' } });
  assert.equal(answer.busy, false);
  assert.equal(answer.evidence, 'none');
});

test('the aggregate blocks when any service does, and names which', () => {
  const entries = [
    { service: { id: 'a', name: 'A' }, busy: { busy: false, unknown: false, monitored: true, reason: 'idle' } },
    { service: { id: 'b', name: 'B' }, busy: { busy: true, unknown: false, monitored: true, reason: '1 turn running' } },
    { service: { id: 'c', name: 'C' }, busy: { busy: true, unknown: true, monitored: false, reason: 'not monitored' } },
  ];
  const aggregate = aggregateBusy(entries);
  assert.equal(aggregate.busy, true);
  assert.equal(aggregate.unknown, true);
  assert.equal(aggregate.monitoredServices, 2);
  assert.equal(aggregate.unmonitoredServices, 1);
  assert.match(aggregate.reason, /B: 1 turn running/);
});

test('several probes share one budget instead of adding up', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home);
    writeFixture(dir, 'slow-seconds', '3');
    const slow = (id) => ({ id, name: id, busy: { type: 'command', command: call(dir, 'slow'), timeoutSeconds: 3 } });
    const started = Date.now();
    const { busy, entries } = await checkAllBusy({ services: [slow('a'), slow('b'), slow('c'), slow('d')] }, { budgetMs: 4000 });
    const elapsed = Date.now() - started;
    // Four three-second probes ran one after another used to take twelve seconds
    // and push status past the point where both clients had given up.
    assert.ok(elapsed < 7000, `four 3 s probes took ${elapsed} ms; they must overlap`);
    assert.equal(entries.length, 4);
    assert.equal(typeof busy.busy, 'boolean');
  });
});

test('a probe that does not fit in the budget blocks rather than running late', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home);
    writeFixture(dir, 'slow-seconds', '5');
    const service = { id: 'a', name: 'A', busy: { type: 'command', command: call(dir, 'slow'), timeoutSeconds: 5 } };
    const started = Date.now();
    const { busy } = await checkAllBusy({ services: [service] }, { budgetMs: 500 });
    assert.ok(Date.now() - started < 3000);
    assert.equal(busy.busy, true, 'a probe that could not be completed must fail closed');
    assert.equal(busy.unknown, true);
  });
});

// ---------------------------------------------------------------------------
// The state database probe
// ---------------------------------------------------------------------------

function makeDb(home, rows) {
  const { DatabaseSync } = require('node:sqlite');
  const dir = path.join(home, 't3', 'userdata');
  fs.mkdirSync(dir, { recursive: true });
  const file = path.join(dir, 'state.sqlite');
  const db = new DatabaseSync(file);
  db.exec('CREATE TABLE projection_turns (thread_id TEXT, turn_id TEXT, state TEXT, requested_at TEXT)');
  db.exec("CREATE TABLE projection_pending_approvals (request_id TEXT, thread_id TEXT, turn_id TEXT, created_at TEXT, status TEXT)");
  for (const row of rows) {
    db.prepare('INSERT INTO projection_turns VALUES (?, ?, ?, ?)').run(row.thread, row.turn, row.state, row.at);
  }
  db.close();
  return path.join(home, 't3');
}

import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);

test('a turn that is still running blocks however old it is', async () => {
  await withHome(async (home) => {
    const sevenHoursAgo = new Date(Date.now() - 7 * 60 * 60 * 1000).toISOString();
    const t3Home = makeDb(home, [{ thread: 't', turn: 'u', state: 'running', at: sevenHoursAgo }]);
    // The old rule aged this out after six hours, which is exactly when a long
    // agent run is most worth protecting.
    const answer = checkT3Sqlite({ home: t3Home, staleHours: 6 });
    assert.equal(answer.busy, true);
    assert.equal(answer.runningTurns, 1);
  });
});

test('a running turn is retired only by evidence from outside the database', async () => {
  await withHome(async (home) => {
    const anHourAgo = new Date(Date.now() - 60 * 60 * 1000).toISOString();
    const t3Home = makeDb(home, [{ thread: 't', turn: 'u', state: 'running', at: anHourAgo }]);

    // The service is not running: nothing can be executing the turn.
    const stopped = checkT3Sqlite({ home: t3Home, staleHours: 6 }, { liveness: { serviceRunning: false } });
    assert.equal(stopped.busy, false);
    assert.equal(stopped.abandonedTurns, 1);

    // The service restarted after the turn began: it cannot have survived that.
    const restarted = checkT3Sqlite(
      { home: t3Home, staleHours: 6 },
      { liveness: { serviceRunning: true, startedAt: new Date().toISOString() } },
    );
    assert.equal(restarted.busy, false);

    // The service has been up since before the turn: the turn is still live.
    const running = checkT3Sqlite(
      { home: t3Home, staleHours: 6 },
      { liveness: { serviceRunning: true, startedAt: new Date(Date.now() - 2 * 60 * 60 * 1000).toISOString() } },
    );
    assert.equal(running.busy, true);
  });
});

test('a pending turn still ages out on the staleness window', async () => {
  await withHome(async (home) => {
    const sevenHoursAgo = new Date(Date.now() - 7 * 60 * 60 * 1000).toISOString();
    const t3Home = makeDb(home, [{ thread: 't', turn: 'u', state: 'pending', at: sevenHoursAgo }]);
    const answer = checkT3Sqlite({ home: t3Home, staleHours: 6 });
    assert.equal(answer.busy, false, 'queued work with nothing executing does age out');
    assert.equal(answer.staleTurns, 1);
  });
});

test('a missing state database blocks unless the config says it is expected', async () => {
  await withHome(async (home) => {
    const nowhere = path.join(home, 'not-here');
    const blocked = checkT3Sqlite({ home: nowhere });
    assert.equal(blocked.busy, true, 'a wrong path and a fresh machine look identical from here');
    assert.equal(blocked.unknown, true);
    assert.match(blocked.error, /there is no state database/);

    const allowed = checkT3Sqlite({ home: nowhere, allowMissing: true });
    assert.equal(allowed.busy, false);
  });
});
