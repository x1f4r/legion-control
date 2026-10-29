// The cycle's deadline: nothing that can leave a service down is started unless
// it can be over before the scheduler ends the cycle.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { loadConfig } from '../src/config.mjs';
import { beginOperation, queueOperation, readOperation, updateOperation } from '../src/operations.mjs';
import { CYCLE_BUDGET_MS, cycleBudget, runCycle, SCHEDULER_LIMIT_MS } from '../src/scheduler.mjs';
import { serviceState } from '../src/state.mjs';
import { recoveryWorstCaseMs, restartWorstCaseMs, runUpdate, updateWorstCaseMs } from '../src/update.mjs';
import { commandService, deadPid, fixtureConfig, fixtureData, npmService, readFixture, REPO, withHome, writeConfig } from './helpers.mjs';

const minute = 60 * 1000;

/** Write the document and read it back the way the agent does, so services carry their defaults. */
function configured(home, document) {
  writeConfig(home, document);
  const loaded = loadConfig();
  assert.equal(loaded.ok, true, loaded.error);
  return loaded.config;
}

test('a cycle budgets for the shortest scheduler limit, and the Linux unit allows that long', () => {
  assert.equal(SCHEDULER_LIMIT_MS, 30 * minute);
  assert.ok(CYCLE_BUDGET_MS < SCHEDULER_LIMIT_MS, 'some of it is kept back for the reply');
  const unit = fs.readFileSync(path.join(REPO, 'agent', 'install', 'legion-control-update.service'), 'utf8');
  const limit = Number(/^TimeoutStartSec=(\d+)$/m.exec(unit)?.[1]) * 1000;
  assert.ok(limit >= SCHEDULER_LIMIT_MS, `systemd ends a cycle after ${limit} ms`);
});

test('the worst case of an update counts its drain, and fits in a cycle as configured by default', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home);
    const config = configured(home, fixtureConfig(dir, {
      services: [
        commandService(dir),
        commandService(dir, { id: 'drained', name: 'Drained', drain: { command: [process.execPath, '-e', ''], timeoutSeconds: 600 } }),
      ],
    }));
    const [plain, drained] = config.services;
    const plainMs = updateWorstCaseMs(plain);
    const npm = configured(home, fixtureConfig(dir, { services: [npmService(dir, { pkg: 'demo-pkg', prefix: path.join(home, 'npm-prefix') })] })).services[0];
    assert.ok(updateWorstCaseMs(npm) > 10 * minute, 'an npm install may run for ten minutes');
    assert.ok(updateWorstCaseMs(npm) < CYCLE_BUDGET_MS, 'and still fits in a cycle that has just begun');
    assert.ok(plainMs > 10 * minute, 'the update command alone may run for ten minutes');
    assert.ok(plainMs < CYCLE_BUDGET_MS, 'or no scheduled update could ever start');
    assert.ok(updateWorstCaseMs(drained) >= plainMs + 10 * minute, 'a ten minute drain is ten minutes more');
    assert.ok(restartWorstCaseMs(plain) < plainMs);
    assert.ok(recoveryWorstCaseMs(plain, { rollback: true }) > recoveryWorstCaseMs(plain));
  });
});

test('the budget says what waits and why, and lets through what fits', () => {
  const budget = cycleBudget({ budgetMs: 20 * minute, startedAt: Date.now() - 15.5 * minute });
  assert.equal(budget.refuses(4 * minute, 'the update of Demo'), null);
  assert.equal(budget.refuses(6 * minute + 1, 'the update of Demo'), 'the update of Demo can take up to 7 min, and this cycle has 4 min left, so it waits for the next cycle');
  assert.match(budget.refuses(25 * minute, 'the update of Demo'), /longer than a whole cycle has \(20 min\), so no cycle starts it/);
  const spent = cycleBudget({ budgetMs: 20 * minute, startedAt: Date.now() - 21 * minute });
  assert.match(spent.refuses(1, 'the restart of Demo'), /this cycle has no time left/);
});

test('a cycle without the time for an update does not begin it, and the next one does', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home, { latest: '2.0.0' });
    const config = configured(home, fixtureConfig(dir));

    const short = await runCycle(config, { budgetMs: 1000 });
    assert.equal(short.action, 'cycled');
    assert.deepEqual(short.children.map((child) => [child.service, child.opId, child.action, child.reasonCode]), [['demo', null, 'deferred', 'timed-out']]);
    assert.match(short.children[0].message, /the update of Demo can take up to \d+ min, longer than a whole cycle has/);
    assert.equal(readFixture(dir, 'installed'), '1.0.0');
    assert.equal(readFixture(dir, 'stop-runs'), '');

    const full = await runCycle(config);
    assert.deepEqual(full.children.map((child) => [child.service, child.action]), [['demo', 'updated']]);
    assert.equal(readFixture(dir, 'installed'), '2.0.0');
  });
});

