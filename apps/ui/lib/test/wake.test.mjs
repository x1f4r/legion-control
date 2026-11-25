import assert from 'node:assert/strict';
import test from 'node:test';

import {
  createWakeController,
  formatWakeContinueLine,
  WAKE_POLL_INTERVAL_MS,
  WAKE_TIMEOUT_MS,
} from '../wake.js';

function createFakeClock(startMs = 1_000_000) {
  let nowMs = startMs;
  let nextId = 1;
  const intervals = new Map();

  return {
    now: () => nowMs,
    setInterval(fn, ms) {
      const id = nextId++;
      intervals.set(id, { fn, ms, nextAt: nowMs + ms });
      return id;
    },
    clearInterval(id) {
      intervals.delete(id);
    },
    async advance(ms) {
      const target = nowMs + ms;
      while (true) {
        let soonest = null;
        for (const item of intervals.values()) {
          if (item.nextAt <= target && (!soonest || item.nextAt < soonest.nextAt)) {
            soonest = item;
          }
        }
        if (!soonest) break;
        nowMs = soonest.nextAt;
        soonest.nextAt += soonest.ms;
        soonest.fn();
        await Promise.resolve();
        await Promise.resolve();
        await Promise.resolve();
      }
      nowMs = target;
    },
  };
}

function makeHarness({ wakeAfterPings = 2, canWake = true, allowWake = true } = {}) {
  const clock = createFakeClock();
  const calls = [];
  let pingCount = 0;
  let laptopReachable = false;

  const machines = () => [
    { name: 'pi', reachable: true, canWake: false },
    { name: 'laptop', reachable: laptopReachable, canWake },
  ];

  const link = {
    async call(tool, args, opts) {
      calls.push({ tool, args, opts });
      if (tool === 'power.wake') {
        return { machine: args.machine, awake: false, sent_by: 'pi' };
      }
      if (tool === 'fleet.ping') {
        pingCount += 1;
        if (wakeAfterPings !== Infinity && pingCount >= wakeAfterPings) {
          laptopReachable = true;
          return { results: [{ machine: 'laptop', reachable: true }] };
        }
        return { results: [{ machine: 'laptop', reachable: false }] };
      }
      return { ok: true };
    },
  };

  const controller = createWakeController({
    link,
    clock,
    getMachines: machines,
    getLocalNode: () => 'pi',
    getAllowedSet: () => (allowWake ? new Set(['power.wake', 'fleet.ping']) : new Set(['fleet.ping'])),
  });

  return { clock, calls, controller, setReachable: (v) => { laptopReachable = v; } };
}

test('ensureAwake runs immediately when machine is already on, and declines when wake is unavailable', async () => {
  const on = makeHarness();
  on.setReachable(true);
  let ran = 0;
  const res = await on.controller.ensureAwake('laptop', {
    kind: 'screen',
    onContinue: () => { ran += 1; },
  });
  assert.deepEqual(res, { awake: true, woke: false });
  assert.equal(ran, 1);
  assert.equal(on.calls.length, 0);

  const noPerm = makeHarness({ allowWake: false });
  const denied = await noPerm.controller.ensureAwake('laptop', { kind: 'screen' });
  assert.deepEqual(denied, { awake: false, canWake: false });
  assert.equal(noPerm.controller.getState().sheet, null);

  const noWol = makeHarness({ canWake: false });
  const unconfigured = await noWol.controller.ensureAwake('laptop', { kind: 'screen' });
  assert.deepEqual(unconfigured, { awake: false, canWake: false });
  assert.equal(noWol.controller.getState().sheet, null);
});

test('two waiters share one wake and one sheet, and each continues exactly once', async () => {
  const { clock, calls, controller } = makeHarness({ wakeAfterPings: 2 });
  let firstRan = 0;
  let secondRan = 0;

  const p1 = controller.ensureAwake('laptop', {
    kind: 'screen',
    onContinue: () => { firstRan += 1; },
  });
  const p2 = controller.ensureAwake('laptop', {
    kind: 'transfer',
    onContinue: () => { secondRan += 1; },
  });

  // Still waiting for human confirmation: no power.wake sent yet.
  assert.equal(calls.length, 0);
  const sheet = controller.getState().sheet;
  assert.equal(sheet.machineName, 'laptop');
  assert.equal(sheet.phase, 'prompt');
  assert.equal(sheet.waiterCount, 2);

  // Person clicks "Wake it and continue".
  await controller.confirmWake('laptop');
  assert.equal(calls.filter((c) => c.tool === 'power.wake').length, 1);
  assert.equal(controller.getState().sheet.phase, 'waking');

  // After 3 seconds: first ping still off; elapsed shows 3s.
  await clock.advance(WAKE_POLL_INTERVAL_MS);
  assert.equal(calls.filter((c) => c.tool === 'fleet.ping').length, 1);
  assert.equal(controller.getState().sheet.elapsedS, 3);
  assert.equal(firstRan, 0);

  // After 6 seconds: second ping answers -> both waiters run exactly once and sheet closes.
  await clock.advance(WAKE_POLL_INTERVAL_MS);
  const [r1, r2] = await Promise.all([p1, p2]);
  assert.deepEqual(r1, { awake: true, woke: true });
  assert.deepEqual(r2, { awake: true, woke: true });
  assert.equal(firstRan, 1);
  assert.equal(secondRan, 1);
  assert.equal(calls.filter((c) => c.tool === 'power.wake').length, 1);
  assert.equal(controller.getState().sheet, null);
});

