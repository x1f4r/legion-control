import assert from 'node:assert/strict';
import test from 'node:test';

import { canBrowserImmersive, createBrowserImmersive } from '../immersive-browser.js';
import { createAppLink, detectAppImmersiveCapability } from '../../link/app.js';
import {
  buildMediaQueue,
  clampVolume,
  classifyPlayerKey,
  computeResumePosition,
  createFullscreenController,
  createResumeStore,
  createVideoController,
  DOUBLE_TAP_WINDOW_MS,
  extractTagsMap,
  extractThumbMap,
  formatMusicDisplay,
  formatPlayerTime,
  formatPositionValueText,
  formatRemainingTime,
  formatSpeed,
  isNearEnd,
  isResumeEligible,
  OVERLAY_HIDE_DELAY_MS,
  PLAYBACK_SPEEDS,
  preferredVideoOrientation,
  queueNeighbors,
  RESUME_CHIP_DURATION_MS,
  RESUME_MAX_ENTRIES,
  RESUME_MIN_DURATION_S,
  shouldMusicPrevRestart,
  STALL_SPINNER_DELAY_MS,
  stripFileExtension,
  tapZoneForPosition,
} from '../player.js';

function createFakeClock(startMs = 1_000_000) {
  let now = startMs;
  let nextId = 1;
  const timers = new Map();

  return {
    now: () => now,
    setTimeout(fn, ms) {
      const id = nextId++;
      timers.set(id, { at: now + Math.max(0, ms), fn });
      return id;
    },
    clearTimeout(id) {
      timers.delete(id);
    },
    advance(ms) {
      const target = now + ms;
      while (true) {
        let soonestId = null;
        let soonest = null;
        for (const [id, item] of timers.entries()) {
          if (item.at <= target && (!soonest || item.at < soonest.at)) {
            soonest = item;
            soonestId = id;
          }
        }
        if (!soonest) break;
        timers.delete(soonestId);
        now = soonest.at;
        soonest.fn();
      }
      now = target;
    },
    pendingCount: () => timers.size,
  };
}

function createMemoryStorage() {
  const map = new Map();
  return {
    getItem: (k) => (map.has(k) ? map.get(k) : null),
    setItem: (k, v) => map.set(k, String(v)),
    removeItem: (k) => map.delete(k),
  };
}

// ── Controlled-clock video overlay timing rules ──────────────────────────────────────────────

test('video control layer hides 3 s after last activity while playing and hides desktop cursor with it', () => {
  const clock = createFakeClock();
  const states = [];
  const ctrl = createVideoController({ clock, onChange: (s) => states.push(s) });

  ctrl.setLoading(false);
  ctrl.setPlaying(true);
  assert.equal(ctrl.getState().visible, true);
  assert.equal(ctrl.getState().cursorHidden, false);

  clock.advance(OVERLAY_HIDE_DELAY_MS - 1);
  assert.equal(ctrl.getState().visible, true);
  assert.equal(ctrl.getState().cursorHidden, false);

  clock.advance(1);
  assert.equal(ctrl.getState().visible, false);
  assert.equal(ctrl.getState().cursorHidden, true);

  // Pointer movement / key activity shows layer and resets the 3 s timer.
  ctrl.notifyActivity({ show: true });
  assert.equal(ctrl.getState().visible, true);
  assert.equal(ctrl.getState().cursorHidden, false);

  clock.advance(2000);
  ctrl.notifyActivity({ show: true });
  clock.advance(2000);
  assert.equal(ctrl.getState().visible, true);

  clock.advance(1000);
  assert.equal(ctrl.getState().visible, false);
  assert.equal(ctrl.getState().cursorHidden, true);

  ctrl.dispose();
});

