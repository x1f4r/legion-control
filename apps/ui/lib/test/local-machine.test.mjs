import assert from 'node:assert/strict';
import test from 'node:test';

import { formatMachineStatusPhrase } from '../activity.js';
import { createCapabilitiesStore } from '../capabilities.js';
import { createThumbQueue } from '../files.js';
import { createFolderChangesStore } from '../folder-changes.js';
import {
  migrateLocalFavouritesOnNode,
  migrateLocalMachineStorage,
  normalizeFavouritesMachine,
} from '../local-machine.js';
import { createResumeStore, RESUME_STORAGE_KEY } from '../player.js';
import { createThumbCache, thumbCacheKey } from '../thumb-cache.js';

function memoryStorage(initial = {}) {
  const map = new Map(Object.entries(initial));
  return {
    get length() {
      return map.size;
    },
    key(index) {
      return Array.from(map.keys())[index] ?? null;
    },
    getItem(k) {
      return map.has(k) ? map.get(k) : null;
    },
    setItem(k, v) {
      map.set(k, String(v));
    },
    removeItem(k) {
      map.delete(k);
    },
    keys() {
      return Array.from(map.keys());
    },
  };
}

test('migrateLocalMachineStorage moves local-keyed recents, remembered folders, view modes, fleet cache, identity, thumbs, resume, and session tools to the node name', () => {
  const localStorage = memoryStorage({
    'legion.files.recent.v1': JSON.stringify([
      { machine: 'local', path: '/home/me/notes.txt', name: 'notes.txt', openedAt: 2000 },
      { machine: 'studio', path: '/home/me/notes.txt', name: 'notes.txt', openedAt: 1000 },
      { machine: 'local', path: '/home/me/photo.jpg', name: 'photo.jpg', openedAt: 1500 },
      { machine: 'tower', path: '/home/me/other.txt', name: 'other.txt', openedAt: 500 },
    ]),
    'legion.files.recentDirs.v1': JSON.stringify({
      local: ['/home/me/Downloads', '/home/me/Docs'],
      studio: ['/home/me/Docs', '/home/me/Photos'],
      tower: ['/home/me/Videos'],
    }),
    'legion.files.viewMode.v1': JSON.stringify({
      'local\0/home/me/Pictures': 'grid',
      'local\0/home/me/Docs': 'grid',
      'studio\0/home/me/Docs': 'list',
      'tower\0/home/me/Media': 'grid',
    }),
    'legion.cache.fleet': JSON.stringify({
      savedAt: 5000,
      value: {
        node: 'local',
        savedAt: 5000,
        machines: [
          { name: 'local', thisNode: true, reachable: true, lastSeenAt: 4500, lastCheckedAt: 5000 },
          { name: 'studio', thisNode: false, reachable: true, lastSeenAt: 4000, lastCheckedAt: 4200 },
          { name: 'tower', thisNode: false, reachable: false, lastSeenAt: 1000, lastCheckedAt: 5000 },
        ],
      },
    }),
    'legion.nodeIdentity.local': JSON.stringify({ principal: 'local', role: 'owner', checkedAt: '2026-11-24T00:00:00Z' }),
    'legion.thumb.v1.local\u0000/home/me/photo.jpg\u00001\u0000128': JSON.stringify({ url: 'data:image/png;base64,AAA' }),
    'legion.files.resume.v1': JSON.stringify({
      'local\0/home/me/movie.mp4': 142.5,
      'tower\0/home/me/clip.mp4': 12.0,
    }),
  });
  const sessionStorage = memoryStorage({
    'legion.cache.tools:local': JSON.stringify({ savedAt: 5000, value: [{ name: 'fs.list' }] }),
  });

  const first = migrateLocalMachineStorage('studio', { localStorage, sessionStorage });
  assert.equal(first.migrated, true);

  // Recent files deduplicated, keeping newer openedAt for /home/me/notes.txt.
  const recents = JSON.parse(localStorage.getItem('legion.files.recent.v1'));
  assert.equal(recents.length, 3);
  assert.equal(recents.some((r) => r.machine === 'local'), false);
  assert.equal(recents.find((r) => r.path === '/home/me/notes.txt').openedAt, 2000);
  assert.equal(recents.find((r) => r.path === '/home/me/photo.jpg').machine, 'studio');

  // Remembered destinations merged without duplicates and local removed.
  const recentDirs = JSON.parse(localStorage.getItem('legion.files.recentDirs.v1'));
  assert.equal('local' in recentDirs, false);
  assert.deepEqual(recentDirs.studio, ['/home/me/Docs', '/home/me/Photos', '/home/me/Downloads']);
  assert.deepEqual(recentDirs.tower, ['/home/me/Videos']);

  // View modes migrated without overwriting existing native studio entry.
  const viewModes = JSON.parse(localStorage.getItem('legion.files.viewMode.v1'));
  assert.equal(viewModes['studio\0/home/me/Pictures'], 'grid');
  assert.equal(viewModes['studio\0/home/me/Docs'], 'list');
  assert.equal(viewModes['tower\0/home/me/Media'], 'grid');
  assert.equal(Object.keys(viewModes).some((k) => k.startsWith('local\0')), false);

  // Fleet cache deduplicated studio + local into one local-computer row.
  const fleet = JSON.parse(localStorage.getItem('legion.cache.fleet'));
  assert.equal(fleet.value.node, 'studio');
  assert.deepEqual(
    fleet.value.machines.map((m) => m.name),
    ['studio', 'tower'],
  );
  assert.equal(fleet.value.machines[0].isLocalComputer, true);
  assert.equal(fleet.value.machines[0].lastSeenAt, 4500);

  // Node identity, thumbnail cache, resume position, and session tools migrated cleanly.
  assert.equal(localStorage.getItem('legion.nodeIdentity.local'), null);
  assert.ok(localStorage.getItem('legion.nodeIdentity.studio'));
  assert.equal(localStorage.getItem('legion.thumb.v1.local\u0000/home/me/photo.jpg\u00001\u0000128'), null);
  assert.ok(localStorage.getItem('legion.thumb.v1.studio\u0000/home/me/photo.jpg\u00001\u0000128'));
  const resume = JSON.parse(localStorage.getItem('legion.files.resume.v1'));
  assert.equal(resume['studio\0/home/me/movie.mp4'], 142.5);
  assert.equal('local\0/home/me/movie.mp4' in resume, false);
  assert.equal(sessionStorage.getItem('legion.cache.tools:local'), null);
  assert.ok(sessionStorage.getItem('legion.cache.tools:studio'));

  // Idempotent on second call.
  const second = migrateLocalMachineStorage('studio', { localStorage, sessionStorage });
  assert.equal(second.migrated, false);
});

