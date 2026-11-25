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

test('refreshFleetProgressive in app mode keeps selected node fixed when A is off and B answers fleet.list', async () => {
  const seenOn = [];
  const link = {
    kind: 'app',
    async call(tool, args = {}, opts = {}) {
      seenOn.push({ tool, on: opts.on ?? null });
      if (tool === 'fleet.list') {
        // Originally selected node tower is off; pi answered discovery read.
        return {
          node: 'pi',
          answered_by: 'pi',
          machines: [
            { name: 'pi', this_node: true, systems: [], wake_via: [] },
            { name: 'tower', this_node: false, systems: [], wake_via: [] },
          ],
        };
      }
      if (tool === 'fleet.ping') {
        assert.deepEqual(args.machines, ['tower']);
        return {
          answered_by: 'pi',
          results: [
            {
              machine: 'tower',
              reachable: false,
              error: { code: 'unreachable', message: 'tower did not answer.', machine: 'tower' },
            },
          ],
        };
      }
      if (opts.on === 'tower') {
        throw { code: 'unreachable', message: 'tower did not answer.', machine: 'tower' };
      }
      if (tool === 'meta.version') {
        return { version: '0.1.0', os: 'linux', arch: 'aarch64' };
      }
      if (tool === 'sys.info') {
        return {
          node: 'pi',
          os: { family: 'linux', arch: 'aarch64' },
          uptime_s: 300,
          cpu: { usage_pct: 8 },
          memory: { total: 8000, used: 1000 },
          disks: [],
          gpus: [],
        };
      }
      if (tool === 'job.list') return { jobs: [] };
      if (tool === 'power.status') return { system: 'linux', busy: { state: 'idle', reasons: [] } };
      throw new Error(`unexpected ${tool} on=${opts.on}`);
    },
  };

  const finalSnap = await refreshFleetProgressive({
    link,
    localNode: 'tower',
    onUpdate() {},
    now: () => 1700000300000,
  });

  const pi = finalSnap.machines.find((m) => m.name === 'pi');
  const tower = finalSnap.machines.find((m) => m.name === 'tower');
  assert.equal(pi.reachable, true);
  assert.equal(pi.thisNode, false, 'answering peer must not become the selected node');
  assert.equal(pi.sys.node, 'pi');
  assert.equal(tower.reachable, false);
  assert.equal(tower.status, 'offline');
  assert.equal(tower.thisNode, true, 'selected node stays marked as selected even while offline');
  // In app mode, detail calls carry explicit `on: machine` so Link routes directly.
  assert.ok(seenOn.some((c) => c.tool === 'sys.info' && c.on === 'pi'));
});

test('desktop local node in fleet produces one row first in list and never emits local', async () => {
  const link = {
    kind: 'app',
    async call(tool, args = {}, opts = {}) {
      if (tool === 'fleet.list') {
        return {
          node: 'studio',
          answered_by: 'local',
          machines: [
            { name: 'alpha', this_node: false, systems: [], wake_via: [] },
            { name: 'studio', this_node: true, systems: [], wake_via: [] },
            { name: 'tower', this_node: false, systems: [], wake_via: [] },
          ],
        };
      }
      if (tool === 'fleet.ping') {
        const target = args.machines?.[0];
        return { results: [{ machine: target, reachable: true, os: 'linux', arch: 'x86_64', version: '0.2.0', latency_ms: 3 }] };
      }
      if (tool === 'meta.version') return { version: '0.2.0', os: 'macos', arch: 'aarch64' };
      if (tool === 'sys.info') return { node: opts.on || 'studio', os: { family: 'macos', arch: 'aarch64' }, uptime_s: 100, cpu: { usage_pct: 5 }, memory: { total: 100, used: 20 }, disks: [], gpus: [] };
      if (tool === 'job.list') return { jobs: [] };
      if (tool === 'power.status') return { system: 'macos', busy: { state: 'idle', reasons: [] } };
      throw new Error(`unexpected ${tool}`);
    },
  };

  const finalSnap = await refreshFleetProgressive({
    link,
    localNode: 'studio',
    isLocalComputer: true,
    onUpdate() {},
    now: () => 1700000400000,
  });

  assert.deepEqual(
    finalSnap.machines.map((m) => m.name),
    ['studio', 'alpha', 'tower'],
    'local computer must appear once and first even before alphabetically earlier peers',
  );
  assert.equal(finalSnap.machines[0].isLocalComputer, true);
  assert.equal(finalSnap.machines[0].thisNode, true);
  assert.equal(finalSnap.machines[1].isLocalComputer, false);
  assert.equal(finalSnap.machines.some((m) => m.name === 'local'), false);
});

test('desktop local node not in any fleet produces one row under its own node name', async () => {
  const link = {
    kind: 'app',
    async call(tool) {
      if (tool === 'fleet.list') {
        return { node: 'studio', answered_by: 'local', machines: [] };
      }
      if (tool === 'meta.version') return { version: '0.2.0', os: 'macos', arch: 'aarch64' };
      if (tool === 'sys.info') return { node: 'studio', os: { family: 'macos', arch: 'aarch64' }, uptime_s: 50, cpu: { usage_pct: 2 }, memory: { total: 100, used: 10 }, disks: [], gpus: [] };
      if (tool === 'job.list') return { jobs: [] };
      if (tool === 'power.status') return { system: 'macos', busy: { state: 'idle', reasons: [] } };
      throw new Error(`unexpected ${tool}`);
    },
  };

  const finalSnap = await refreshFleetProgressive({
    link,
    localNode: 'studio',
    isLocalComputer: true,
    onUpdate() {},
    now: () => 1700000500000,
  });

  assert.equal(finalSnap.machines.length, 1);
  assert.equal(finalSnap.machines[0].name, 'studio');
  assert.equal(finalSnap.machines[0].isLocalComputer, true);
  assert.equal(finalSnap.machines[0].reachable, true);
});