test('video control layer stays visible while paused, loading, ended, on error, or with keyboard focus inside', () => {
  const clock = createFakeClock();
  const ctrl = createVideoController({ clock });

  // While loading
  ctrl.setPlaying(true);
  clock.advance(10_000);
  assert.equal(ctrl.getState().visible, true);

  // Loaded & playing -> hides after 3 s
  ctrl.setLoading(false);
  clock.advance(OVERLAY_HIDE_DELAY_MS);
  assert.equal(ctrl.getState().visible, false);

  // Paused -> shows immediately and never auto-hides
  ctrl.setPlaying(false);
  assert.equal(ctrl.getState().visible, true);
  assert.equal(ctrl.getState().cursorHidden, false);
  clock.advance(10_000);
  assert.equal(ctrl.getState().visible, true);

  // Playing with keyboard focus inside -> never auto-hides until focus leaves
  ctrl.setPlaying(true);
  ctrl.setFocusInside(true);
  clock.advance(10_000);
  assert.equal(ctrl.getState().visible, true);

  ctrl.setFocusInside(false);
  clock.advance(OVERLAY_HIDE_DELAY_MS);
  assert.equal(ctrl.getState().visible, false);

  // Ended -> shows immediately and stays visible (no auto-next for video)
  ctrl.setEnded(true);
  assert.equal(ctrl.getState().visible, true);
  assert.equal(ctrl.getState().playing, false);
  clock.advance(10_000);
  assert.equal(ctrl.getState().visible, true);

  // Error -> stays visible
  ctrl.setEnded(false);
  ctrl.setPlaying(true);
  ctrl.setError(true);
  assert.equal(ctrl.getState().visible, true);
  clock.advance(10_000);
  assert.equal(ctrl.getState().visible, true);

  ctrl.dispose();
});

test('single tap toggles layer at once; double tap within 300 ms on left/right third seeks 10 s and restores layer', () => {
  const clock = createFakeClock();
  const seeks = [];
  const ctrl = createVideoController({
    clock,
    onSeekDelta: (delta) => seeks.push(delta),
  });

  ctrl.setLoading(false);
  ctrl.setPlaying(true);
  assert.equal(ctrl.getState().visible, true);

  // Single tap on center toggles hidden immediately.
  const t1 = ctrl.handlePictureTap('center');
  assert.equal(t1.action, 'toggle');
  assert.equal(ctrl.getState().visible, false);

  // Wait beyond 300 ms -> single tap on right shows layer immediately.
  clock.advance(DOUBLE_TAP_WINDOW_MS + 50);
  const t2 = ctrl.handlePictureTap('right');
  assert.equal(t2.action, 'toggle');
  assert.equal(ctrl.getState().visible, true);

  // Now layer is visible; tap right third -> toggles hidden; 2nd tap within 300 ms seeks +10s AND restores visible!
  clock.advance(DOUBLE_TAP_WINDOW_MS + 50);
  assert.equal(ctrl.getState().visible, true);
  ctrl.handlePictureTap('right');
  assert.equal(ctrl.getState().visible, false);
  clock.advance(180);
  const dblRight = ctrl.handlePictureTap('right');
  assert.deepEqual(dblRight, { action: 'seek', delta: 10 });
  assert.equal(ctrl.getState().visible, true);
  assert.deepEqual(seeks, [10]);

  // Hide layer, then double-tap left third -> first tap shows, second tap seeks -10s AND puts layer back hidden!
  clock.advance(OVERLAY_HIDE_DELAY_MS);
  assert.equal(ctrl.getState().visible, false);
  ctrl.handlePictureTap('left');
  assert.equal(ctrl.getState().visible, true);
  clock.advance(200);
  const dblLeft = ctrl.handlePictureTap('left');
  assert.deepEqual(dblLeft, { action: 'seek', delta: -10 });
  assert.equal(ctrl.getState().visible, false);
  assert.deepEqual(seeks, [10, -10]);

  // Double tap on center does NOT seek (only left/right thirds seek).
  clock.advance(500);
  ctrl.handlePictureTap('center');
  assert.equal(ctrl.getState().visible, true);
  clock.advance(150);
  const centerSecond = ctrl.handlePictureTap('center');
  assert.equal(centerSecond.action, 'toggle');
  assert.equal(ctrl.getState().visible, false);
  assert.deepEqual(seeks, [10, -10]);

  // Check third-of-width zone classification.
  assert.equal(tapZoneForPosition(50, 0, 300), 'left');
  assert.equal(tapZoneForPosition(150, 0, 300), 'center');
  assert.equal(tapZoneForPosition(250, 0, 300), 'right');

  ctrl.dispose();
});

