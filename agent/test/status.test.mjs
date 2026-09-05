import assert from 'node:assert/strict';
import fs from 'node:fs';
import https from 'node:https';
import http from 'node:http';
import { EventEmitter } from 'node:events';
import { createRequire } from 'node:module';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { deadline, normalizeConfig } from '../src/config.mjs';
import { beginOperation, operationPath, updateOperation } from '../src/operations.mjs';
import { probeProcess, probeProcessAsync } from '../src/probes/process.mjs';
import { fetchLatestVersion } from '../src/providers/app.mjs';
import { httpGet } from '../src/http.mjs';
import { acquireMutex, mutexPath } from '../src/mutex.mjs';
import { stateLockPath } from '../src/state.mjs';
import { buildStatus } from '../src/status.mjs';
import { cli, fixtureConfig, fixtureData, withHome, writeConfig } from './helpers.mjs';

const pause = (ms) => [process.execPath, '-e', `setTimeout(() => process.exit(0), ${ms})`];
function servicesConfig(dir, overrides, count = 3) {
  const fixture = fixtureConfig(dir);
  fixture.services = Array.from({ length: count }, (_, i) => ({
    ...fixture.services[0], id: `service${i}`, busy: { type: 'none' }, health: { type: 'none' },
    installedVersion: null, latestVersion: null, update: null, rollback: null, ...overrides,
  }));
  return normalizeConfig(fixture);
}

test('process probes across more than one batch share the total status budget', async () => {
  await withHome(async (home) => {
    const loaded = servicesConfig(fixtureData(home), { process: { type: 'command', running: pause(900) } }, 7);
    assert.equal(loaded.ok, true);
    let heartbeat = false;
    const timer = setTimeout(() => { heartbeat = true; }, 30);
    const started = Date.now();
    const result = await buildStatus(loaded, { budgetMs: 120 });
    clearTimeout(timer);
    assert.ok(Date.now() - started < 600, JSON.stringify(result.timing));
    assert.equal(heartbeat, true, 'status must not block the event loop');
    assert.equal(result.timing.partial, true);
    assert.equal(result.services.length, 7);
    for (const service of result.services) {
      assert.equal(service.process.state, 'unknown');
      assert.match(service.process.error, /timed out/);
      assert.equal(service.process.startedAt, null);
      assert.equal(service.healthy, false);
      assert.match(service.health.error, /unknown/);
      assert.equal(service.busy.unknown, true);
    }
  });
});

test('command version readers are async and cannot restart a consumed status budget', async () => {
  await withHome(async (home) => {
    const loaded = servicesConfig(fixtureData(home), {
      process: { type: 'none' }, installedVersion: pause(900), latestVersion: pause(900),
    }, 3);
    const started = Date.now();
    const result = await buildStatus(loaded, { budgetMs: 120 });
    assert.ok(Date.now() - started < 600, JSON.stringify(result.timing));
    for (const service of result.services) {
      assert.equal(service.installed, null);
      assert.equal(service.latest, null);
      assert.equal(service.upToDate, null);
    }
    assert.equal(result.timing.partial, true);
  });
});

test('successful process evidence is gathered once per service', async () => {
  await withHome(async (home) => {
    const count = path.join(home, 'probe-count');
    const running = [process.execPath, '-e', 'require("fs").appendFileSync(process.argv[1], "1")', count];
    const loaded = servicesConfig(fixtureData(home), { process: { type: 'command', running } }, 1);
    const result = await buildStatus(loaded, { budgetMs: 2000 });
    assert.equal(fs.readFileSync(count, 'utf8'), '1');
    assert.equal(result.services[0].process.running, true);
    assert.equal(result.services[0].process.error, null);
    assert.equal(result.timing.partial, false);
  });
});

test('systemd returns process state and start time in one bounded query', async () => {
  const calls = [];
  const probe = await probeProcessAsync({ process: { type: 'systemd-user', unit: 'demo.service' } }, {
    clock: deadline(200),
    runner: async (file, args, options) => {
      calls.push({ file, args, options });
      return { ok: true, code: 0, stdout: 'ActiveState=active\nActiveEnterTimestamp=2026-09-05T12:34:56.000Z\n' };
    },
  });
  assert.equal(calls.length, 1);
  assert.deepEqual(calls[0].args, ['--user', 'show', 'demo.service', '--property=ActiveState', '--property=ActiveEnterTimestamp']);
  assert.ok(calls[0].options.timeoutMs <= 200);
  assert.equal(probe.running, true);
  assert.equal(probe.startedAt, '2026-09-05T12:34:56.000Z');
});

