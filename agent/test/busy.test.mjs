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
  assert.equal(answer.unknown, false);
  assert.equal(answer.evidence, 'none');
  // A declaration is an answer, so it is monitored, and the aggregate counts
  // only the services that block for want of a probe as unmonitored.
  assert.equal(answer.monitored, true);
  const aggregate = aggregateBusy([
    { service: { id: 'x', name: 'X' }, busy: answer },
    { service: { id: 'y', name: 'Y' }, busy: await checkServiceBusy({ id: 'y', name: 'Y', busy: { type: 'unmonitored' } }) },
  ]);
  assert.equal(aggregate.monitoredServices, 1);
  assert.equal(aggregate.unmonitoredServices, 1);
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
    assert.match(restarted.reason, /left behind by a service that has restarted since/);

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

// ---------------------------------------------------------------------------
// The state database T3 writes now
// ---------------------------------------------------------------------------

const hoursAgo = (hours) => new Date(Date.now() - hours * 60 * 60 * 1000).toISOString();

/** statev2.sqlite with the tables the probe reads, in T3's own column layout. */
function makeCurrentDb(home, { threads = [], turns = [], subagents = [], runs = [], requests = [] } = {}) {
  const { DatabaseSync } = require('node:sqlite');
  const dir = path.join(home, 't3', 'userdata');
  fs.mkdirSync(dir, { recursive: true });
  const db = new DatabaseSync(path.join(dir, 'statev2.sqlite'));
  db.exec(`CREATE TABLE orchestration_v2_projection_threads (thread_id TEXT PRIMARY KEY, project_id TEXT NOT NULL, title TEXT NOT NULL,
    default_provider TEXT NOT NULL, runtime_mode TEXT NOT NULL, interaction_mode TEXT NOT NULL, active_provider_thread_id TEXT,
    created_at TEXT NOT NULL, updated_at TEXT NOT NULL, archived_at TEXT, deleted_at TEXT, payload_json TEXT NOT NULL, provider_instance_id TEXT)`);
  db.exec(`CREATE TABLE orchestration_v2_projection_provider_turns (provider_turn_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL,
    provider_thread_id TEXT NOT NULL, node_id TEXT NOT NULL, run_attempt_id TEXT, ordinal INTEGER NOT NULL, status TEXT NOT NULL,
    started_at TEXT, completed_at TEXT, payload_json TEXT NOT NULL)`);
  db.exec(`CREATE TABLE orchestration_v2_projection_subagents (subagent_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, run_id TEXT,
    parent_node_id TEXT NOT NULL, provider TEXT NOT NULL, provider_thread_id TEXT, child_thread_id TEXT, origin TEXT NOT NULL,
    status TEXT NOT NULL, started_at TEXT, completed_at TEXT, updated_at TEXT NOT NULL, payload_json TEXT NOT NULL, driver TEXT,
    provider_instance_id TEXT, completion_delivery_state TEXT)`);
  db.exec(`CREATE TABLE orchestration_v2_projection_runs (run_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, ordinal INTEGER NOT NULL,
    provider TEXT NOT NULL, provider_thread_id TEXT, status TEXT NOT NULL, requested_at TEXT NOT NULL, completed_at TEXT,
    payload_json TEXT NOT NULL, provider_instance_id TEXT)`);
  db.exec(`CREATE TABLE orchestration_v2_projection_runtime_requests (runtime_request_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL,
    node_id TEXT NOT NULL, provider_turn_id TEXT, kind TEXT NOT NULL, status TEXT NOT NULL, created_at TEXT NOT NULL, resolved_at TEXT,
    payload_json TEXT NOT NULL)`);
  for (const [index, row] of threads.entries()) {
    db.prepare(`INSERT INTO orchestration_v2_projection_threads VALUES (?, 'project', ?, 'claudeAgent', 'full-access', 'default', NULL, ?, ?, ?, ?, '{}', NULL)`)
      .run(row.thread, row.title ?? `Thread ${index}`, hoursAgo(48), hoursAgo(1), row.archivedAt ?? null, row.deletedAt ?? null);
  }
  for (const [index, row] of turns.entries()) {
    db.prepare(`INSERT INTO orchestration_v2_projection_provider_turns VALUES (?, ?, 'provider-thread', 'node', NULL, ?, ?, ?, NULL, '{}')`)
      .run(row.turn ?? `turn-${index}`, row.thread, index + 1, row.status, row.startedAt ?? null);
  }
  for (const [index, row] of subagents.entries()) {
    db.prepare(`INSERT INTO orchestration_v2_projection_subagents VALUES (?, ?, NULL, 'node', 'claudeAgent', NULL, NULL, ?, ?, ?, NULL, ?, '{}', NULL, NULL, NULL)`)
      .run(row.id ?? `agent-${index}`, row.thread, row.origin ?? 'provider_native', row.status, row.startedAt ?? null, row.updatedAt ?? row.startedAt ?? hoursAgo(0));
  }
  for (const [index, row] of runs.entries()) {
    db.prepare(`INSERT INTO orchestration_v2_projection_runs VALUES (?, ?, ?, 'claudeAgent', NULL, ?, ?, NULL, '{}', NULL)`)
      .run(row.id ?? `run-${index}`, row.thread, index + 1, row.status, row.requestedAt);
  }
  for (const [index, row] of requests.entries()) {
    db.prepare(`INSERT INTO orchestration_v2_projection_runtime_requests VALUES (?, ?, 'node', NULL, ?, ?, ?, NULL, '{}')`)
      .run(row.id ?? `request-${index}`, row.thread, row.kind ?? 'command', row.status, row.createdAt);
  }
  db.close();
  return path.join(home, 't3');
}

