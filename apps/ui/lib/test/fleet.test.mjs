import assert from 'node:assert/strict';
import test from 'node:test';
import {
  callWithTimeout,
  hydrateFleetCache,
  refreshFleetProgressive,
  serializeFleetCache,
} from '../fleet.js';

test('serializeFleetCache keeps only non-secret machine fields and hydrateFleetCache restores them', () => {
  const saved = serializeFleetCache(
    'tower',
    [
      {
        name: 'tower',
        thisNode: true,
        reachable: true,
        os: 'linux',
        arch: 'x86_64',
        system: 'linux',
        version: '0.1.0',
        canWake: false,
        lastSeenAt: 1700000000000,
        jobs: [{ id: 'j-1', command: 'secret-cmd --token=xyz' }],
        error: { message: 'private detail' },
        sys: { hostname: 'tower' },
      },
      {
        name: 'laptop',
        thisNode: false,
        reachable: false,
        os: 'linux',
        arch: 'x86_64',
        system: null,
        version: null,
        canWake: true,
        lastSeenAt: 1699990000000,
      },
    ],
    1700000050000,
  );

  const json = JSON.stringify(saved);
  assert.ok(!json.includes('secret-cmd'));
  assert.ok(!json.includes('private detail'));
  assert.equal(saved.node, 'tower');
  assert.equal(saved.savedAt, 1700000050000);
  assert.equal(saved.machines.length, 2);

  const hydrated = hydrateFleetCache(saved, 'tower');
  assert.ok(hydrated);
  assert.equal(hydrated.stale, true);
  assert.equal(hydrated.cachedAt, 1700000050000);
  assert.equal(hydrated.machines[0].name, 'tower');
  assert.equal(hydrated.machines[0].status, 'asking');
  assert.equal(hydrated.machines[1].name, 'laptop');
  assert.equal(hydrated.machines[1].status, 'offline');
  assert.equal(hydrated.machines[1].asking, false);
  assert.equal(hydrated.machines[1].canWake, true);
  assert.equal(hydrated.machines[1].lastSeenAt, 1699990000000);

  // Different node does not reuse another node's cached fleet.
  assert.equal(hydrateFleetCache(saved, 'other'), null);
});

test('refreshFleetProgressive keeps previously-off machines offline without asking and throttles rechecks', async () => {
  const pinged = [];
  const link = {
    async call(tool, args = {}) {
      if (tool === 'fleet.list') {
        return {
          node: 'tower',
          machines: [
            { name: 'tower', this_node: true, systems: [], wake_via: [] },
            { name: 'laptop', this_node: false, systems: [], wake_via: ['pi'] },
          ],
        };
      }
      if (tool === 'meta.version') return { version: '0.1.0', os: 'linux', arch: 'x86_64' };
      if (tool === 'sys.info') return { os: { family: 'linux', arch: 'x86_64' }, uptime_s: 10, cpu: { usage_pct: 5 }, memory: { total: 100, used: 10 }, disks: [], gpus: [] };
      if (tool === 'job.list') return { jobs: [] };
      if (tool === 'power.status') return { system: 'linux', busy: { state: 'idle', reasons: [] } };
      if (tool === 'fleet.ping') {
        pinged.push(...(args.machines || []));
        return { results: [{ machine: 'laptop', reachable: false, error: { code: 'unreachable', message: 'Off' } }] };
      }
      throw new Error(`unexpected ${tool}`);
    },
  };

  const t0 = 1_700_000_000_000;
  const cached = serializeFleetCache(
    'tower',
    [
      { name: 'tower', thisNode: true, reachable: true, lastSeenAt: t0 },
      { name: 'laptop', thisNode: false, reachable: false, lastSeenAt: t0 - 86_400_000, lastCheckedAt: t0 },
    ],
    t0,
  );
  const hydrated = hydrateFleetCache(cached, 'tower');

  // Within 2 minutes and not forced: laptop stays offline and is not pinged.
  const seenStatuses = [];
  await refreshFleetProgressive({
    link,
    localNode: 'tower',
    initialMachines: hydrated.machines,
    now: () => t0 + 30_000,
    onUpdate(snap) {
      const m = snap.machines.find((x) => x.name === 'laptop');
      if (m) seenStatuses.push(m.status);
    },
  });
  assert.deepEqual(pinged, []);
  assert.ok(seenStatuses.every((s) => s === 'offline'));

  // Explicit pull-to-refresh (forceOffline: true) checks quietly while staying offline (never asking).
  seenStatuses.length = 0;
  await refreshFleetProgressive({
    link,
    localNode: 'tower',
    initialMachines: hydrated.machines,
    forceOffline: true,
    now: () => t0 + 35_000,
    onUpdate(snap) {
      const m = snap.machines.find((x) => x.name === 'laptop');
      if (m) seenStatuses.push(m.status);
    },
  });
  assert.deepEqual(pinged, ['laptop']);
  assert.ok(seenStatuses.every((s) => s === 'offline'));
});

