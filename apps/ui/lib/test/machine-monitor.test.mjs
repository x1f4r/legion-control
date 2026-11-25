import assert from 'node:assert/strict';
import test from 'node:test';

import {
  buildOverviewMeters,
  createOverviewPoller,
  formatOverviewStateLine,
  isOverviewBusy,
  machineAttentionLabel,
  selectBusyPrograms,
} from '../machine-monitor.js';

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
      }
      nowMs = target;
    },
  };
}

test('overview state line marks Busy only when CPU or a GPU exceeds 60 percent', () => {
  assert.equal(isOverviewBusy({ cpu: { usage_pct: 60 }, gpus: [{ utilization_pct: 60 }] }), false);
  assert.equal(isOverviewBusy({ cpu: { usage_pct: 61 }, gpus: [] }), true);
  assert.equal(isOverviewBusy({ cpu: { usage_pct: 10 }, gpus: [{ utilization_pct: 65 }] }), true);
  assert.equal(
    formatOverviewStateLine({ uptime_s: 3 * 86400, cpu: { usage_pct: 72 }, gpus: [] }),
    'On for 3 days · Busy',
  );
  assert.equal(
    formatOverviewStateLine({ uptime_s: 3 * 86400, cpu: { usage_pct: 12 }, gpus: [] }),
    'On for 3 days · Idle',
  );
});

test('meters are omitted when optional values are absent', () => {
  assert.deepEqual(buildOverviewMeters({}), []);
  assert.deepEqual(
    buildOverviewMeters({
      os: { family: 'linux' },
      uptime_s: 300,
      gpus: [{ name: 'Intel GPU', vendor: 'intel' }],
      disks: [],
    }),
    [],
  );

  const partial = buildOverviewMeters({
    cpu: { usage_pct: 24, cores: 4 },
    memory: { total: 8e9, used: 2e9 },
  });
  assert.deepEqual(
    partial.map((m) => m.id),
    ['cpu', 'memory'],
  );
});

test('amber thresholds fire on storage under 5% free, 85 C or more, and battery <= 15% not charging', () => {
  // Storage: 4% free -> amber; 5% free -> not amber.
  const lowDisk = buildOverviewMeters({
    disks: [{ mount: '/', total: 1000e9, available: 40e9 }],
  });
  assert.equal(lowDisk[0].id, 'storage');
  assert.equal(lowDisk[0].amber, true);
  assert.equal(lowDisk[0].detail, '40 GB free');
  assert.equal(
    machineAttentionLabel({ disks: [{ mount: '/', total: 1000e9, available: 40e9 }] }),
    'Storage almost full',
  );

  const okDisk = buildOverviewMeters({
    disks: [{ mount: '/', total: 1000e9, available: 50e9 }],
  });
  assert.equal(okDisk[0].amber, false);
  assert.equal(
    machineAttentionLabel({ disks: [{ mount: '/', total: 1000e9, available: 50e9 }] }),
    null,
  );

  // Temperature: 85 C -> amber; 84.9 C -> not amber.
  const hot = buildOverviewMeters({
    cpu: { temperature_c: 70 },
    gpus: [{ name: 'RTX', temperature_c: 85 }],
  });
  assert.equal(hot[0].id, 'temperature');
  assert.equal(hot[0].amber, true);
  assert.equal(hot[0].detail, '85 °C · graphics');
  assert.equal(
    machineAttentionLabel({ cpu: { temperature_c: 70 }, gpus: [{ name: 'RTX', temperature_c: 85 }] }),
    'Running hot',
  );
  assert.equal(machineAttentionLabel({ cpu: { temperature_c: 84.9 } }), null);

  // Battery: <= 15% discharging -> amber; <= 15% charging -> not amber; 16% discharging -> not amber.
  const lowBatt = buildOverviewMeters({
    battery: { percent: 15, state: 'discharging', on_ac: false, time_left_s: 1200 },
  });
  assert.equal(lowBatt[0].id, 'battery');
  assert.equal(lowBatt[0].amber, true);
  assert.equal(
    machineAttentionLabel({ battery: { percent: 15, state: 'discharging', on_ac: false } }),
    'Battery low',
  );
  assert.equal(
    machineAttentionLabel({ battery: { percent: 10, state: 'charging', on_ac: true } }),
    null,
  );
  assert.equal(
    machineAttentionLabel({ battery: { percent: 16, state: 'discharging', on_ac: false } }),
    null,
  );
});

test('selectBusyPrograms filters by 5% whole CPU, 5% memory, or 256 MB GPU and caps at five', () => {
  const sys = {
    cpu: { cores: 32 },
    memory: { total: 64e9 },
    gpus: [{ name: 'RTX 5080', memory_total: 16e9 }],
  };
  const rows = [
    { name: 'idle-daemon', user: 'me', count: 1, cpu_pct: 50, memory: 100e6, members: [] }, // 1.56% whole CPU -> excluded
    { name: 'rustc', user: 'me', count: 12, cpu_pct: 1984, memory: 2e9, members: [{ pid: 10, start_time: '2026-11-25T05:00:00Z' }] }, // 62% whole CPU
    { name: 'python3', user: 'me', count: 1, cpu_pct: 20, memory: 2e9, gpu_memory: 9.1e9, members: [] }, // 9.1 GB GPU
    { name: 'chrome', user: 'me', count: 8, cpu_pct: 32, memory: 6.4e9, members: [] }, // 10% memory
    { name: 'p4', user: 'me', count: 1, cpu_pct: 320, memory: 1e9, members: [] }, // 10% CPU
    { name: 'p5', user: 'me', count: 1, cpu_pct: 256, memory: 1e9, members: [] }, // 8% CPU
    { name: 'p6', user: 'me', count: 1, cpu_pct: 192, memory: 1e9, members: [] }, // 6% CPU
  ];
  const picked = selectBusyPrograms(rows, sys);
  assert.equal(picked.length, 5);
  assert.equal(picked[0].name, 'rustc');
  assert.equal(picked[0].primaryText, '62% of the processor');
  assert.equal(picked[1].name, 'python3');
  assert.equal(picked[1].primaryText, '9.1 GB of graphics memory');
  assert.ok(!picked.some((p) => p.name === 'idle-daemon'));
});

test('overview poller never overlaps in-flight calls, pauses when hidden, and dims after 15 seconds', async () => {
  const clock = createFakeClock();
  let visible = true;
  let calls = 0;
  let resolveCurrent = null;
  const staleHistory = [];

  const poller = createOverviewPoller({
    clock,
    isVisible: () => visible,
    onStaleChange: (s) => staleHistory.push(s),
    fetchTick() {
      calls += 1;
      return new Promise((resolve) => {
        resolveCurrent = resolve;
      });
    },
  });

  poller.start({ immediate: true });
  assert.equal(calls, 1);

  // Advance 16 seconds while the first call is still hanging: no overlapping ticks, becomes stale.
  await clock.advance(16_000);
  assert.equal(calls, 1, 'must never start an overlapping tick while previous call is in flight');
  assert.equal(poller.getState().stale, true);

  // First call finally completes -> clears stale flag.
  resolveCurrent(true);
  await Promise.resolve();
  await Promise.resolve();
  assert.equal(poller.getState().stale, false);

  // Hide the screen: advancing 20 seconds makes zero calls.
  visible = false;
  await clock.advance(20_000);
  assert.equal(calls, 1, 'hidden screen must ask nothing');

  // Show again -> fetches immediately.
  visible = true;
  poller.onVisibilityChange();
  assert.equal(calls, 2);

  poller.stop();
  assert.ok(staleHistory.includes(true));
});
