import test from 'node:test';
import assert from 'node:assert/strict';

import {
  createCoalescedRefetch,
  createFolderChangesStore,
  DEFAULT_THROTTLE_MS,
  normalizeFolderPath,
  parseTransferTarget,
  STALE_AFTER_MS,
} from '../folder-changes.js';

function createFakeClock(startMs = 1_000_000) {
  let currentMs = startMs;
  let nextId = 1;
  const timers = new Map();

  return {
    now: () => currentMs,
    setTimer(fn, ms) {
      const id = nextId++;
      timers.set(id, { at: currentMs + Math.max(0, ms), fn });
      return id;
    },
    clearTimer(id) {
      timers.delete(id);
    },
    advance(ms) {
      const target = currentMs + ms;
      while (true) {
        let soonestId = null;
        let soonestAt = Infinity;
        for (const [id, entry] of timers.entries()) {
          if (entry.at <= target && entry.at < soonestAt) {
            soonestAt = entry.at;
            soonestId = id;
          }
        }
        if (soonestId === null) break;
        const entry = timers.get(soonestId);
        timers.delete(soonestId);
        currentMs = entry.at;
        entry.fn();
      }
      currentMs = target;
    },
    pendingCount: () => timers.size,
  };
}

test('normalizeFolderPath preserves Unix, tilde, and Windows drive roots', () => {
  assert.equal(normalizeFolderPath('~'), '~');
  assert.equal(normalizeFolderPath('/'), '/');
  assert.equal(normalizeFolderPath('/home/me/Photos///'), '/home/me/Photos');
  assert.equal(normalizeFolderPath('C:\\'), 'C:\\');
  assert.equal(normalizeFolderPath('C:\\Users\\me\\Docs\\'), 'C:\\Users\\me\\Docs');
});

test('parseTransferTarget handles remote POSIX, remote Windows, and local paths', () => {
  assert.deepEqual(parseTransferTarget('nas:/home/me/Photos/a.jpg', 'tower'), {
    machine: 'nas',
    path: '/home/me/Photos/a.jpg',
    dir: '/home/me/Photos',
  });
  assert.deepEqual(parseTransferTarget('winbox:C:\\Users\\me\\report.pdf', 'tower'), {
    machine: 'winbox',
    path: 'C:\\Users\\me\\report.pdf',
    dir: 'C:\\Users\\me',
  });
  assert.deepEqual(parseTransferTarget('C:\\Users\\me\\report.pdf', 'winbox'), {
    machine: 'winbox',
    path: 'C:\\Users\\me\\report.pdf',
    dir: 'C:\\Users\\me',
  });
  assert.deepEqual(parseTransferTarget('/home/me/note.txt', 'tower'), {
    machine: 'tower',
    path: '/home/me/note.txt',
    dir: '/home/me',
  });
});

test('subscribe receives announcements for matching machine/path and resolves home aliases', () => {
  const clock = createFakeClock();
  const store = createFolderChangesStore({ now: clock.now, setTimer: clock.setTimer, clearTimer: clock.clearTimer });

  const hits = [];
  const otherHits = [];
  const unsub1 = store.subscribe('tower', '~', (e) => hits.push(e));
  const unsub2 = store.subscribe('nas', '/home/me', (e) => otherHits.push(e));

  store.registerHome('tower', '/home/me');
  store.changed('tower', '/home/me', { reason: 'upload' });

  assert.equal(hits.length, 1);
  assert.equal(hits[0].machine, 'tower');
  assert.equal(hits[0].path, '/home/me');
  assert.equal(hits[0].isParent, false);
  assert.equal(otherHits.length, 0);

  // Nested ~/Photos matches /home/me/Photos once home is registered.
  const nestedHits = [];
  const unsub3 = store.subscribe('tower', '~/Photos', (e) => nestedHits.push(e));
  store.changed('tower', '/home/me/Photos/', { reason: 'mkdir' });
  assert.equal(nestedHits.length, 1);
  assert.equal(nestedHits[0].isParent, false);
  // Parent (/home/me) was also notified with isParent: true.
  assert.equal(hits.length, 2);
  assert.equal(hits[1].isParent, true);
  assert.equal(hits[1].childPath, '/home/me/Photos');

  unsub1();
  unsub2();
  unsub3();
  store.dispose();
});