test('refreshFleetProgressive shows local node first, pings once per remote, and skips detail calls for offline machines', async () => {
  const calls = [];
  let releaseDeadPing;
  const deadPingPromise = new Promise((resolve) => {
    releaseDeadPing = resolve;
  });

  const link = {
    async call(tool, args = {}, opts = {}) {
      calls.push({ tool, args, on: opts.on || null });
      if (tool === 'fleet.list') {
        return {
          node: 'tower',
          machines: [
            { name: 'tower', this_node: true, systems: [], wake_via: [] },
            { name: 'pi', this_node: false, systems: [], wake_via: [] },
            { name: 'dead', this_node: false, systems: [], wake_via: ['pi'] },
          ],
        };
      }
      if (tool === 'meta.version') {
        return { version: '0.1.0', os: 'linux', arch: 'x86_64' };
      }
      if (tool === 'fleet.ping') {
        const target = args.machines?.[0];
        if (target === 'pi') {
          return {
            results: [
              { machine: 'pi', reachable: true, os: 'linux', arch: 'aarch64', version: '0.1.0', latency_ms: 5 },
            ],
          };
        }
        if (target === 'dead') {
          return deadPingPromise;
        }
      }
      if (tool === 'sys.info') {
        const who = opts.on || 'tower';
        return {
          os: { family: 'linux', arch: who === 'pi' ? 'aarch64' : 'x86_64' },
          uptime_s: 120,
          cpu: { usage_pct: 10 },
          memory: { total: 1000, used: 200 },
          disks: [],
          gpus: [],
        };
      }
      if (tool === 'job.list') {
        return { jobs: [] };
      }
      if (tool === 'power.status') {
        return { system: 'linux', busy: { state: 'idle', reasons: [] } };
      }
      throw new Error(`unexpected tool ${tool}`);
    },
  };

  const snapshots = [];
  const run = refreshFleetProgressive({
    link,
    localNode: 'tower',
    onUpdate(snap) {
      snapshots.push(structuredClone(snap));
    },
    now: () => 1700000100000,
  });

  // Wait a couple ticks for tower and pi to answer while dead is still pending.
  await new Promise((r) => setTimeout(r, 25));

  const mid = snapshots.at(-1);
  assert.ok(mid, 'expected progressive updates before dead machine settled');
  const towerMid = mid.machines.find((m) => m.name === 'tower');
  const piMid = mid.machines.find((m) => m.name === 'pi');
  const deadMid = mid.machines.find((m) => m.name === 'dead');
  assert.equal(towerMid.status, 'answering');
  assert.equal(piMid.status, 'answering');
  assert.equal(deadMid.status, 'asking');

  // Now fail the dead ping.
  releaseDeadPing({
    results: [
      {
        machine: 'dead',
        reachable: false,
        error: { code: 'unreachable', message: 'Timed out.', hint: 'Check power.', machine: 'dead' },
      },
    ],
  });

  const finalSnap = await run;
  assert.equal(finalSnap.stale, false);
  const deadFinal = finalSnap.machines.find((m) => m.name === 'dead');
  assert.equal(deadFinal.status, 'offline');
  assert.equal(deadFinal.reason, 'Timed out.');

  // Dead machine received only fleet.ping, never sys.info/job.list/power.status.
  const callsForDead = calls.filter(
    (c) => c.on === 'dead' || (c.tool === 'fleet.ping' && c.args?.machines?.includes('dead')),
  );
  assert.deepEqual(
    callsForDead.map((c) => c.tool),
    ['fleet.ping'],
  );
});

test('callWithTimeout aborts a slow call independently without hanging', async () => {
  const link = {
    call(_tool, _args, { signal }) {
      return new Promise((_resolve, reject) => {
        signal.addEventListener('abort', () => {
          const err = new Error('aborted');
          err.name = 'AbortError';
          reject(err);
        });
      });
    },
  };

  await assert.rejects(
    () => callWithTimeout(link, 'sys.info', {}, { on: 'slow' }, 30),
    (err) => err.code === 'unreachable' && err.machine === 'slow',
  );
});

test('refreshFleetProgressive preserves busyState (busy object reasons and unmonitored)', async () => {
  const probeReason = { source: 'probe', name: 'agents', detail: 'busy: 42 worker' };
  const link = {
    async call(tool, args = {}, opts = {}) {
      if (tool === 'fleet.list') {
        return {
          node: 'tower',
          machines: [
            { name: 'tower', this_node: true, systems: [], wake_via: [] },
            { name: 'pi', this_node: false, systems: [], wake_via: [] },
          ],
        };
      }
      if (tool === 'meta.version') {
        return { version: '0.1.0', os: 'linux', arch: 'x86_64' };
      }
      if (tool === 'fleet.ping') {
        return {
          results: [
            { machine: 'pi', reachable: true, os: 'linux', arch: 'aarch64', version: '0.1.0', latency_ms: 4 },
          ],
        };
      }
      if (tool === 'sys.info') {
        return {
          os: { family: 'linux', arch: 'x86_64' },
          uptime_s: 60,
          cpu: { usage_pct: 5 },
          memory: { total: 1000, used: 100 },
          disks: [],
          gpus: [],
        };
      }
      if (tool === 'job.list') {
        return { jobs: [] };
      }
      if (tool === 'power.status') {
        if (opts.on === 'pi') {
          return { system: 'linux', busy: { state: 'busy', reasons: [probeReason] } };
        }
        return { system: 'linux', busy: { state: 'unmonitored', reasons: [] } };
      }
      throw new Error(`unexpected tool ${tool}`);
    },
  };

  const finalSnap = await refreshFleetProgressive({
    link,
    localNode: 'tower',
    onUpdate() {},
    now: () => 1700000200000,
  });

  const tower = finalSnap.machines.find((m) => m.name === 'tower');
  const pi = finalSnap.machines.find((m) => m.name === 'pi');
  assert.equal(tower.busy, false);
  assert.equal(tower.busyState, 'unmonitored');
  assert.deepEqual(tower.busyReasons, []);

  assert.equal(pi.busy, true);
  assert.equal(pi.busyState, 'busy');
  assert.deepEqual(pi.busyReasons, [probeReason]);
});