test('normalizeFavouritesMachine and migrateLocalFavouritesOnNode migrate legacy local favourites and pins quietly', async () => {
  const entries = [
    { id: 'fav-1', machine: 'local', path: '/home/me/Projects', label: 'Projects', pinned: true },
    { id: 'fav-2', machine: 'local', path: '/home/me/Docs', label: 'Docs', pinned: true },
    { id: 'fav-3', machine: 'studio', path: '/home/me/Docs', label: 'Docs', pinned: false },
    { id: 'fav-4', machine: 'tower', path: '/home/me/Videos', label: 'Videos', pinned: false },
  ];

  const normalized = normalizeFavouritesMachine(entries, 'studio');
  assert.equal(normalized.length, 3);
  assert.equal(normalized.some((e) => e.machine === 'local'), false);
  assert.equal(normalized.find((e) => e.path === '/home/me/Docs').pinned, true);
  assert.equal(normalized.find((e) => e.path === '/home/me/Projects').machine, 'studio');

  const ops = [];
  const link = {
    async call(tool, args, opts) {
      ops.push({ tool, args, on: opts?.on });
      if (tool === 'fav.add') return { entry: { id: 'fav-new', ...args } };
      return { ok: true };
    },
  };

  const res = await migrateLocalFavouritesOnNode({
    entries,
    realNodeName: 'studio',
    link,
    localNode: 'studio',
    canWrite: true,
  });
  assert.equal(res.migrated, true);
  assert.ok(ops.some((o) => o.tool === 'fav.add' && o.args.machine === 'studio' && o.args.path === '/home/me/Projects' && o.args.pinned === true));
  assert.ok(ops.some((o) => o.tool === 'fav.remove' && o.args.id === 'fav-1'));
  assert.ok(ops.some((o) => o.tool === 'fav.set' && o.args.id === 'fav-3' && o.args.pinned === true));
  assert.ok(ops.some((o) => o.tool === 'fav.remove' && o.args.id === 'fav-2'));
});

