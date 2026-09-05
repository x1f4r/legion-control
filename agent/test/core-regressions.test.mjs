import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { cli, fixtureConfig, fixtureData, readFixture, withHome, writeConfig } from './helpers.mjs';
import { acquireOperationLock } from '../src/lock.mjs';
import {
  beginOperation, deriveState, judgeInterrupted, operationPath,
  readOperation, recoverOperations, updateOperation,
} from '../src/operations.mjs';
import { runCycle } from '../src/scheduler.mjs';

const command = (source) => [process.execPath, '-e', source];
function powerFixture(home) {
  const dir = fixtureData(home);
  const marker = path.join(home, 'reboot-marker');
  const config = fixtureConfig(dir, {
    services: [], updates: { automatic: false, maintenanceWindows: [] },
    boot: {
      targets: { other: { method: 'command', arm: command('process.exit(0)'), verify: command('process.exit(0)') } },
      reboot: command(`require('node:fs').writeFileSync(${JSON.stringify(marker)}, 'requested')`),
    },
  });
  writeConfig(home, config);
  return { config, marker };
}

test('retrying a running cycle preserves its live record byte for byte', async () => {
  await withHome(async (home) => {
    powerFixture(home);
    const id = 'cycle-live-retry-001';
    beginOperation({ id, kind: 'cycle', mode: 'scheduled', force: false, systemId: 'test' });
    updateOperation(id, { children: [{ service: 'existing', action: 'updated' }] });
    const held = acquireOperationLock({ kind: 'cycle', opId: id });
    assert.equal(held.ok, true);
    try {
      const before = fs.readFileSync(operationPath(id), 'utf8');
      const retry = cli(['cycle', '--op', id], { home }).payload;
      assert.equal(retry.action, 'accepted');
      assert.equal(retry.replayed, true);
      assert.equal(retry.op.state, 'running');
      assert.equal(fs.readFileSync(operationPath(id), 'utf8'), before);
      const changed = cli(['cycle', '--force', '--op', id], { home }).payload;
      assert.equal(changed.action, 'conflict');
      assert.equal(fs.readFileSync(operationPath(id), 'utf8'), before);
    } finally { held.lock.release(); }
  });
});

test('a cycle id binds its selected services', async () => {
  await withHome(async (home) => {
    const { config } = powerFixture(home);
    const id = 'cycle-selection-001';
    beginOperation({ id, kind: 'cycle', services: ['a'], systemId: 'test' });
    const before = fs.readFileSync(operationPath(id), 'utf8');
    assert.equal((await runCycle(config, { opId: id, services: ['b'] })).action, 'conflict');
    assert.equal(fs.readFileSync(operationPath(id), 'utf8'), before);
  });
});

test('queued arm-only boot keeps its intent and never dispatches a reboot', async () => {
  await withHome(async (home) => {
    const { marker } = powerFixture(home);
    const id = 'queued-arm-only-001';
    assert.equal(cli(['boot', 'other', '--when-idle', '--no-reboot', '--op', id], { home }).payload.action, 'queued');
    assert.equal(readOperation(id).noReboot, true);
    assert.equal(cli(['boot', 'other', '--when-idle', '--op', id], { home }).payload.action, 'conflict');
    assert.equal(cli(['cycle'], { home }).payload.action, 'cycled');
    assert.equal(fs.existsSync(marker), false);
    const record = readOperation(id);
    assert.equal(record.state, 'finished');
    assert.equal(record.result.action, 'armed');
  });
});

test('arm-only and ordinary boot cannot reuse an operation id', async () => {
  await withHome(async (home) => {
    const { marker } = powerFixture(home);
    const id = 'arm-intent-001';
    assert.equal(cli(['boot', 'other', '--no-reboot', '--op', id], { home }).payload.action, 'armed');
    const before = fs.readFileSync(operationPath(id), 'utf8');
    assert.equal(cli(['boot', 'other', '--op', id], { home }).payload.action, 'conflict');
    assert.equal(fs.readFileSync(operationPath(id), 'utf8'), before);
    assert.equal(fs.existsSync(marker), false);
    assert.equal(cli(['boot', 'other', '--no-reboot', '--op', id], { home }).payload.replayed, true);
  });
});