test('Windows task query shares one process list for liveness and start time', async () => {
  let calls = 0;
  const probe = await probeProcessAsync({ name: 'Demo', process: { type: 'scheduled-task', match: 'demo' } }, {
    clock: deadline(100), includeRelay: true,
    runner: async (file, args) => {
      calls += 1;
      assert.equal(file, 'powershell.exe');
      assert.match(args.at(-1), /startedAt = \$started/);
      return { ok: true, code: 0, stdout: JSON.stringify({ running: true, pids: '42', startedAt: '2026-09-05T12:00:00Z', relayRunning: true }) };
    },
  });
  assert.equal(calls, 1);
  assert.equal(probe.startedAt, '2026-09-05T12:00:00.000Z');
  assert.deepEqual(probe.relay, { configured: true, running: true });
});

test('expired process deadline starts no commands and preserves configured relay', async () => {
  const probe = await probeProcessAsync({ process: { type: 'command', running: pause(1000) } }, {
    clock: deadline(0), includeRelay: true, runner: async () => { assert.fail('expired budget must not spawn'); },
  });
  assert.equal(probe.unitState, 'unknown');
  assert.deepEqual(probe.relay, { configured: true, running: false });
  assert.match(probe.error, /timed out/);
});

test('synchronous mutation process probe retains its return type and behavior', () => {
  const result = probeProcess({ process: { type: 'command', running: [process.execPath, '-e', 'process.exit(0)'] } });
  assert.equal(result.running, true);
  assert.equal(result.then, undefined);
});

test('release feeds stop at the absolute deadline even while response data keeps arriving', async (t) => {
  let destroyed = false;
  let chunks = 0;
  let interval;
  t.mock.method(https, 'get', (_url, _options, onResponse) => {
    const request = new EventEmitter();
    request.destroy = () => { destroyed = true; clearInterval(interval); };
    queueMicrotask(() => {
      const response = new EventEmitter();
      response.statusCode = 200;
      response.setEncoding = () => {};
      onResponse(response);
      interval = setInterval(() => { chunks += 1; response.emit('data', ' '); }, 5);
    });
    return request;
  });
  try {
    const started = Date.now();
    const result = await fetchLatestVersion({ latest: { repo: 'example/app' } }, { timeoutMs: 80 });
    assert.equal(result.version, null);
    assert.match(result.error, /timed out/);
    assert.ok(Date.now() - started < 350);
    assert.ok(chunks > 0);
    assert.equal(destroyed, true);
  } finally { clearInterval(interval); }
});