test('capabilities store and folder changes store move local/pending entries onto the resolved node name', () => {
  const caps = createCapabilitiesStore({ kind: 'app' });
  caps.seed('local', {
    tools: [{ name: 'fs.list', effect: 'read', input_schema: { properties: { path: {} } } }],
    version: '0.2.0',
  });
  assert.equal(caps.isKnown('local'), true);
  assert.equal(caps.renameMachine('local', 'studio'), true);
  assert.equal(caps.isKnown('local'), false);
  assert.equal(caps.isKnown('studio'), true);
  assert.equal(caps.hasTool('studio', 'fs.list'), true);

  const changes = createFolderChangesStore();
  changes.registerHome('This computer', '/home/me');
  changes.setCached('This computer', '/home/me', { rawEntries: [{ name: 'a.txt' }] });
  assert.equal(changes.renameMachine('This computer', 'studio'), true);
  assert.equal(changes.getCached('This computer', '/home/me'), null);
  assert.deepEqual(changes.getCached('studio', '~'), { rawEntries: [{ name: 'a.txt' }] });
});

test('formatMachineStatusPhrase prefixes This computer for desktop local machine without repeating when pending', () => {
  assert.equal(
    formatMachineStatusPhrase({ name: 'studio', isLocalComputer: true, reachable: true, busy: false, runningJobs: 0 }),
    'This computer · Idle',
  );
  assert.equal(
    formatMachineStatusPhrase({ name: 'studio', isLocalComputer: true, reachable: true, busy: true, runningJobs: 2 }),
    'This computer · Busy · 2 things running',
  );
  assert.equal(
    formatMachineStatusPhrase({ name: 'This computer', isLocalComputer: true, reachable: false, status: 'asking', asking: true }),
    'Checking…',
  );
  assert.equal(
    formatMachineStatusPhrase({ name: 'tower', isLocalComputer: false, reachable: true, busy: false, runningJobs: 0 }),
    'Idle',
  );
});

test('createThumbQueue finds migrated local and pending thumbnails under the resolved node name without calling fs.thumb', () => {
  // 1. Persisted legacy "local" thumbnail migrated in Web Storage is hit directly by createThumbQueue.
  const storage = memoryStorage({
    [`legion.thumb.v1.${thumbCacheKey('local', '/home/me/photo.jpg', '2026-01-01T00:00:00Z', 128)}`]: JSON.stringify({
      url: 'data:image/jpeg;base64,TE9DQUw=',
    }),
  });
  migrateLocalMachineStorage('studio', { localStorage: storage });

  const storageCache = createThumbCache({ maxEntries: 32, storage });
  let rpcCalls = 0;
  const timers = [];
  const q1 = createThumbQueue({
    machine: 'studio',
    link: {
      async call() {
        rpcCalls += 1;
        return { results: [] };
      },
    },
    thumbCache: storageCache,
    schedule: (fn) => {
      timers.push(fn);
      return timers.length;
    },
    cancel: () => {},
  });
  q1.enqueue({ path: '/home/me/photo.jpg', mtime: '2026-01-01T00:00:00Z' });
  assert.equal(timers.length, 0, 'migrated storage thumbnail must be a cache hit without scheduling fs.thumb');
  assert.equal(rpcCalls, 0);
  assert.equal(
    storageCache.get(thumbCacheKey('studio', '/home/me/photo.jpg', '2026-01-01T00:00:00Z', 128))?.url,
    'data:image/jpeg;base64,TE9DQUw=',
  );
  q1.dispose();

  // 2. In-memory thumbnails cached under "This computer" before the node resolved move onto the node name.
  const memCache = createThumbCache({ maxEntries: 32, storage: null });
  memCache.set(thumbCacheKey('This computer', '/home/me/clip.mp4', 'm2', 128), {
    url: 'data:image/png;base64,TUVN',
  });
  assert.equal(memCache.renameMachine('This computer', 'studio'), true);
  assert.equal(memCache.get(thumbCacheKey('This computer', '/home/me/clip.mp4', 'm2', 128)), null);

  const timers2 = [];
  const q2 = createThumbQueue({
    machine: 'studio',
    link: {
      async call() {
        rpcCalls += 1;
        return { results: [] };
      },
    },
    thumbCache: memCache,
    schedule: (fn) => {
      timers2.push(fn);
      return timers2.length;
    },
    cancel: () => {},
  });
  q2.enqueue({ path: '/home/me/clip.mp4', mtime: 'm2' });
  assert.equal(timers2.length, 0, 'renamed in-memory thumbnail must be a cache hit without scheduling fs.thumb');
  assert.equal(rpcCalls, 0);
  assert.equal(
    memCache.get(thumbCacheKey('studio', '/home/me/clip.mp4', 'm2', 128))?.url,
    'data:image/png;base64,TUVN',
  );
  q2.dispose();
});