test('pending local node shows one row named This computer until known, then settles to its real node name without duplicating', async () => {
  let nodeReady = false;
  let learnedName = null;
  const resolvedCallbacks = [];

  const link = {
    kind: 'app',
    getLocalMachineName: () => learnedName,
    setLocalMachineName(name) {
      learnedName = name;
    },
    async call(tool, args = {}, opts = {}) {
      if (!nodeReady) {
        throw { code: 'unreachable', message: 'Starting up.', machine: 'This computer' };
      }
      if (tool === 'fleet.list') {
        learnedName = 'studio';
        return {
          node: 'studio',
          answered_by: 'studio',
          machines: [
            { name: 'studio', this_node: true, systems: [], wake_via: [] },
            { name: 'tower', this_node: false, systems: [], wake_via: [] },
          ],
        };
      }
      if (tool === 'fleet.ping') {
        return { results: [{ machine: args.machines[0], reachable: true, os: 'linux', arch: 'x86_64', version: '0.2.0', latency_ms: 5 }] };
      }
      if (tool === 'meta.whoami') return { node: 'studio', principal: 'local', role: 'owner', tools: [] };
      if (tool === 'meta.version') return { version: '0.2.0', os: 'macos', arch: 'aarch64' };
      if (tool === 'sys.info') return { node: opts.on === 'tower' ? 'tower' : 'studio', os: { family: 'macos', arch: 'aarch64' }, uptime_s: 30, cpu: { usage_pct: 4 }, memory: { total: 100, used: 15 }, disks: [], gpus: [] };
      if (tool === 'job.list') return { jobs: [] };
      if (tool === 'power.status') return { system: 'macos', busy: { state: 'idle', reasons: [] } };
      throw new Error(`unexpected ${tool}`);
    },
  };

  // Phase 1: node name cannot be read yet -> one row named "This computer".
  const firstSnap = await refreshFleetProgressive({
    link,
    localNode: 'This computer',
    isLocalComputer: true,
    nodeNamePending: true,
    onLocalNodeResolved: (n) => resolvedCallbacks.push(n),
    onUpdate() {},
    now: () => 1700000600000,
  });

  assert.deepEqual(
    firstSnap.machines.map((m) => m.name),
    ['This computer'],
  );
  assert.equal(firstSnap.machines[0].isLocalComputer, true);
  assert.equal(firstSnap.machines[0].nodeNamePending, true);
  assert.deepEqual(resolvedCallbacks, []);

  // Phase 2: node becomes reachable -> settles in place to 'studio', never showing both rows.
  nodeReady = true;
  const seenSnapshots = [];
  const secondSnap = await refreshFleetProgressive({
    link,
    localNode: 'This computer',
    isLocalComputer: true,
    nodeNamePending: true,
    initialMachines: firstSnap.machines,
    onLocalNodeResolved: (n) => resolvedCallbacks.push(n),
    onUpdate(snap) {
      seenSnapshots.push(snap.machines.map((m) => m.name));
    },
    now: () => 1700000605000,
  });

  for (const names of seenSnapshots) {
    assert.equal(
      names.includes('This computer') && names.includes('studio'),
      false,
      `This computer and studio must never coexist in one snapshot: ${JSON.stringify(names)}`,
    );
    assert.equal(names.includes('local'), false);
  }
  assert.deepEqual(
    secondSnap.machines.map((m) => m.name),
    ['studio', 'tower'],
  );
  assert.equal(secondSnap.machines[0].isLocalComputer, true);
  assert.equal(secondSnap.machines[0].nodeNamePending, false);
  assert.deepEqual(resolvedCallbacks, ['studio']);
});

test('browser HTTP door without local pipe remains unchanged', async () => {
  const link = {
    kind: 'http',
    async call(tool, args = {}) {
      if (tool === 'fleet.list') {
        return {
          node: 'tower',
          machines: [
            { name: 'tower', this_node: true, systems: [], wake_via: [] },
            { name: 'pi', this_node: false, systems: [], wake_via: [] },
          ],
        };
      }
      if (tool === 'fleet.ping') {
        return { results: [{ machine: args.machines[0], reachable: true, os: 'linux', arch: 'aarch64', version: '0.2.0', latency_ms: 4 }] };
      }
      if (tool === 'meta.version') return { version: '0.2.0', os: 'linux', arch: 'x86_64' };
      if (tool === 'sys.info') return { os: { family: 'linux', arch: 'x86_64' }, uptime_s: 20, cpu: { usage_pct: 5 }, memory: { total: 100, used: 10 }, disks: [], gpus: [] };
      if (tool === 'job.list') return { jobs: [] };
      if (tool === 'power.status') return { system: 'linux', busy: { state: 'idle', reasons: [] } };
      throw new Error(`unexpected ${tool}`);
    },
  };

  const snap = await refreshFleetProgressive({
    link,
    localNode: 'tower',
    onUpdate() {},
    now: () => 1700000700000,
  });

  assert.deepEqual(
    snap.machines.map((m) => m.name),
    ['tower', 'pi'],
  );
  assert.equal(snap.machines[0].thisNode, true);
  assert.equal(snap.machines[0].isLocalComputer, false);
  assert.equal(snap.machines[1].isLocalComputer, false);
});