test('a turn running in the current database blocks however old it is, and is named by its thread', async () => {
  await withHome(async (home) => {
    const t3Home = makeCurrentDb(home, {
      threads: [{ thread: 't', title: 'Long agent run' }],
      turns: [{ thread: 't', status: 'running', startedAt: hoursAgo(7) }, { thread: 't', status: 'completed', startedAt: hoursAgo(9) }],
    });
    const answer = checkT3Sqlite({ home: t3Home, staleHours: 6 });
    assert.equal(answer.busy, true);
    assert.equal(answer.runningTurns, 1);
    assert.equal(answer.reason, '1 turn running');
    assert.equal(answer.threads[0].title, 'Long agent run');
    assert.equal(answer.threads[0].state, 'running');
  });
});

test('a running turn in the current database is retired only by outside evidence, and the reason names which', async () => {
  await withHome(async (home) => {
    const t3Home = makeCurrentDb(home, { turns: [{ thread: 't', status: 'running', startedAt: hoursAgo(1) }] });

    const stopped = checkT3Sqlite({ home: t3Home }, { liveness: { serviceRunning: false } });
    assert.equal(stopped.busy, false);
    assert.equal(stopped.abandonedTurns, 1);
    assert.equal(stopped.reason, 'idle (1 running row left behind by a service that is not running)');

    // T3 is running here. What retires the row is that it started after the
    // turn did, and the reason used to say it was not running at all.
    const restarted = checkT3Sqlite({ home: t3Home }, { liveness: { serviceRunning: true, startedAt: new Date().toISOString() } });
    assert.equal(restarted.busy, false);
    assert.equal(restarted.abandonedTurns, 1);
    assert.equal(restarted.reason, 'idle (1 running row left behind by a service that has restarted since)');

    const live = checkT3Sqlite({ home: t3Home }, { liveness: { serviceRunning: true, startedAt: hoursAgo(2) } });
    assert.equal(live.busy, true);
  });
});

test('a turn with no start time keeps its protection through a restart', async () => {
  await withHome(async (home) => {
    const t3Home = makeCurrentDb(home, { turns: [{ thread: 't', status: 'running', startedAt: null }] });
    assert.equal(checkT3Sqlite({ home: t3Home }, { liveness: { serviceRunning: true, startedAt: new Date().toISOString() } }).busy, true);
    assert.equal(checkT3Sqlite({ home: t3Home }, { liveness: { serviceRunning: false } }).busy, false);
  });
});