test('migrateLocalMachineStorage migrates player resume store positions from local and This computer to the node name', () => {
  // Minimal storage shim (getItem/setItem/removeItem only) verifies explicit RESUME_STORAGE_KEY migration.
  const map = new Map();
  const storage = {
    getItem(k) {
      return map.has(k) ? map.get(k) : null;
    },
    setItem(k, v) {
      map.set(k, String(v));
    },
    removeItem(k) {
      map.delete(k);
    },
  };

  const store = createResumeStore(storage);
  const localFilm = { machine: 'local', path: '/home/me/Videos/Film.mp4', size: 120_000, mtime: '2026-11-20T10:00:00Z' };
  const pendingAlbum = {
    machine: 'This computer',
    path: '/home/me/Music/LiveSet.flac',
    size: 80_000,
    mtime: '2026-11-21T12:00:00Z',
  };
  const bothLocal = { machine: 'local', path: '/home/me/Videos/Both.mp4', size: 50_000, mtime: '2026-11-22T08:00:00Z' };
  const bothNative = { ...bothLocal, machine: 'studio' };
  const remoteFilm = { machine: 'tower', path: '/home/me/Videos/Film.mp4', size: 120_000, mtime: '2026-11-20T10:00:00Z' };

  assert.equal(store.save(localFilm, 412, 1800, 100), true);
  assert.equal(store.save(pendingAlbum, 725, 2400, 200), true);
  assert.equal(store.save(bothLocal, 95, 1800, 300), true);
  assert.equal(store.save(bothNative, 840, 1800, 400), true);
  assert.equal(store.save(remoteFilm, 310, 1800, 500), true);

  const first = migrateLocalMachineStorage('studio', { localStorage: storage });
  assert.equal(first.migrated, true);
  assert.ok(first.keys.includes(RESUME_STORAGE_KEY));

  // Fresh store instance finds every migrated position under 'studio' and leaves 'local' / 'This computer' gone.
  const after = createResumeStore(storage);
  assert.equal(after.load(localFilm, 1800), null);
  assert.equal(after.load({ ...localFilm, machine: 'studio' }, 1800), 412);
  assert.equal(after.load(pendingAlbum, 2400), null);
  assert.equal(after.load({ ...pendingAlbum, machine: 'studio' }, 2400), 725);
  assert.equal(after.load(bothNative, 1800), 840);
  assert.equal(after.load(bothLocal, 1800), null);
  assert.equal(after.load(remoteFilm, 1800), 310);

  // Idempotent on second run.
  const second = migrateLocalMachineStorage('studio', { localStorage: storage });
  assert.equal(second.migrated, false);
});