test('an update whose time runs out on the way to its stop waits, with the version it found', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home, { latest: '2.0.0' });
    const config = configured(home, fixtureConfig(dir));
    const [service] = config.services;
    const result = await runUpdate(config, service, { mode: 'scheduled', budget: cycleBudget({ budgetMs: 5 * minute }) });
    assert.equal(result.action, 'deferred');
    assert.equal(result.reasonCode, 'timed-out');
    assert.equal(result.to, '2.0.0');
    assert.match(result.message, /the update of Demo to 2\.0\.0 can take up to/);
    const record = readOperation(result.opId);
    assert.equal(record.result.outcome, 'deferred');
    assert.equal(serviceState(service).pendingVersion, '2.0.0', 'what waits is noted as waiting');
    assert.equal(readFixture(dir, 'stop-runs'), '', 'nothing was stopped');
    assert.equal(readFixture(dir, 'installed'), '1.0.0');
  });
});

test('a queued request a cycle has no time for keeps its place, and its expiry', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home, { latest: '2.0.0' });
    const config = configured(home, fixtureConfig(dir, { updates: { automatic: false } }));
    const restart = 'queued-budget-restart-01';
    assert.equal(queueOperation({ id: restart, kind: 'restart', service: 'demo', ttlMs: 60 * minute }).ok, true);
    const expiresAt = readOperation(restart).expiresAt;

    const short = await runCycle(config, { budgetMs: 1000 });
    assert.deepEqual(short.queue.map((entry) => [entry.opId, entry.action, entry.reasonCode]), [[restart, 'queued', 'timed-out']]);
    assert.equal(readOperation(restart).state, 'queued');
    assert.equal(readOperation(restart).expiresAt, expiresAt);
    assert.equal(readFixture(dir, 'stop-runs'), '');

    // Taken up, and then found short of time at its stop, an update goes back
    // to the queue just as one the machine was too busy for.
    const update = 'queued-budget-update-01';
    queueOperation({ id: update, kind: 'update', service: 'demo', ttlMs: 60 * minute });
    const deferred = await runUpdate(config, config.services[0], { existingOpId: update, mode: 'queued', budget: cycleBudget({ budgetMs: 5 * minute }) });
    assert.equal(deferred.action, 'queued');
    assert.equal(readOperation(update).state, 'queued');
    assert.equal(readOperation(update).pid, null);

    const full = await runCycle(config);
    assert.deepEqual(full.queue.map((entry) => [entry.opId, entry.action]).sort(), [[restart, 'restarted'], [update, 'updated']].sort());
    assert.equal(readFixture(dir, 'installed'), '2.0.0');
  });
});

test('recovery always puts back the first service, and what it has no time for waits', async () => {
  await withHome(async (home) => {
    const first = fixtureData(home, { running: 'off' });
    const second = path.join(home, 'second');
    fs.cpSync(first, second, { recursive: true });
    const config = configured(home, fixtureConfig(first, {
      updates: { automatic: false },
      services: [commandService(first), commandService(second, { id: 'other', name: 'Other' })],
    }));
    for (const [id, service] of [['budget-recovery-first', 'demo'], ['budget-recovery-other', 'other']]) {
      beginOperation({ id, kind: 'update', service, systemId: 'test' });
      updateOperation(id, { pid: deadPid(), phase: 'stopping' });
    }

    const short = await runCycle(config, { budgetMs: 1000 });
    const restored = short.recovery.restored.map((entry) => [entry.service, entry.action, entry.settled]);
    assert.deepEqual(restored, [['demo', 'started', true], ['other', 'deferred', false]]);
    assert.equal(readFixture(first, 'running'), 'on');
    assert.equal(readFixture(second, 'running'), 'off');
    assert.equal(readOperation('budget-recovery-other').recoveryPending, true);
    assert.match(readOperation('budget-recovery-other').log.at(-1).line, /recovery is still owed: putting Other back can take up to \d+ min/);
    assert.match(short.message, /recovery is still owed/);

    await runCycle(config);
    assert.equal(readFixture(second, 'running'), 'on');
    assert.equal(readOperation('budget-recovery-other').recoveryPending, false);
  });
});