test('stall spinner shows only after > 400 ms, pending seek holds target time, and re-seek or pause cancels pending seek', () => {
  const clock = createFakeClock();
  const committed = [];
  const ctrl = createVideoController({
    clock,
    onCommitSeek: (target, seq) => committed.push({ target, seq }),
  });

  ctrl.setLoading(false);
  ctrl.setPlaying(true);

  // Brief buffer wait (< 400 ms) does NOT flash spinner.
  ctrl.onWaiting();
  clock.advance(STALL_SPINNER_DELAY_MS - 1);
  assert.equal(ctrl.getState().stalled, false);
  ctrl.onCanPlay();
  clock.advance(500);
  assert.equal(ctrl.getState().stalled, false);

  // Slow seek (e.g. Matroska/WebM index at end): target stays at released time, spinner after 400 ms.
  const seq1 = ctrl.beginSeek(145);
  assert.equal(ctrl.getState().pendingSeekTime, 145);
  assert.equal(ctrl.getState().stalled, false);
  clock.advance(STALL_SPINNER_DELAY_MS);
  assert.equal(ctrl.getState().stalled, true);
  assert.equal(ctrl.getState().pendingSeekTime, 145);
  assert.equal(ctrl.getState().visible, true);

  // Another seek replaces the pending seek; stale seeked from seq1 is ignored.
  const seq2 = ctrl.beginSeek(210);
  assert.equal(ctrl.getState().pendingSeekTime, 210);
  assert.equal(ctrl.getState().stalled, false);
  ctrl.onSeeked(seq1); // stale!
  assert.equal(ctrl.getState().pendingSeekTime, 210);

  // Matching seeked clears pendingSeekTime and spinner.
  ctrl.onSeeked(seq2);
  assert.equal(ctrl.getState().pendingSeekTime, null);
  assert.equal(ctrl.getState().stalled, false);

  // Pause cancels an active slow seek immediately.
  ctrl.beginSeek(320);
  clock.advance(STALL_SPINNER_DELAY_MS);
  assert.equal(ctrl.getState().stalled, true);
  assert.equal(ctrl.getState().pendingSeekTime, 320);
  ctrl.setPlaying(false);
  assert.equal(ctrl.getState().pendingSeekTime, null);
  assert.equal(ctrl.getState().stalled, false);
  assert.equal(ctrl.getState().visible, true);

  assert.deepEqual(
    committed.map((c) => c.target),
    [145, 210, 320],
  );
  ctrl.dispose();
});

test('resume chip shows for exactly 5 seconds and clears immediately on dismiss', () => {
  const clock = createFakeClock();
  const ctrl = createVideoController({ clock });

  ctrl.showResumeChip(760);
  assert.deepEqual(ctrl.getState().resumeChip, { position: 760 });
  clock.advance(RESUME_CHIP_DURATION_MS - 1);
  assert.deepEqual(ctrl.getState().resumeChip, { position: 760 });
  clock.advance(1);
  assert.equal(ctrl.getState().resumeChip, null);

  ctrl.showResumeChip(612);
  ctrl.dismissResumeChip();
  assert.equal(ctrl.getState().resumeChip, null);
  ctrl.dispose();
});

// ── Resume store (10 min threshold, last 5% skip, 200-entry LRU cap, tuple key) ──────────────