test('a subagent the provider runs in its own process blocks after the turn that started it has finished', async () => {
  await withHome(async (home) => {
    const t3Home = makeCurrentDb(home, {
      turns: [{ thread: 't', status: 'completed', startedAt: hoursAgo(2) }],
      subagents: [
        { thread: 't', status: 'running', startedAt: hoursAgo(1) },
        { thread: 't', status: 'pending', startedAt: null, updatedAt: hoursAgo(1) },
        // A subagent T3 runs as a thread of its own has turns of its own.
        { thread: 't', status: 'running', startedAt: hoursAgo(1), origin: 'app_owned' },
        { thread: 't', status: 'completed', startedAt: hoursAgo(2) },
      ],
    });
    const answer = checkT3Sqlite({ home: t3Home }, { liveness: { serviceRunning: true, startedAt: hoursAgo(3) } });
    assert.equal(answer.busy, true);
    assert.equal(answer.runningTurns, 2);

    // None of them outlives the provider process, so a restart since retires them.
    const restarted = checkT3Sqlite({ home: t3Home }, { liveness: { serviceRunning: true, startedAt: hoursAgo(0.5) } });
    assert.equal(restarted.busy, false);
    assert.equal(restarted.abandonedTurns, 2);
  });
});

test('approvals and questions waiting in the current database block until they go stale', async () => {
  await withHome(async (home) => {
    const fresh = makeCurrentDb(home, {
      requests: [
        { thread: 't', kind: 'command', status: 'pending', createdAt: hoursAgo(1) },
        { thread: 't', kind: 'user_input', status: 'pending', createdAt: hoursAgo(1) },
        { thread: 't', kind: 'command', status: 'resolved', createdAt: hoursAgo(1) },
      ],
    });
    const waiting = checkT3Sqlite({ home: fresh, staleHours: 6 });
    assert.equal(waiting.busy, true);
    assert.equal(waiting.pendingApprovals, 2);
    assert.equal(waiting.reason, '2 approvals waiting');
  });
  await withHome(async (home) => {
    const old = makeCurrentDb(home, { requests: [{ thread: 't', status: 'pending', createdAt: hoursAgo(7) }] });
    const stale = checkT3Sqlite({ home: old, staleHours: 6 });
    assert.equal(stale.busy, false);
    assert.equal(stale.staleApprovals, 1);
  });
});

test('a queued run waits as a pending turn and ages out on the staleness window', async () => {
  await withHome(async (home) => {
    const t3Home = makeCurrentDb(home, {
      runs: [
        { thread: 't', status: 'queued', requestedAt: hoursAgo(1) },
        { thread: 't', status: 'queued', requestedAt: hoursAgo(7) },
        { thread: 't', status: 'completed', requestedAt: hoursAgo(1) },
      ],
    });
    const answer = checkT3Sqlite({ home: t3Home, staleHours: 6 });
    assert.equal(answer.busy, true);
    assert.equal(answer.pendingTurns, 1);
    assert.equal(answer.staleTurns, 1);
  });
});

test('work left marked live in a deleted thread is not work, while an archived thread still holds some', async () => {
  await withHome(async (home) => {
    const t3Home = makeCurrentDb(home, {
      threads: [{ thread: 'gone', deletedAt: hoursAgo(1) }, { thread: 'shelved', archivedAt: hoursAgo(1) }],
      turns: [{ thread: 'gone', status: 'running', startedAt: hoursAgo(2) }],
      subagents: [{ thread: 'gone', status: 'running', startedAt: hoursAgo(2) }],
      requests: [{ thread: 'gone', status: 'pending', createdAt: hoursAgo(1) }],
    });
    assert.equal(checkT3Sqlite({ home: t3Home }).reason, 'idle');
  });
  await withHome(async (home) => {
    const t3Home = makeCurrentDb(home, {
      threads: [{ thread: 'shelved', archivedAt: hoursAgo(1) }],
      turns: [{ thread: 'shelved', status: 'running', startedAt: hoursAgo(2) }],
    });
    assert.equal(checkT3Sqlite({ home: t3Home }).busy, true);
  });
});