test('a reboot acknowledgement stays open and only a changed boot on the requested system completes it', async () => {
  await withHome(async (home) => {
    const { marker } = powerFixture(home);
    const id = 'boot-observation-001';
    const reply = cli(['boot', 'other', '--op', id], { home }).payload;
    assert.equal(reply.action, 'rebooting');
    assert.equal(fs.existsSync(marker), true);
    const record = readOperation(id);
    assert.equal(record.state, 'running');
    assert.equal(record.phase, 'rebooting');
    assert.equal(record.result, null);
    assert.equal(record.dispatchAcknowledgement.action, 'rebooting');
    assert.equal(reply.op.result, null);
    assert.equal(reply.op.dispatchAcknowledgement, undefined);
    assert.ok(record.pid);
    assert.equal(cli(['op', id], { home }).payload.op.state, 'running');
    assert.equal(cli(['cycle'], { home }).payload.action, 'conflict');
    assert.deepEqual(recoverOperations({ systemId: 'other', bootId: record.bootId }), []);
    // A pid reused after reboot cannot prevent evidence-based recovery.
    updateOperation(id, { pid: process.pid });
    const recovered = recoverOperations({ systemId: 'other', bootId: 'new-observed-boot' });
    assert.equal(recovered.find((item) => item.id === id).result.action, 'rebooted');
    assert.equal(readOperation(id).state, 'finished');
  });
});

test('read-only op observation uses the currently configured system and preserves the file', async () => {
  await withHome(async (home) => {
    const { config } = powerFixture(home);
    const id = 'boot-read-only-001';
    beginOperation({ id, kind: 'boot', target: 'other', systemId: 'test' });
    updateOperation(id, { phase: 'rebooting', bootId: 'previous-boot', pid: process.pid });
    config.system = { id: 'other', name: 'Other' };
    writeConfig(home, config);
    const before = fs.readFileSync(operationPath(id), 'utf8');
    const observed = cli(['op', id], { home }).payload;
    assert.equal(observed.op.result.action, 'rebooted');
    assert.equal(observed.op.phase, 'done');
    assert.equal(observed.op.pid, null);
    assert.ok(observed.op.finishedAt);
    assert.equal(fs.readFileSync(operationPath(id), 'utf8'), before);
  });
});

test('pre-dispatch, arm-only, same-boot, and unknown-identity records never prove reboot success', () => {
  const record = { kind: 'boot', target: 'other', bootId: 'before', phase: 'rebooting' };
  assert.equal(judgeInterrupted(record, { systemId: 'other', bootId: 'before' }).action, 'interrupted');
  assert.equal(judgeInterrupted(record, { systemId: 'other', bootId: null }).action, 'interrupted');
  assert.equal(judgeInterrupted({ ...record, phase: 'arming' }, { systemId: 'other', bootId: 'after' }).action, 'interrupted');
  assert.equal(judgeInterrupted({ ...record, noReboot: true }, { systemId: 'other', bootId: 'after' }).action, 'interrupted');
  assert.equal(judgeInterrupted(record, { systemId: 'wrong', bootId: 'after' }).action, 'failed');
});

test('unobserved power acknowledgements expire into unknown outcomes rather than success', async () => {
  await withHome(async () => {
    for (const kind of ['boot', 'sleep']) {
      const id = `power-timeout-${kind}`;
      beginOperation({ id, kind, target: kind === 'boot' ? 'other' : null });
      const record = updateOperation(id, {
        phase: kind === 'boot' ? 'rebooting' : 'suspending', pid: null,
        transitionPendingUntil: new Date(Date.now() - 1).toISOString(),
      });
      assert.equal(deriveState(record, { systemId: 'other', bootId: record.bootId }).result.action, 'interrupted');
    }
  });
});

for (const broken of ['{broken', '', '[]']) {
  for (const force of [false, true]) {
    test(`invalid required state blocks all updater work before mutation (${JSON.stringify(broken)}, force=${force})`, async () => {
      await withHome(async (home) => {
        const dir = fixtureData(home, { latest: '2.0.0' });
        writeConfig(home, fixtureConfig(dir));
        fs.mkdirSync(path.join(home, 'state'));
        const state = path.join(home, 'state', 'service-demo.json');
        fs.writeFileSync(state, broken);
        const result = cli(['update', '--service', 'demo', ...(force ? ['--force'] : [])], { home }).payload;
        assert.equal(result.ok, false);
        assert.equal(result.action, 'failed');
        assert.match(result.message, /no service work was started/);
        assert.equal(readFixture(dir, 'installed'), '1.0.0');
        assert.equal(readFixture(dir, 'update-runs'), '');
        assert.equal(readFixture(dir, 'stop-runs'), '');
        assert.equal(fs.readFileSync(state, 'utf8'), broken);
      });
    });
  }
}