test('resume store honors 10 min threshold, skips last 5%, keys by machine/path/size/mtime, and caps at 200', () => {
  const storage = createMemoryStorage();
  const store = createResumeStore(storage);

  assert.equal(isResumeEligible(RESUME_MIN_DURATION_S - 1), false);
  assert.equal(isResumeEligible(RESUME_MIN_DURATION_S), true);
  assert.equal(isNearEnd(949, 1000), false);
  assert.equal(isNearEnd(950, 1000), true);
  assert.equal(computeResumePosition(960, 1000), null);
  assert.equal(computeResumePosition(760, 1000), 760);

  const idA = { machine: 'tower', path: '/home/me/Videos/Film.mp4', size: 1_000_000, mtime: '2026-11-20T10:00:00Z' };
  // Short file (< 10 min) is not saved.
  assert.equal(store.save(idA, 120, 300, 100), false);
  assert.equal(store.load(idA, 300), null);

  // >= 10 min file saves and resumes.
  assert.equal(store.save(idA, 760, 1200, 200), true);
  assert.equal(store.load(idA, 1200), 760);

  // Different mtime or size does not match stale saved position.
  assert.equal(store.load({ ...idA, mtime: '2026-11-21T10:00:00Z' }, 1200), null);
  assert.equal(store.load({ ...idA, size: 2_000_000 }, 1200), null);
  assert.equal(store.load({ ...idA, machine: 'pi' }, 1200), null);

  // Saving in last 5% clears the saved position.
  assert.equal(store.save(idA, 1150, 1200, 300), false);
  assert.equal(store.load(idA, 1200), null);

  // LRU cap of 200 files evicts oldest.
  for (let i = 1; i <= RESUME_MAX_ENTRIES + 15; i += 1) {
    store.save(
      { machine: 'tower', path: `/home/me/Videos/v-${i}.mp4`, size: 1000 + i, mtime: '2026-11-20T00:00:00Z' },
      120,
      900,
      1000 + i,
    );
  }
  assert.equal(store.count(), RESUME_MAX_ENTRIES);
  assert.equal(
    store.load({ machine: 'tower', path: '/home/me/Videos/v-1.mp4', size: 1001, mtime: '2026-11-20T00:00:00Z' }, 900),
    null,
  );
  assert.equal(
    store.load({ machine: 'tower', path: `/home/me/Videos/v-${RESUME_MAX_ENTRIES + 15}.mp4`, size: 1000 + RESUME_MAX_ENTRIES + 15, mtime: '2026-11-20T00:00:00Z' }, 900),
    120,
  );
});

// ── Queue, Music metadata, formatting, wire extractors, and keys ─────────────────────────────

test('folder queue keeps same kind in folder sort order and music prev restarts only past 3 s', () => {
  const folder = [
    { name: 'Sub', path: '/a/Sub', kind: 'dir' },
    { name: 'clip-b.mp4', path: '/a/clip-b.mp4', kind: 'file' },
    { name: 'photo.jpg', path: '/a/photo.jpg', kind: 'file' },
    { name: 'song-2.mp3', path: '/a/song-2.mp3', kind: 'file' },
    { name: 'clip-a.mp4', path: '/a/clip-a.mp4', kind: 'file' },
    { name: 'song-1.flac', path: '/a/song-1.flac', kind: 'file' },
  ];

  const videos = buildMediaQueue(folder, 'video');
  assert.deepEqual(
    videos.map((v) => v.name),
    ['clip-b.mp4', 'clip-a.mp4'],
  );

  const firstVid = queueNeighbors(folder, '/a/clip-b.mp4', 'video');
  assert.equal(firstVid.prev, null);
  assert.equal(firstVid.next?.name, 'clip-a.mp4');
  assert.equal(firstVid.hasOther, true);

  const secondVid = queueNeighbors(folder, '/a/clip-a.mp4', 'video');
  assert.equal(secondVid.prev?.name, 'clip-b.mp4');
  assert.equal(secondVid.next, null);

  const soloPic = queueNeighbors(folder, '/a/photo.jpg', 'image');
  assert.equal(soloPic.hasOther, false);
  assert.equal(soloPic.prev, null);
  assert.equal(soloPic.next, null);

  assert.equal(shouldMusicPrevRestart(0), false);
  assert.equal(shouldMusicPrevRestart(3), false);
  assert.equal(shouldMusicPrevRestart(3.1), true);
});