test('health and busy HTTP probes have an absolute timeout despite continuous response bytes', async () => {
  const intervals = new Set();
  const server = http.createServer((_request, response) => {
    response.writeHead(200);
    response.write(' ');
    const interval = setInterval(() => response.write(' '), 5);
    intervals.add(interval);
    response.on('close', () => { clearInterval(interval); intervals.delete(interval); });
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  try {
    for (const wantBody of [false, true]) {
      const started = Date.now();
      const result = await httpGet({ host: '127.0.0.1', port: server.address().port, timeoutMs: 80, wantBody });
      assert.equal(result.ok, false);
      assert.match(result.error, /timed out/);
      assert.ok(Date.now() - started < 350);
    }
  } finally {
    for (const interval of intervals) clearInterval(interval);
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
  }
});

test('a synchronous SQLite query cannot block status and worker scratch is removed', async () => {
  await withHome(async (home) => {
    const { DatabaseSync } = createRequire(import.meta.url)('node:sqlite');
    const t3 = path.join(home, 't3');
    fs.mkdirSync(path.join(t3, 'userdata'), { recursive: true });
    const database = path.join(t3, 'userdata', 'state.sqlite');
    const db = new DatabaseSync(database);
    db.exec(`CREATE VIEW projection_turns AS WITH RECURSIVE counts(n) AS
      (SELECT 1 UNION ALL SELECT n+1 FROM counts WHERE n < 100000000)
      SELECT CAST(n AS TEXT) AS thread_id, 'turn' AS turn_id, 'running' AS state,
        '2026-09-05T00:00:00Z' AS requested_at FROM counts`);
    db.close();
    const before = fs.readFileSync(database);
    const scratchBefore = fs.readdirSync(os.tmpdir()).filter((name) => name.startsWith('legionctl-status-probe-'));
    const loaded = servicesConfig(fixtureData(home), { process: { type: 'none' }, busy: { type: 't3-sqlite', home: t3 } }, 1);
    const started = Date.now();
    const result = await buildStatus(loaded, { budgetMs: 200 });
    assert.ok(Date.now() - started < 800, JSON.stringify(result.timing));
    assert.equal(result.services[0].busy.unknown, true);
    assert.equal(result.services[0].busy.evidence, 'timed-out');
    assert.deepEqual(fs.readFileSync(database), before);
    assert.deepEqual(fs.readdirSync(os.tmpdir()).filter((name) => name.startsWith('legionctl-status-probe-')), scratchBefore);
  });
});

test('status and history observe the configured boot target without changing the record', async () => {
  await withHome(async (home) => {
    const fixture = fixtureConfig(fixtureData(home), { services: [], system: { id: 'other', name: 'Other' } });
    writeConfig(home, fixture);
    const id = 'status-boot-observation';
    beginOperation({ id, kind: 'boot', target: 'other', systemId: 'test' });
    updateOperation(id, { phase: 'rebooting', bootId: 'previous-boot', pid: process.pid });
    const before = fs.readFileSync(operationPath(id), 'utf8');
    const result = await buildStatus(normalizeConfig(fixture), { budgetMs: 2000 });
    assert.equal(result.operations.recent.find((record) => record.id === id).action, 'rebooted');
    const history = cli(['history'], { home }).payload;
    assert.equal(history.operations.find((record) => record.id === id).action, 'rebooted');
    assert.equal(fs.readFileSync(operationPath(id), 'utf8'), before);
  });
});

test('status does not wait for a held state mutex to save a reconstructible version cache', async () => {
  await withHome(async (home) => {
    const fixture = fixtureConfig(fixtureData(home));
    fixture.services[0] = { ...fixture.services[0], process: { type: 'none' }, busy: { type: 'none' }, health: { type: 'none' },
      installedVersion: null, latestVersion: [process.execPath, '-e', 'console.log("2.0.0")'] };
    writeConfig(home, fixture);
    const held = acquireMutex(mutexPath(stateLockPath()), { waitMs: 0 });
    assert.equal(held.ok, true);
    try {
      const started = Date.now();
      const { payload, exitCode } = cli(['status', '--budget-ms', '500'], { home });
      assert.equal(exitCode, 0);
      assert.ok(Date.now() - started < 1000, JSON.stringify(payload.timing));
      assert.equal(payload.services[0].latest, '2.0.0');
      assert.equal(fs.existsSync(path.join(home, 'state', 'cache.json')), false);
    } finally { held.release(); }
  });
});

test('a fresh status process reads legacy state without creating migration files', async () => {
  await withHome(async (home) => {
    const fixture = fixtureConfig(fixtureData(home));
    fixture.services[0] = { ...fixture.services[0], process: { type: 'none' }, busy: { type: 'none' }, health: { type: 'none' },
      installedVersion: null, latestVersion: null };
    writeConfig(home, fixture);
    const legacyFile = path.join(home, 'state.json');
    fs.writeFileSync(legacyFile, JSON.stringify({ services: { demo: { pendingRestart: true, pendingVersion: '2.0.0' } },
      channelCache: { 'command:demo:latest': { version: '2.0.0', at: new Date().toISOString() } } }));
    const before = fs.readFileSync(legacyFile);
    const { payload, exitCode } = cli(['status'], { home });
    assert.equal(exitCode, 0);
    assert.equal(payload.services[0].pendingRestart, true);
    assert.equal(payload.services[0].pendingVersion, '2.0.0');
    assert.deepEqual(fs.readFileSync(legacyFile), before);
    assert.equal(fs.existsSync(path.join(home, 'state', 'service-demo.json')), false);
    assert.equal(fs.existsSync(path.join(home, 'state', 'legacy-first-service.json')), false);
    assert.equal(fs.existsSync(path.join(home, 'state', 'cache.json')), false);
  });
});