test('throttled multi-file announcements emit at most twice per second and immediately on completion', () => {
  const clock = createFakeClock();
  const store = createFolderChangesStore({
    throttleMs: DEFAULT_THROTTLE_MS,
    now: clock.now,
    setTimer: clock.setTimer,
    clearTimer: clock.clearTimer,
  });

  const events = [];
  store.subscribe('tower', '/home/me/Imports', (e) => events.push({ t: clock.now(), reason: e.reason }));

  // Simulate 200 files copying over 2 seconds (every 10ms).
  for (let i = 0; i < 200; i += 1) {
    store.changed('tower', '/home/me/Imports', { throttle: true, reason: 'transfer-progress' });
    clock.advance(10);
  }

  // Over 2000ms with 500ms throttle, there must be at most 5 emissions (at 0, 500, 1000, 1500, 2000).
  assert.ok(events.length >= 4 && events.length <= 5, `expected 4-5 throttled events, got ${events.length}`);
  for (let i = 1; i < events.length; i += 1) {
    assert.ok(
      events[i].t - events[i - 1].t >= DEFAULT_THROTTLE_MS,
      `emissions must be >= ${DEFAULT_THROTTLE_MS}ms apart`,
    );
  }

  // Final unthrottled completion fires immediately and cancels any pending timer.
  const countBeforeEnd = events.length;
  store.changed('tower', '/home/me/Imports', { throttle: false, reason: 'transfer' });
  assert.equal(events.length, countBeforeEnd + 1);
  assert.equal(events[events.length - 1].reason, 'transfer');
  assert.equal(clock.pendingCount(), 0);
  store.dispose();
});

test('off-screen staleness marks folder and parent stale on change and after 10 seconds', () => {
  const clock = createFakeClock();
  const store = createFolderChangesStore({
    staleAfterMs: STALE_AFTER_MS,
    now: clock.now,
    setTimer: clock.setTimer,
    clearTimer: clock.clearTimer,
  });

  const t0 = clock.now();
  store.markLoaded('tower', '/home/me', t0);
  store.markLoaded('tower', '/home/me/Docs', t0);
  assert.equal(store.isStale('tower', '/home/me', { loadedAt: t0 }), false);
  assert.equal(store.isStale('tower', '/home/me/Docs', { loadedAt: t0 }), false);

  // Change in child folder marks both child and parent stale.
  clock.advance(1000);
  store.changed('tower', '/home/me/Docs', { reason: 'delete' });
  assert.equal(store.isStale('tower', '/home/me/Docs', { loadedAt: t0 }), true);
  assert.equal(store.isStale('tower', '/home/me', { loadedAt: t0 }), true);

  // Reloading clears staleness until >10 seconds pass.
  const t1 = clock.now();
  store.markLoaded('tower', '/home/me/Docs', t1);
  clock.advance(STALE_AFTER_MS);
  assert.equal(store.isStale('tower', '/home/me/Docs', { loadedAt: t1 }), false);
  clock.advance(1);
  assert.equal(store.isStale('tower', '/home/me/Docs', { loadedAt: t1 }), true);
  store.dispose();
});

test('observeTransferJobs announces destination progress and final destination + move source', () => {
  const clock = createFakeClock();
  const store = createFolderChangesStore({ now: clock.now, setTimer: clock.setTimer, clearTimer: clock.clearTimer });
  const seen = new Map();

  const destEvents = [];
  const srcEvents = [];
  store.subscribe('nas', '/home/me/Archive', (e) => destEvents.push(e));
  store.subscribe('tower', '/home/me/Inbox', (e) => srcEvents.push(e));

  // Initial observation while running (0 files done).
  store.observeTransferJobs(
    [
      {
        id: 'j-1',
        kind: 'transfer',
        machine: 'tower',
        state: 'running',
        move: true,
        from: '/home/me/Inbox/video.mp4',
        to: 'nas:/home/me/Archive/video.mp4',
        progress: { files_done: 0, files_total: 2 },
      },
    ],
    seen,
  );
  assert.equal(destEvents.length, 0);
  assert.equal(srcEvents.length, 0);

  // Mid-transfer progress announces throttled.
  clock.advance(100);
  store.observeTransferJobs(
    [
      {
        id: 'j-1',
        kind: 'transfer',
        machine: 'tower',
        state: 'running',
        move: true,
        from: '/home/me/Inbox/video.mp4',
        to: 'nas:/home/me/Archive/video.mp4',
        progress: { files_done: 1, files_total: 2 },
      },
    ],
    seen,
  );
  assert.equal(destEvents.length, 1);
  assert.equal(srcEvents.length, 1);

  // Completion announces unthrottled on both destination and move source.
  clock.advance(100);
  store.observeTransferJobs(
    [
      {
        id: 'j-1',
        kind: 'transfer',
        machine: 'tower',
        state: 'succeeded',
        move: true,
        from: '/home/me/Inbox/video.mp4',
        to: 'nas:/home/me/Archive/video.mp4',
        progress: { files_done: 2, files_total: 2 },
      },
    ],
    seen,
  );
  assert.equal(destEvents.length, 2);
  assert.equal(srcEvents.length, 2);
  assert.equal(destEvents[1].reason, 'transfer');
  assert.equal(srcEvents[1].reason, 'transfer');
  store.dispose();
});