test('music display uses tags, omits missing artist/album parts, and never outputs Unknown artist', () => {
  assert.deepEqual(
    formatMusicDisplay({ title: 'Morning Light', artist: 'Klara', album: 'Early Hours' }, '01.flac'),
    { title: 'Morning Light', subtitle: 'Klara · Early Hours' },
  );
  assert.deepEqual(
    formatMusicDisplay({ title: 'Sketch', artist: 'Klara' }, '03.ogg'),
    { title: 'Sketch', subtitle: 'Klara' },
  );
  assert.deepEqual(
    formatMusicDisplay({ album: 'Live Tape' }, 'track_04.mp3'),
    { title: 'track_04', subtitle: 'Live Tape' },
  );
  assert.deepEqual(
    formatMusicDisplay(null, 'field_recording.v2.wav'),
    { title: 'field_recording.v2', subtitle: '' },
  );
  assert.equal(stripFileExtension('.hidden'), '.hidden');
  assert.equal(stripFileExtension('song.mp3'), 'song');
});

test('time formatting, spoken slider value, speed labels, orientation, and wire extractors', () => {
  assert.equal(formatPlayerTime(0), '0:00');
  assert.equal(formatPlayerTime(760), '12:40');
  assert.equal(formatPlayerTime(3661), '1:01:01');
  assert.equal(formatRemainingTime(15, 75), '\u22121:00');
  assert.equal(formatRemainingTime(15, 75).codePointAt(0), 0x2212);
  assert.equal(formatPositionValueText(135, 724), '2 minutes 15 seconds of 12 minutes 4 seconds');
  assert.equal(formatSpeed(1), '1×');
  assert.equal(formatSpeed(1.25), '1.25×');
  assert.deepEqual(PLAYBACK_SPEEDS, [0.5, 0.75, 1, 1.25, 1.5, 1.75, 2]);
  assert.equal(preferredVideoOrientation(1920, 1080), 'landscape');
  assert.equal(preferredVideoOrientation(720, 1280), 'portrait');
  assert.equal(clampVolume(1.4), 1);
  assert.equal(clampVolume(-0.2), 0);

  // Supports both node `{ results }` and mock `{ thumbnails }` shapes.
  const fromNode = extractThumbMap({
    results: [
      { path: '/a.mp3', ok: { mime: 'image/jpeg', content: 'abc', width: 512, height: 512 } },
      { path: '/b.mp3', error: { code: 'not_found', message: 'No cover' } },
    ],
  });
  assert.equal(fromNode['/a.mp3'].mime, 'image/jpeg');
  assert.equal(fromNode['/b.mp3'].error.code, 'not_found');

  const fromLegacy = extractThumbMap({
    thumbnails: { '/a.jpg': { mime: 'image/svg+xml', content: 'xyz' } },
  });
  assert.equal(fromLegacy['/a.jpg'].mime, 'image/svg+xml');

  const tags = extractTagsMap({
    results: [
      { path: '/a.mp3', ok: { title: 'Hi', cover: true } },
      { path: '/x.txt', error: { code: 'unsupported' } },
    ],
  });
  assert.equal(tags['/a.mp3'].ok.title, 'Hi');
  assert.equal(tags['/x.txt'].error.code, 'unsupported');
});