test('a state save failure after installation reports failure and the version actually installed', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home, { latest: '2.0.0' });
    const config = fixtureConfig(dir);
    const state = path.join(home, 'state', 'service-demo.json');
    config.services[0].update = command(`const fs=require('node:fs');fs.writeFileSync(${JSON.stringify(path.join(dir, 'installed'))},'2.0.0');fs.writeFileSync(${JSON.stringify(state)},'{broken-after-install');`);
    writeConfig(home, config);
    const result = cli(['update', '--service', 'demo', '--op', 'state-after-install'], { home }).payload;
    assert.equal(result.action, 'failed');
    assert.equal(result.ok, false);
    assert.equal(result.to, '2.0.0');
    assert.match(result.message, /state could not be saved/);
    assert.equal(readFixture(dir, 'installed'), '2.0.0');
    assert.equal(readFixture(dir, 'running'), 'on');
    assert.equal(readOperation('state-after-install').result.action, 'failed');
  });
});

test('cycle children honor live watchdog locks, even with force, and preserve the foreign file', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home, { latest: '2.0.0' });
    const config = fixtureConfig(dir);
    const lock = path.join(home, 'service.lock');
    const contents = `target\npid=${process.pid}\n`;
    fs.writeFileSync(lock, contents);
    fs.utimesSync(lock, new Date(0), new Date(0));
    config.services[0].lockFile = lock;
    writeConfig(home, config);
    for (const args of [['update', '--service', 'demo'], ['cycle'], ['cycle', '--force']]) {
      const result = cli(args, { home }).payload;
      assert.equal(args[0] === 'cycle' ? result.children[0].action : result.action, 'conflict');
      assert.equal(readFixture(dir, 'installed'), '1.0.0');
      assert.equal(readFixture(dir, 'update-runs'), '');
      assert.equal(fs.readFileSync(lock, 'utf8'), contents);
    }
  });
});

test('a fresh busy response returns a claimed request to its queue with the original expiry', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home);
    const config = fixtureConfig(dir, { updates: { automatic: false } });
    const count = path.join(home, 'busy-count');
    config.services[0].busy = { type: 'command', command: command(`const fs=require('node:fs');const f=${JSON.stringify(count)};const n=fs.existsSync(f)?Number(fs.readFileSync(f,'utf8')):0;fs.writeFileSync(f,String(n+1));process.exit(n===1?1:0);`) };
    writeConfig(home, config);
    const id = 'queued-busy-race-001';
    assert.equal(cli(['restart', '--service', 'demo', '--when-idle', '--op', id], { home }).payload.action, 'queued');
    const expiresAt = readOperation(id).expiresAt;
    cli(['cycle'], { home });
    const waiting = readOperation(id);
    assert.equal(waiting.state, 'queued');
    assert.equal(waiting.expiresAt, expiresAt);
    assert.equal(waiting.pid, null);
    assert.equal(waiting.startedAt, null);
    assert.equal(waiting.workerClaimed, false);
    assert.equal(readFixture(dir, 'stop-runs'), '');
    cli(['cycle'], { home });
    assert.equal(readOperation(id).state, 'finished');
    assert.equal(readOperation(id).result.action, 'restarted');
  });
});

test('queued boot already on target and update with a removed update command finish as no-ops', async () => {
  await withHome(async (home) => {
    const { config } = powerFixture(home);
    cli(['boot', 'other', '--when-idle', '--op', 'queue-now-current'], { home });
    config.system.id = 'other';
    writeConfig(home, config);
    cli(['cycle'], { home });
    assert.equal(readOperation('queue-now-current').result.action, 'noop');
    const dir = fixtureData(home);
    const servicesConfig = fixtureConfig(dir, { updates: { automatic: false } });
    writeConfig(home, servicesConfig);
    cli(['update', '--service', 'demo', '--when-idle', '--op', 'queue-removed-update'], { home });
    delete servicesConfig.services[0].update;
    writeConfig(home, servicesConfig);
    cli(['cycle'], { home });
    assert.equal(readOperation('queue-removed-update').result.action, 'noop');
  });
});

test('a queued arm-only request keeps its intent through op-run and replays its finished outcome', async () => {
  await withHome(async (home) => {
    const { marker } = powerFixture(home);
    const id = 'queued-arm-worker-001';
    const args = ['boot', 'other', '--when-idle', '--no-reboot', '--op', id];
    assert.equal(cli(args, { home }).payload.action, 'queued');
    assert.equal(cli(['op-run', id], { home }).payload.action, 'armed');
    assert.equal(fs.existsSync(marker), false);
    const replay = cli(args, { home }).payload;
    assert.equal(replay.action, 'armed');
    assert.equal(replay.replayed, true);
    assert.equal(replay.op.state, 'finished');
  });
});