test('observeDeviceTransfers and observeTransferJobs announce nothing on cancelling/cancelled and announce on done', () => {
  const clock = createFakeClock();
  const store = createFolderChangesStore({
    now: clock.now,
    setTimer: clock.setTimer,
    clearTimer: clock.clearTimer,
  });
  const events = [];
  store.subscribe('tower', '/home/me/Documents', (ev) => events.push(ev));

  // Device upload: queued -> running -> cancelling -> cancelled announces nothing.
  let prev = new Map();
  const stepTransfers = (list) => {
    store.observeDeviceTransfers(list, prev);
    prev = new Map(list.map((t) => [t.id, t.state]));
  };
  const baseUp = {
    id: 'up-cancel',
    direction: 'upload',
    machine: 'tower',
    path: '/home/me/Documents/partial.bin',
  };
  stepTransfers([{ ...baseUp, state: 'queued' }]);
  stepTransfers([{ ...baseUp, state: 'running' }]);
  stepTransfers([{ ...baseUp, state: 'cancelling' }]);
  stepTransfers([{ ...baseUp, state: 'cancelled' }]);
  assert.equal(events.length, 0, 'Cancelling and cancelled uploads must announce nothing');

  // Background transfer job: running -> cancelling -> cancelled announces nothing.
  const seenJobs = new Map();
  const baseJob = {
    id: 'j-cancel',
    kind: 'transfer',
    machine: 'tower',
    from: '/home/me/Inbox/clip.mp4',
    to: 'tower:/home/me/Documents/clip.mp4',
    progress: { files_done: 0, files_total: 1 },
  };
  store.observeTransferJobs([{ ...baseJob, state: 'running' }], seenJobs);
  store.observeTransferJobs([{ ...baseJob, state: 'cancelling' }], seenJobs);
  store.observeTransferJobs([{ ...baseJob, state: 'cancelled' }], seenJobs);
  assert.equal(events.length, 0, 'Cancelling and cancelled jobs must announce nothing');

  // A finished upload DOES announce (unthrottled even when a sibling upload is cancelling).
  const okUp = {
    id: 'up-ok',
    direction: 'upload',
    machine: 'tower',
    path: '/home/me/Documents/finished.txt',
  };
  stepTransfers([
    { ...baseUp, state: 'cancelling' },
    { ...okUp, state: 'running' },
  ]);
  assert.equal(events.length, 0);
  stepTransfers([
    { ...baseUp, state: 'cancelling' },
    { ...okUp, state: 'done' },
  ]);
  assert.equal(events.length, 1, 'Finished upload must announce changed folder immediately');
  assert.equal(events[0].reason, 'upload');
  store.dispose();
});

test('createCoalescedRefetch never overlaps and coalesces multiple bursts into one follow-up', async () => {
  let active = 0;
  let maxConcurrent = 0;
  let runs = 0;
  const resolvers = [];

  const runner = createCoalescedRefetch(async () => {
    runs += 1;
    active += 1;
    if (active > maxConcurrent) maxConcurrent = active;
    await new Promise((resolve) => resolvers.push(resolve));
    active -= 1;
    return runs;
  });

  const p1 = runner.trigger();
  assert.equal(runs, 1);
  assert.equal(runner.isRunning(), true);

  // Fire 5 more triggers while run #1 is in flight.
  const followUps = [runner.trigger(), runner.trigger(), runner.trigger(), runner.trigger(), runner.trigger()];
  assert.equal(runs, 1);

  // Finish run #1 -> exactly one follow-up (run #2) should begin.
  resolvers.shift()();
  await p1;
  assert.equal(runs, 2);
  assert.equal(maxConcurrent, 1);

  // Finish run #2 -> all 5 coalesced waiters resolve and no run #3 starts.
  resolvers.shift()();
  await Promise.all(followUps);
  assert.equal(runs, 2);
  assert.equal(maxConcurrent, 1);
  assert.equal(runner.isRunning(), false);
});