test('keyboard shortcuts map all documented player keys and respect text inputs & buttons', () => {
  assert.equal(classifyPlayerKey({ key: ' ' }, { mode: 'video' }), 'toggle_play');
  assert.equal(classifyPlayerKey({ key: 'k' }, { mode: 'video' }), 'toggle_play');
  assert.equal(classifyPlayerKey({ key: 'ArrowLeft' }, { mode: 'video' }), 'seek_back');
  assert.equal(classifyPlayerKey({ key: 'ArrowRight' }, { mode: 'video' }), 'seek_forward');
  assert.equal(classifyPlayerKey({ key: 'ArrowUp' }, { mode: 'video' }), 'volume_up');
  assert.equal(classifyPlayerKey({ key: 'ArrowDown' }, { mode: 'video' }), 'volume_down');
  assert.equal(classifyPlayerKey({ key: 'm' }, { mode: 'video' }), 'toggle_mute');
  assert.equal(classifyPlayerKey({ key: 'f' }, { mode: 'video' }), 'toggle_fullscreen');
  assert.equal(classifyPlayerKey({ key: 'f' }, { mode: 'audio' }), null);
  assert.equal(classifyPlayerKey({ key: 'n' }, { mode: 'video' }), 'next');
  assert.equal(classifyPlayerKey({ key: 'p' }, { mode: 'audio' }), 'prev');
  assert.equal(classifyPlayerKey({ key: 'Escape' }, { mode: 'video', isFullscreen: true }), 'exit_fullscreen');
  assert.equal(classifyPlayerKey({ key: 'Escape' }, { mode: 'video', isFullscreen: false }), 'close');

  // Space on a focused button belongs to the button; text input ignores all shortcuts.
  assert.equal(classifyPlayerKey({ key: ' ', target: { tagName: 'BUTTON' } }, { mode: 'video' }), null);
  assert.equal(classifyPlayerKey({ key: 'k', target: { tagName: 'INPUT', type: 'search' } }, { mode: 'video' }), null);
  assert.equal(classifyPlayerKey({ key: 'f', ctrlKey: true }, { mode: 'video' }), null);
});

// ── Browser immersive + wake lock implementation ─────────────────────────────────────────────

test('createBrowserImmersive requests fullscreen on viewer element, falls back to webkitEnterFullscreen, and manages wakeLock', async () => {
  const listeners = new Map();
  let fullscreenElement = null;
  let lockCalls = [];
  let unlocked = 0;

  const fakeDoc = {
    fullscreenEnabled: true,
    visibilityState: 'visible',
    get fullscreenElement() {
      return fullscreenElement;
    },
    async exitFullscreen() {
      fullscreenElement = null;
      for (const fn of listeners.get('fullscreenchange') || []) fn();
    },
    addEventListener(type, fn) {
      if (!listeners.has(type)) listeners.set(type, new Set());
      listeners.get(type).add(fn);
    },
    removeEventListener(type, fn) {
      listeners.get(type)?.delete(fn);
    },
  };

  const fakeElement = {
    async requestFullscreen() {
      fullscreenElement = fakeElement;
      for (const fn of listeners.get('fullscreenchange') || []) fn();
    },
  };

  const fakeScreen = {
    orientation: {
      async lock(o) {
        lockCalls.push(o);
      },
      unlock() {
        unlocked += 1;
      },
    },
  };

  let wakeReleased = 0;
  let wakeRequested = 0;
  const fakeNav = {
    wakeLock: {
      async request() {
        wakeRequested += 1;
        return {
          addEventListener() {},
          async release() {
            wakeReleased += 1;
          },
        };
      },
    },
  };

  const api = createBrowserImmersive({ doc: fakeDoc, nav: fakeNav, scr: fakeScreen });
  const observed = [];
  const unsub = api.onImmersiveChange((active) => observed.push(active));

  assert.equal(api.canImmersive({ element: fakeElement }), true);
  const entered = await api.immersive(true, { element: fakeElement, orientation: 'landscape' });
  assert.deepEqual(entered, { active: true, mode: 'element' });
  assert.equal(fullscreenElement, fakeElement);
  assert.deepEqual(lockCalls, ['landscape']);

  await api.immersive(false, { element: fakeElement });
  assert.equal(fullscreenElement, null);
  assert.equal(unlocked, 1);
  assert.deepEqual(observed, [true, false]);
  unsub();

  // Wake lock acquire + release.
  await api.keepScreenOn(true);
  assert.equal(wakeRequested, 1);
  await api.keepScreenOn(false);
  assert.equal(wakeReleased, 1);

  // iPhone Safari fallback (`webkitEnterFullscreen`) when Fullscreen API is disabled.
  let webkitEntered = 0;
  const disabledDoc = { fullscreenEnabled: false };
  const webkitVideo = {
    webkitEnterFullscreen() {
      webkitEntered += 1;
    },
  };
  assert.equal(canBrowserImmersive({ doc: disabledDoc, element: {}, video: null }), false);
  assert.equal(canBrowserImmersive({ doc: disabledDoc, element: {}, video: webkitVideo }), true);
  const safariApi = createBrowserImmersive({ doc: disabledDoc });
  const res = await safariApi.immersive(true, { element: {}, video: webkitVideo, orientation: 'landscape' });
  assert.deepEqual(res, { active: true, mode: 'webkit-video' });
  assert.equal(webkitEntered, 1);
});