test('a legacy database older than the current one never decides, and a newer one still counts', async () => {
  await withHome(async (home) => {
    // The machine this was found on: two turns marked running in the spring,
    // in the file T3 stopped writing when it moved to statev2.
    const t3Home = makeDb(home, [{ thread: 'a', turn: 'u', state: 'running', at: '2026-04-18T10:00:00.000Z' }, { thread: 'b', turn: 'v', state: 'running', at: '2026-04-19T10:00:00.000Z' }]);
    makeCurrentDb(home, { turns: [{ thread: 't', status: 'completed', startedAt: hoursAgo(1) }] });
    const legacy = path.join(t3Home, 'userdata', 'state.sqlite');
    const current = path.join(t3Home, 'userdata', 'statev2.sqlite');
    const month = new Date(Date.now() - 30 * 24 * 60 * 60 * 1000);
    fs.utimesSync(legacy, month, month);
    const answer = checkT3Sqlite({ home: t3Home });
    assert.equal(answer.busy, false, 'two frozen rows in the old file must not block every update');
    assert.equal(answer.reason, 'idle');

    // A T3 that went back to the old schema writes the old file again.
    const hour = new Date(Date.now() - 60 * 60 * 1000);
    fs.utimesSync(legacy, new Date(), new Date());
    fs.utimesSync(current, hour, hour);
    const newer = checkT3Sqlite({ home: t3Home });
    assert.equal(newer.busy, true);
    assert.equal(newer.runningTurns, 2);
  });
});

test('a legacy database still being written through its log counts as written then', async () => {
  await withHome(async (home) => {
    const { DatabaseSync } = require('node:sqlite');
    const t3Home = makeCurrentDb(home);
    const legacy = path.join(t3Home, 'userdata', 'state.sqlite');
    const writer = new DatabaseSync(legacy);
    try {
      writer.exec('PRAGMA journal_mode = WAL');
      writer.exec('CREATE TABLE projection_turns (thread_id TEXT, turn_id TEXT, state TEXT, requested_at TEXT)');
      writer.exec('CREATE TABLE projection_pending_approvals (request_id TEXT, thread_id TEXT, turn_id TEXT, created_at TEXT, status TEXT)');
      writer.prepare('INSERT INTO projection_turns VALUES (?, ?, ?, ?)').run('t', 'u', 'running', hoursAgo(1));
      assert.equal(fs.existsSync(`${legacy}-wal`), true);
      // The main file has not been checkpointed since long before statev2 was
      // written; its log was written just now.
      const old = new Date(Date.now() - 30 * 24 * 60 * 60 * 1000);
      const hour = new Date(Date.now() - 60 * 60 * 1000);
      fs.utimesSync(legacy, old, old);
      fs.utimesSync(path.join(t3Home, 'userdata', 'statev2.sqlite'), hour, hour);
      assert.equal(checkT3Sqlite({ home: t3Home }).runningTurns, 1);
    } finally {
      writer.close();
    }
  });
});

test('a current database T3 has changed beyond reading blocks rather than reading as idle', async () => {
  await withHome(async (home) => {
    const { DatabaseSync } = require('node:sqlite');
    const dir = path.join(home, 't3', 'userdata');
    fs.mkdirSync(dir, { recursive: true });
    const db = new DatabaseSync(path.join(dir, 'statev2.sqlite'));
    db.exec('CREATE TABLE orchestration_v2_projection_provider_turns (provider_turn_id TEXT PRIMARY KEY)');
    db.close();
    const answer = checkT3Sqlite({ home: path.join(home, 't3') });
    assert.equal(answer.busy, true);
    assert.equal(answer.unknown, true);
    assert.equal(answer.evidence, 'probe-error');
  });
});