test('timeout after 3 minutes shows timeout state and Try again restarts polling', async () => {
  const { clock, calls, controller } = makeHarness({ wakeAfterPings: Infinity });
  const p = controller.ensureAwake('laptop', { kind: 'action' });
  await controller.confirmWake('laptop');

  await clock.advance(WAKE_TIMEOUT_MS);
  const sheet = controller.getState().sheet;
  assert.equal(sheet.phase, 'timeout');
  assert.equal(sheet.machineName, 'laptop');

  // Retrying sends a fresh power.wake and returns to waking phase.
  await controller.retry('laptop');
  assert.equal(calls.filter((c) => c.tool === 'power.wake').length, 2);
  assert.equal(controller.getState().sheet.phase, 'waking');

  controller.dismiss('laptop');
  // Since 'action' is non-forgettable, dismissing while waking keeps it in background; stop cleanly.
  controller.dispose();
  assert.ok(p);
});

test('leaving screen forgets opening a screen or files, but keeps transfers and commands running', async () => {
  const { clock, controller } = makeHarness({ wakeAfterPings: 2 });
  let screenOpened = 0;
  let transferStarted = 0;

  const pScreen = controller.ensureAwake('laptop', {
    kind: 'screen',
    onContinue: () => { screenOpened += 1; },
  });
  const pTransfer = controller.ensureAwake('laptop', {
    kind: 'transfer',
    onContinue: () => { transferStarted += 1; },
  });

  await controller.confirmWake('laptop');
  // User navigates away while waking.
  controller.leaveScreen();
  assert.equal(controller.getState().sheet, null, 'sheet hides when leaving screen');
  const screenResult = await pScreen;
  assert.deepEqual(screenResult, { awake: false, forgotten: true });

  // Machine wakes up 6 seconds later: transfer still runs once, screen stays forgotten.
  await clock.advance(WAKE_POLL_INTERVAL_MS * 2);
  const transferResult = await pTransfer;
  assert.deepEqual(transferResult, { awake: true, woke: true });
  assert.equal(screenOpened, 0);
  assert.equal(transferStarted, 1);
});

test('immediate wake starts directly in waking state, fires onWakeSent, and formats continue lines', async () => {
  const clock = createFakeClock();
  const calls = [];
  const sent = [];
  let laptopReachable = false;

  const link = {
    async call(tool, args) {
      calls.push({ tool, args });
      if (tool === 'power.wake') return { machine: args.machine, awake: false, sent_by: 'pi' };
      if (tool === 'fleet.ping') return { results: [{ machine: 'laptop', reachable: laptopReachable }] };
      return { ok: true };
    },
  };

  const controller = createWakeController({
    link,
    clock,
    getMachines: () => [
      { name: 'pi', reachable: true, canWake: false },
      { name: 'laptop', reachable: laptopReachable, canWake: true },
    ],
    getLocalNode: () => 'pi',
    getAllowedSet: () => new Set(['power.wake', 'fleet.ping']),
    onWakeSent: (name) => sent.push(name),
  });

  assert.equal(formatWakeContinueLine({ kind: 'screen', label: 'open laptop' }, 'laptop'), 'Then: open laptop');
  assert.equal(formatWakeContinueLine({ kind: 'files' }, 'laptop'), 'Then: open its files');
  assert.equal(
    formatWakeContinueLine({ kind: 'transfer', label: 'send holiday.mp4 to laptop' }, 'laptop'),
    'Then: send holiday.mp4 to laptop',
  );
  assert.equal(
    formatWakeContinueLine({ kind: 'action', label: 'run Update packages' }, 'laptop'),
    'Then: run Update packages',
  );
  assert.equal(formatWakeContinueLine({ kind: 'wake' }, 'laptop'), null);

  const p = controller.ensureAwake('laptop', { kind: 'wake', immediate: true, forgettable: true });
  await Promise.resolve();
  assert.equal(controller.getState().sheet.phase, 'waking');
  assert.equal(calls.filter((c) => c.tool === 'power.wake').length, 1);
  assert.deepEqual(sent, ['laptop']);

  controller.dismiss('laptop');
  assert.equal(controller.getState().sheet, null);
  const res = await p;
  assert.deepEqual(res, { awake: false, forgotten: true });
});