test('createBrowserImmersive keeps one in-flight wakeLock request and never leaks locks across rapid toggles', async () => {
  const pendingGrants = [];
  let activeCount = 0;
  let totalRequested = 0;
  let totalReleased = 0;

  const fakeNav = {
    wakeLock: {
      request() {
        totalRequested += 1;
        return new Promise((resolve) => {
          pendingGrants.push(() => {
            activeCount += 1;
            let released = false;
            resolve({
              addEventListener() {},
              async release() {
                if (released) return;
                released = true;
                activeCount -= 1;
                totalReleased += 1;
              },
            });
          });
        });
      },
    },
  };

  const fakeDoc = {
    visibilityState: 'visible',
    addEventListener() {},
    removeEventListener() {},
  };

  const api = createBrowserImmersive({ doc: fakeDoc, nav: fakeNav });

  // Case 1: true -> false -> true while the first slow grant is still in flight.
  const p1 = api.keepScreenOn(true);
  const p2 = api.keepScreenOn(false);
  const p3 = api.keepScreenOn(true);
  assert.equal(totalRequested, 1, 'must keep only one in-flight wakeLock request');
  assert.equal(pendingGrants.length, 1);

  pendingGrants.shift()();
  await Promise.all([p1, p2, p3]);
  assert.equal(activeCount, 1);
  assert.equal(totalReleased, 0);

  // Release -> zero locks remain held.
  await api.keepScreenOn(false);
  assert.equal(activeCount, 0);
  assert.equal(totalReleased, 1);

  // Case 2: true -> false while request is still in flight -> released immediately upon grant.
  const p4 = api.keepScreenOn(true);
  const p5 = api.keepScreenOn(false);
  assert.equal(totalRequested, 2);
  pendingGrants.shift()();
  await Promise.all([p4, p5]);
  assert.equal(activeCount, 0);
  assert.equal(totalReleased, 2);
});

test('createFullscreenController owns state, history entry, and external platform exits as a single source of truth', async () => {
  const stack = [{ state: null, hash: '#/files/tower/Videos?view=A.mp4' }];
  let idx = 0;
  let currentHash = stack[0].hash;
  const fakeHistory = {
    get state() {
      return stack[idx].state;
    },
    replaceState(state, _title, url) {
      stack[idx].state = state;
      if (url) {
        stack[idx].hash = url;
        currentHash = url;
      }
    },
    pushState(state, _title, url) {
      stack.splice(idx + 1);
      const nextHash = url || currentHash;
      stack.push({ state, hash: nextHash });
      idx += 1;
      currentHash = nextHash;
    },
    back() {
      if (idx > 0) {
        idx -= 1;
        currentHash = stack[idx].hash;
        queueMicrotask(() => ctrl.handlePopState());
      }
    },
  };

  const calls = [];
  let platformListener = null;
  const fakeLink = {
    async immersive(on, opts) {
      calls.push({ on, orientation: opts?.orientation });
      return { active: on, mode: 'android' };
    },
    onImmersiveChange(fn) {
      platformListener = fn;
      return () => {
        platformListener = null;
      };
    },
  };

  const states = [];
  const ctrl = createFullscreenController({
    link: fakeLink,
    history: fakeHistory,
    getHash: () => currentHash,
    onChange: (s) => states.push(s),
  });

  // Enter via button -> pushes history entry.
  await ctrl.enter('button', { orientation: 'landscape' });
  assert.equal(ctrl.getState().active, true);
  assert.equal(ctrl.getState().source, 'button');
  assert.equal(ctrl.getState().pushedHistory, true);
  assert.equal(stack.length, 2);

  // Switch sibling while in fullscreen -> updates hash on top entry.
  fakeHistory.replaceState(fakeHistory.state, '', '#/files/tower/Videos?view=B.mp4');
  ctrl.noteViewerHash('#/files/tower/Videos?view=B.mp4');

  // Leave via action (like Rename/Delete) even though native platform sends NO change event:
  // updates state to inactive immediately and pops history cleanly back to B.mp4 (not A.mp4!).
  await ctrl.leave();
  assert.equal(ctrl.getState().active, false);
  assert.equal(ctrl.getState().source, null);
  assert.equal(ctrl.getState().pushedHistory, false);
  assert.equal(idx, 0);
  assert.equal(currentHash, '#/files/tower/Videos?view=B.mp4');
  assert.deepEqual(
    calls.map((c) => c.on),
    [true, false],
  );

  // Platform-initiated exit (e.g. browser Esc) clears state and pops history.
  await ctrl.enter('button', { orientation: 'landscape' });
  assert.equal(ctrl.getState().active, true);
  assert.equal(idx, 1);
  platformListener(false);
  await new Promise((r) => setTimeout(r, 10));
  assert.equal(ctrl.getState().active, false);
  assert.equal(idx, 0);

  ctrl.dispose();
});

test('detectAppImmersiveCapability and createAppLink report true only when a working implementation exists', async () => {
  // Android without __LEGION_ANDROID__ must NEVER fall through to Tauri desktop core.
  assert.deepEqual(
    detectAppImmersiveCapability({ isAndroid: true, hasAndroidBridge: false, hasTauriCore: true }),
    { supported: false, mode: 'none' },
  );
  assert.deepEqual(
    detectAppImmersiveCapability({ isAndroid: true, hasAndroidBridge: true, hasTauriCore: true }),
    { supported: true, mode: 'android' },
  );
  // iOS without message handler must NEVER fall through to Tauri desktop core.
  assert.deepEqual(
    detectAppImmersiveCapability({ isIos: true, hasIosHandler: false, hasTauriCore: true }),
    { supported: false, mode: 'none' },
  );
  assert.deepEqual(
    detectAppImmersiveCapability({ isIos: true, hasIosHandler: true, hasTauriCore: true }),
    { supported: true, mode: 'ios' },
  );
  // Desktop needs Tauri core.
  assert.deepEqual(
    detectAppImmersiveCapability({ hasTauriCore: true }),
    { supported: true, mode: 'desktop' },
  );
  assert.deepEqual(
    detectAppImmersiveCapability({ hasTauriCore: false }),
    { supported: false, mode: 'none' },
  );

  // Verify createAppLink on Android without bridge returns canImmersive() === false and
  // immersive(true) === { active: false, mode: 'none' } without invoking desktop commands.
  const prevWindow = globalThis.window;
  const prevNavDesc = Object.getOwnPropertyDescriptor(globalThis, 'navigator');
  const invoked = [];
  try {
    globalThis.window = {
      __TAURI__: {
        core: {
          invoke: async (cmd) => {
            invoked.push(cmd);
            return { active: false, fullscreen: false };
          },
        },
      },
    };
    Object.defineProperty(globalThis, 'navigator', {
      configurable: true,
      value: { userAgent: 'Mozilla/5.0 (Linux; Android 15; Pixel 8) AppleWebKit/537.36' },
    });
    const link = createAppLink();
    assert.equal(link.canImmersive(), false);
    const res = await link.immersive(true, { orientation: 'landscape' });
    assert.deepEqual(res, { active: false, mode: 'none' });
    assert.deepEqual(invoked, []);
  } finally {
    if (prevWindow === undefined) delete globalThis.window;
    else globalThis.window = prevWindow;
    if (prevNavDesc) Object.defineProperty(globalThis, 'navigator', prevNavDesc);
    else delete globalThis.navigator;
  }
});
