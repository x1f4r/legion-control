import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { extractThumbMap } from '../files.js';
import { canBrowserImmersive, createBrowserImmersive } from '../immersive-browser.js';
import { createAppLink, detectAppImmersiveCapability } from '../../link/app.js';
import {
  applyPinchResistance,
  buildMediaQueue,
  buildShuffledOrder,
  clampVolume,
  classifyPlayerKey,
  computeResumePosition,
  computeVideoZoomMetrics,
  createFullscreenController,
  createPausedDismissTimer,
  createPictureGestureRecognizer,
  createResumeStore,
  createSpaceHoldController,
  createTurnWatcher,
  createVideoController,
  cycleRepeatMode,
  DOUBLE_TAP_WINDOW_MS,
  extractTagsMap,
  formatMiniPlayerDisplay,
  formatMusicDisplay,
  formatPlayerTime,
  formatPositionValueText,
  formatRemainingTime,
  formatScrubDelta,
  formatSpeed,
  GESTURE_EDGE_MARGIN_PX,
  GESTURE_HOLD_DELAY_MS,
  GESTURE_MOVE_SLOP_PX,
  isNearEnd,
  isResumeEligible,
  MUSIC_MODES_STORAGE_KEY,
  MUSIC_PAUSED_DISMISS_MS,
  normalizeRepeatMode,
  OVERLAY_HIDE_DELAY_MS,
  PLAYBACK_SPEEDS,
  preferredVideoOrientation,
  queueNeighbors,
  readMusicModePreferences,
  readZoomPreference,
  resolveMusicEndedAction,
  resolveMusicPrevAction,
  resolveMusicQueueState,
  resolvePinchZoomTarget,
  RESUME_CHIP_DURATION_MS,
  RESUME_MAX_ENTRIES,
  RESUME_MIN_DURATION_S,
  RESUME_SAVE_INTERVAL_MS,
  RESUME_STORAGE_KEY,
  scrubRateForOffset,
  shouldMusicPrevRestart,
  STALL_SPINNER_DELAY_MS,
  stripFileExtension,
  tapZoneForPosition,
  TURN_LEAVE_GUARD_MS,
  TURN_SETTLE_MS,
  writeMusicModePreferences,
  writeZoomPreference,
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

test('resume store throttles periodic writes to at most every 10 s while pause, file change, and close write immediately', () => {
  const map = new Map();
  let reads = 0;
  let writes = 0;
  const countingStorage = {
    getItem(k) {
      reads += 1;
      return map.has(k) ? map.get(k) : null;
    },
    setItem(k, v) {
      writes += 1;
      map.set(k, String(v));
    },
    removeItem(k) {
      map.delete(k);
    },
  };

  assert.equal(RESUME_SAVE_INTERVAL_MS, 10_000);
  const clock = createFakeClock(100_000);
  const store = createResumeStore(countingStorage);
  const idA = { machine: 'tower', path: '/Videos/Long.mp4', size: 10_000, mtime: '2026-11-20T10:00:00Z' };
  const idB = { machine: 'tower', path: '/Videos/Next.mp4', size: 20_000, mtime: '2026-11-20T11:00:00Z' };

  // Short files never touch storage at all.
  assert.equal(store.savePeriodic(idA, 30, 300, clock.now()), false);
  assert.equal(reads, 0);
  assert.equal(writes, 0);

  // Calling savePeriodic every second for 25 seconds writes only at t=0, t=10s, and t=20s (3 writes).
  for (let sec = 0; sec <= 25; sec += 1) {
    store.savePeriodic(idA, 20 + sec, 1200, clock.now());
    clock.advance(1000);
  }
  assert.equal(writes, 3);
  assert.equal(reads, 3);
  assert.equal(store.load(idA, 1200), 40);

  // Immediate save on pause / close writes right away even inside the 10-second window.
  clock.advance(1000); // 7s since last periodic write
  assert.equal(store.save(idA, 47, 1200, clock.now()), true);
  assert.equal(writes, 4);
  assert.equal(store.load(idA, 1200), 47);

  // Immediate write resets the 10s periodic window for the same file.
  clock.advance(5000);
  assert.equal(store.savePeriodic(idA, 52, 1200, clock.now()), false);
  assert.equal(writes, 4);

  // Switching to a different file writes immediately without waiting for the previous file's window.
  assert.equal(store.savePeriodic(idB, 15, 1200, clock.now()), true);
  assert.equal(writes, 5);
  assert.equal(store.load(idB, 1200), 15);
});

test('resume store renameMachine moves local and placeholder entries onto the resolved node name without clobbering existing ones', () => {
  const storage = createMemoryStorage();
  const store = createResumeStore(storage);
  assert.equal(RESUME_STORAGE_KEY, 'legion.player.resume.v1');

  const localMovie = { machine: 'local', path: '/home/me/Videos/Film.mp4', size: 1000, mtime: 'm1' };
  const pendingTrack = { machine: 'This computer', path: '/home/me/Music/Set.flac', size: 2000, mtime: 'm2' };
  const conflictLocal = { machine: 'local', path: '/home/me/Videos/Both.mp4', size: 3000, mtime: 'm3' };
  const conflictNative = { machine: 'studio', path: '/home/me/Videos/Both.mp4', size: 3000, mtime: 'm3' };
  const otherMachine = { machine: 'tower', path: '/home/me/Videos/Film.mp4', size: 1000, mtime: 'm1' };

  store.save(localMovie, 420, 1800, 100);
  store.save(pendingTrack, 610, 2400, 200);
  store.save(conflictLocal, 150, 1800, 300);
  store.save(conflictNative, 900, 1800, 400);
  store.save(otherMachine, 333, 1800, 500);

  assert.equal(store.renameMachine('local', 'studio'), true);
  assert.equal(store.renameMachine('This computer', 'studio'), true);
  assert.equal(store.renameMachine('local', 'studio'), false);

  assert.equal(store.load(localMovie, 1800), null);
  assert.equal(store.load({ ...localMovie, machine: 'studio' }, 1800), 420);
  assert.equal(store.load(pendingTrack, 2400), null);
  assert.equal(store.load({ ...pendingTrack, machine: 'studio' }, 2400), 610);
  // Existing entry under the real machine name wins over the older local entry.
  assert.equal(store.load(conflictNative, 1800), 900);
  assert.equal(store.load(conflictLocal, 1800), null);
  // Unrelated machines are untouched.
  assert.equal(store.load(otherMachine, 1800), 333);
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

  // Shared `extractThumbMap` handles node `{ results }` replies cleanly.
  const fromNode = extractThumbMap({
    results: [
      { path: '/a.mp3', ok: { mime: 'image/jpeg', content: 'abc', width: 512, height: 512 } },
      { path: '/b.mp3', error: { code: 'not_found', message: 'No cover' } },
    ],
  });
  assert.equal(fromNode['/a.mp3'].mime, 'image/jpeg');
  assert.equal(fromNode['/b.mp3'].error.code, 'not_found');

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

  // Wake lock acquire + release reports actual held state.
  assert.equal(await api.keepScreenOn(true), true);
  assert.equal(wakeRequested, 1);
  assert.equal(await api.keepScreenOn(false), false);
  assert.equal(wakeReleased, 1);

  // Without Wake Lock API, when hidden, or when denied, keepScreenOn honestly returns false.
  const noWakeApi = createBrowserImmersive({ doc: { visibilityState: 'visible', addEventListener() {}, removeEventListener() {} }, nav: {} });
  assert.equal(await noWakeApi.keepScreenOn(true), false);

  const deniedWakeApi = createBrowserImmersive({
    doc: { visibilityState: 'visible', addEventListener() {}, removeEventListener() {} },
    nav: {
      wakeLock: {
        request: () => Promise.reject(new Error('Battery saver')),
      },
    },
  });
  assert.equal(await deniedWakeApi.keepScreenOn(true), false);

  const hiddenWakeApi = createBrowserImmersive({
    doc: { visibilityState: 'hidden', addEventListener() {}, removeEventListener() {} },
    nav: fakeNav,
  });
  assert.equal(await hiddenWakeApi.keepScreenOn(true), false);

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

test('createTurnWatcher uses orientation events on injectable clock, guards post-leave bounces, and blocks re-entry during in-flight lock', async () => {
  const clock = createFakeClock();
  let landscape = false;
  let fullscreen = false;
  let source = null;
  let externalInFlight = false;
  let enterCount = 0;
  let leaveCount = 0;
  let resolvePendingEnter = null;

  const watcher = createTurnWatcher({
    clock,
    hasOrientationEvent: true,
    isLandscape: () => landscape,
    isFullscreen: () => fullscreen,
    getFullscreenSource: () => source,
    isInFlight: () => externalInFlight,
    onEnterRotation: () => {
      enterCount += 1;
      return new Promise((resolve) => {
        resolvePendingEnter = () => {
          fullscreen = true;
          source = 'rotation';
          resolve();
        };
      });
    },
    onLeaveRotation: () => {
      leaveCount += 1;
      fullscreen = false;
      source = null;
    },
  });

  watcher.start();
  assert.equal(enterCount, 0);

  // Plain resize is ignored when the platform has orientation events.
  landscape = true;
  watcher.onResize();
  clock.advance(TURN_SETTLE_MS * 2);
  assert.equal(enterCount, 0);

  // Reset to portrait, then turn to landscape via orientation event -> enters after settleMs.
  landscape = false;
  watcher.onOrientationEvent();
  clock.advance(TURN_SETTLE_MS);
  landscape = true;
  watcher.onOrientationEvent();
  clock.advance(TURN_SETTLE_MS - 1);
  assert.equal(enterCount, 0);
  clock.advance(1);
  assert.equal(enterCount, 1);

  // Second turn / bounce while the first orientation.lock is still in flight must NOT start another entry.
  landscape = false;
  watcher.onOrientationEvent();
  clock.advance(TURN_SETTLE_MS);
  landscape = true;
  watcher.onOrientationEvent();
  clock.advance(TURN_SETTLE_MS);
  assert.equal(enterCount, 1);

  // Finish the first entry and turn back to portrait -> leaves rotation fullscreen.
  resolvePendingEnter();
  await Promise.resolve();
  landscape = false;
  watcher.onOrientationEvent();
  clock.advance(TURN_SETTLE_MS);
  assert.equal(leaveCount, 1);
  assert.equal(fullscreen, false);

  // Leaving fullscreen sets a clock-based guard that is NOT cleared early by a transient portrait reading.
  watcher.noteLeave();
  landscape = false;
  watcher.onOrientationEvent();
  clock.advance(TURN_SETTLE_MS); // still within TURN_LEAVE_GUARD_MS (200 < 600)
  landscape = true;
  watcher.onOrientationEvent();
  clock.advance(TURN_SETTLE_MS); // t = 400 < 600 -> must NOT bounce back into fullscreen!
  assert.equal(enterCount, 1);

  // After the guard expires, turning portrait -> landscape enters again cleanly.
  clock.advance(TURN_LEAVE_GUARD_MS);
  landscape = false;
  watcher.onOrientationEvent();
  clock.advance(TURN_SETTLE_MS);
  // Also verify external controller inFlight (e.g. button-initiated orientation.lock) prevents a second entry.
  externalInFlight = true;
  landscape = true;
  watcher.onOrientationEvent();
  clock.advance(TURN_SETTLE_MS);
  assert.equal(enterCount, 1);

  externalInFlight = false;
  landscape = false;
  watcher.onOrientationEvent();
  clock.advance(TURN_SETTLE_MS);
  landscape = true;
  watcher.onOrientationEvent();
  clock.advance(TURN_SETTLE_MS);
  assert.equal(enterCount, 2);

  watcher.dispose();
});

test('createTurnWatcher re-reads orientation when an in-flight entry finishes after turning back to portrait', async () => {
  const clock = createFakeClock();
  let landscape = false;
  let fullscreen = false;
  let source = null;
  let externalInFlight = false;
  let enterCount = 0;
  let leaveCount = 0;
  let resolveEnter = null;

  const watcher = createTurnWatcher({
    clock,
    hasOrientationEvent: true,
    isLandscape: () => landscape,
    isFullscreen: () => fullscreen,
    getFullscreenSource: () => source,
    isInFlight: () => externalInFlight,
    onEnterRotation: () => {
      enterCount += 1;
      return new Promise((resolve) => {
        resolveEnter = () => {
          fullscreen = true;
          source = 'rotation';
          resolve();
        };
      });
    },
    onLeaveRotation: () => {
      leaveCount += 1;
      fullscreen = false;
      source = null;
    },
  });

  watcher.start();

  // 1. Turn to landscape -> rotation entry starts and stays in flight.
  landscape = true;
  watcher.onOrientationEvent();
  clock.advance(TURN_SETTLE_MS);
  assert.equal(enterCount, 1);
  assert.equal(leaveCount, 0);

  // 2. Turn back to portrait BEFORE the entry resolves -> reading arrives while pendingOp is true.
  landscape = false;
  watcher.onOrientationEvent();
  clock.advance(TURN_SETTLE_MS);
  assert.equal(leaveCount, 0);

  // 3. Entry finishes -> watcher must immediately re-read orientation (portrait) and leave rotation fullscreen!
  resolveEnter();
  await Promise.resolve();
  assert.equal(leaveCount, 1);
  assert.equal(fullscreen, false);
  assert.equal(source, null);

  // 4. External in-flight op blocking an orientation reading -> re-evaluates once notifySettled() fires.
  clock.advance(TURN_LEAVE_GUARD_MS);
  externalInFlight = true;
  landscape = true;
  watcher.onOrientationEvent();
  clock.advance(TURN_SETTLE_MS);
  assert.equal(enterCount, 1);

  externalInFlight = false;
  watcher.notifySettled();
  assert.equal(enterCount, 2);
  resolveEnter();
  await Promise.resolve();
  assert.equal(fullscreen, true);
  assert.equal(leaveCount, 1);

  watcher.dispose();
});

test('iOS/iPadOS bridge attaches at WKWebView creation, fixes requestGeometryUpdate, resets on load/termination, and hides button when absent', async () => {
  const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../../..');
  const swiftSrc = fs.readFileSync(
    path.join(repoRoot, 'apps/legion-app/src-tauri/gen/apple/Sources/legion-app/ImmersiveBridge.swift'),
    'utf8',
  );
  const mainMmSrc = fs.readFileSync(
    path.join(repoRoot, 'apps/legion-app/src-tauri/gen/apple/Sources/legion-app/main.mm'),
    'utf8',
  );
  const docSrc = fs.readFileSync(path.join(repoRoot, 'docs/legion/player.md'), 'utf8');

  // 1. No 3-second polling timer in ImmersiveBridge.swift; attaches deterministically at WKWebView creation.
  assert.doesNotMatch(swiftSrc, /retryAttachSoon|attachAttempts|asyncAfter/);
  assert.match(swiftSrc, /legion_ios_attach_webview/);
  assert.match(swiftSrc, /legion_ios_reset_player_state/);
  assert.match(swiftSrc, /injectionTime:\s*\.atDocumentStart/);
  assert.match(swiftSrc, /case "haptic":/);
  assert.match(swiftSrc, /UIImpactFeedbackGenerator/);
  assert.match(mainMmSrc, /initWithFrame:configuration:/);
  assert.match(mainMmSrc, /webViewWebContentProcessDidTerminate:/);

  // 2. requestGeometryUpdate error closure must not hold setNeedsUpdateOfSupportedInterfaceOrientations.
  assert.doesNotMatch(
    swiftSrc,
    /requestGeometryUpdate\([^)]*\)\s*\{\s*_ in\s+[\s\S]*?setNeedsUpdateOfSupportedInterfaceOrientations\(\)\s*\}/,
  );
  assert.match(
    swiftSrc,
    /requestGeometryUpdate\(\.iOS\(interfaceOrientations:\s*restoreMask\)\)\s*\{\s*_ in\s*\}\s*\n\s*vc\.setNeedsUpdateOfSupportedInterfaceOrientations\(\)/,
  );

  // 3. iPadOS multitasking note documented alongside Android tablets.
  assert.match(docSrc, /iPadOS with multitasking.*bars hide and the video fills/);

  // 4. Without the WKScriptMessageHandler on iOS, createAppLink reports canImmersive() === false (never a dead button);
  //    with it present, posts immersive and keep_screen_on messages directly.
  const prevWindow = globalThis.window;
  const prevNavDesc = Object.getOwnPropertyDescriptor(globalThis, 'navigator');
  const invoked = [];
  const posted = [];
  try {
    Object.defineProperty(globalThis, 'navigator', {
      configurable: true,
      value: { userAgent: 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15' },
    });

    globalThis.window = {
      __TAURI__: { core: { invoke: async (cmd) => invoked.push(cmd) } },
    };
    const linkNoHandler = createAppLink();
    assert.equal(linkNoHandler.canImmersive(), false);
    assert.equal(linkNoHandler.haptic, undefined);
    assert.deepEqual(await linkNoHandler.immersive(true, { orientation: 'landscape' }), {
      active: false,
      mode: 'none',
    });
    assert.deepEqual(invoked, []);

    globalThis.window = {
      __TAURI__: { core: { invoke: async (cmd) => invoked.push(cmd) } },
      webkit: {
        messageHandlers: {
          legionImmersive: {
            postMessage(msg) {
              posted.push(msg);
            },
          },
        },
      },
    };
    const linkWithHandler = createAppLink();
    assert.equal(linkWithHandler.canImmersive(), true);
    assert.equal(typeof linkWithHandler.haptic, 'function');
    assert.deepEqual(await linkWithHandler.immersive(true, { orientation: 'landscape' }), {
      active: true,
      mode: 'ios',
    });
    assert.equal(await linkWithHandler.keepScreenOn(true), true);
    linkWithHandler.haptic('tick');
    assert.deepEqual(posted, [
      { type: 'immersive', on: true, orientation: 'landscape' },
      { type: 'keep_screen_on', on: true },
      { type: 'haptic', kind: 'tick' },
    ]);
    assert.deepEqual(invoked, []);
  } finally {
    if (prevWindow === undefined) delete globalThis.window;
    else globalThis.window = prevWindow;
    if (prevNavDesc) Object.defineProperty(globalThis, 'navigator', prevNavDesc);
    else delete globalThis.navigator;
  }
});

test('picture gesture recogniser covers every table row, 1 px / 1 ms thresholds, edges, and second-finger preemption', () => {
  const clock = createFakeClock();
  let layerVisible = true;
  let playing = true;
  let speed = 1;
  let fullscreen = true;
  let zoomMode = 'fit';
  let fillScale = 1.25;
  let currentTime = 100;
  const duration = 600;
  const barWidth = 600;
  const bounds = { left: 0, top: 0, width: 400, height: 800, bottomMarginPx: 24 };

  const events = [];
  const rec = createPictureGestureRecognizer({
    clock,
    edgeMarginPx: GESTURE_EDGE_MARGIN_PX,
    getBounds: () => bounds,
    isLayerVisible: () => layerVisible,
    canHold: () => playing && speed < 2,
    canScrub: () => duration > 0,
    canPinch: () => fullscreen && fillScale > 1.03,
    getZoomFillScale: () => fillScale,
    getZoomMode: () => zoomMode,
    getScrubBarWidth: () => barWidth,
    getCurrentTime: () => currentTime,
    getDuration: () => duration,
    onTap: (zone) => events.push({ type: 'tap', zone }),
    onHoldStart: () => events.push({ type: 'hold_start' }),
    onHoldEnd: () => events.push({ type: 'hold_end' }),
    onScrubStart: (p) => events.push({ type: 'scrub_start', ...p }),
    onScrubMove: (p) => events.push({ type: 'scrub_move', ...p }),
    onScrubEnd: (p) => events.push({ type: 'scrub_end', ...p }),
    onScrubCancel: () => events.push({ type: 'scrub_cancel' }),
    onPinchStart: (p) => events.push({ type: 'pinch_start', ...p }),
    onPinchMove: (p) => events.push({ type: 'pinch_move', ...p }),
    onPinchEnd: (p) => events.push({ type: 'pinch_end', ...p }),
    onPinchCancel: () => events.push({ type: 'pinch_cancel' }),
  });

  // 1. Edge exclusion: 24 px from left, right, or bottom starts nothing; 25 px starts normally.
  for (const [x, y] of [
    [24, 400],
    [376, 400],
    [200, 776],
  ]) {
    assert.equal(rec.onPointerDown({ pointerId: 1, pointerType: 'touch', clientX: x, clientY: y }), false);
    rec.onPointerUp({ pointerId: 1, pointerType: 'touch', clientX: x, clientY: y });
  }
  assert.deepEqual(events, []);

  for (const [x, y, expectedZone] of [
    [25, 400, 'left'],
    [375, 400, 'right'],
    [200, 775, 'center'],
  ]) {
    assert.equal(rec.onPointerDown({ pointerId: 1, pointerType: 'touch', clientX: x, clientY: y }), true);
    clock.advance(50);
    rec.onPointerUp({ pointerId: 1, pointerType: 'touch', clientX: x, clientY: y });
    assert.deepEqual(events.pop(), { type: 'tap', zone: expectedZone });
  }

  // 2. Time threshold: 349 ms within 12 px is a tap; 350 ms within 12 px is a hold, release is NOT a tap.
  rec.onPointerDown({ pointerId: 1, pointerType: 'touch', clientX: 200, clientY: 300 });
  rec.onPointerMove({ pointerId: 1, pointerType: 'touch', clientX: 212, clientY: 300 });
  clock.advance(GESTURE_HOLD_DELAY_MS - 1);
  assert.equal(rec.getState().state, 'pending');
  rec.onPointerUp({ pointerId: 1, pointerType: 'touch', clientX: 212, clientY: 300 });
  assert.deepEqual(events, [{ type: 'tap', zone: 'center' }]);
  events.length = 0;

  rec.onPointerDown({ pointerId: 1, pointerType: 'touch', clientX: 200, clientY: 300 });
  rec.onPointerMove({ pointerId: 1, pointerType: 'touch', clientX: 212, clientY: 300 });
  clock.advance(GESTURE_HOLD_DELAY_MS);
  assert.equal(rec.getState().state, 'hold');
  rec.onPointerUp({ pointerId: 1, pointerType: 'touch', clientX: 212, clientY: 300 });
  assert.deepEqual(events, [{ type: 'hold_start' }, { type: 'hold_end' }]);
  events.length = 0;

  // Staying still >= 350 ms while paused or at 2x neither starts hold nor fires tap on release.
  playing = false;
  rec.onPointerDown({ pointerId: 1, pointerType: 'touch', clientX: 200, clientY: 300 });
  clock.advance(350);
  rec.onPointerUp({ pointerId: 1, pointerType: 'touch', clientX: 200, clientY: 300 });
  assert.deepEqual(events, []);
  playing = true;
  speed = 2;
  rec.onPointerDown({ pointerId: 1, pointerType: 'touch', clientX: 200, clientY: 300 });
  clock.advance(350);
  rec.onPointerUp({ pointerId: 1, pointerType: 'touch', clientX: 200, clientY: 300 });
  assert.deepEqual(events, []);
  speed = 1;

  // 3. Distance threshold (12 vs 13 px) and angle threshold (29° vs 31°).
  rec.onPointerDown({ pointerId: 1, pointerType: 'touch', clientX: 200, clientY: 300 });
  rec.onPointerMove({ pointerId: 1, pointerType: 'touch', clientX: 213, clientY: 300 });
  assert.equal(rec.getState().state, 'scrub');
  rec.onPointerUp({ pointerId: 1, pointerType: 'touch', clientX: 213, clientY: 300 });
  assert.equal(events.filter((e) => e.type === 'tap').length, 0);
  assert.equal(events.at(-1).type, 'scrub_end');
  assert.equal(events.at(-1).committed, true);
  assert.equal(events.at(-1).time, 113);
  events.length = 0;

  const rad29 = (29 * Math.PI) / 180;
  rec.onPointerDown({ pointerId: 1, pointerType: 'touch', clientX: 200, clientY: 300 });
  rec.onPointerMove({ pointerId: 1, pointerType: 'touch', clientX: 200 + 20 * Math.cos(rad29), clientY: 300 + 20 * Math.sin(rad29) });
  assert.equal(rec.getState().state, 'scrub');
  rec.onPointerCancel({ pointerId: 1 });
  events.length = 0;

  const rad31 = (31 * Math.PI) / 180;
  rec.onPointerDown({ pointerId: 1, pointerType: 'touch', clientX: 200, clientY: 300 });
  rec.onPointerMove({ pointerId: 1, pointerType: 'touch', clientX: 200 + 20 * Math.cos(rad31), clientY: 300 + 20 * Math.sin(rad31) });
  assert.equal(rec.getState().state, 'ignored');
  rec.onPointerUp({ pointerId: 1, pointerType: 'touch', clientX: 200 + 20 * Math.cos(rad31), clientY: 300 + 20 * Math.sin(rad31) });
  assert.deepEqual(events, []);

  // 4. Vertical drag and sideways drag with layer hidden -> nothing, never turns into a tap.
  rec.onPointerDown({ pointerId: 1, pointerType: 'touch', clientX: 200, clientY: 300 });
  rec.onPointerMove({ pointerId: 1, pointerType: 'touch', clientX: 200, clientY: 340 });
  rec.onPointerUp({ pointerId: 1, pointerType: 'touch', clientX: 200, clientY: 340 });
  assert.deepEqual(events, []);

  layerVisible = false;
  rec.onPointerDown({ pointerId: 1, pointerType: 'touch', clientX: 200, clientY: 300 });
  rec.onPointerMove({ pointerId: 1, pointerType: 'touch', clientX: 260, clientY: 300 });
  rec.onPointerUp({ pointerId: 1, pointerType: 'touch', clientX: 260, clientY: 300 });
  assert.deepEqual(events, []);
  layerVisible = true;

  // 5. Second finger during scrub turns it into pinch, cancels scrub without seeking, and release is not a tap.
  rec.onPointerDown({ pointerId: 1, pointerType: 'touch', clientX: 150, clientY: 300 });
  rec.onPointerMove({ pointerId: 1, pointerType: 'touch', clientX: 210, clientY: 300 });
  assert.equal(rec.getState().state, 'scrub');
  rec.onPointerDown({ pointerId: 2, pointerType: 'touch', clientX: 300, clientY: 300 });
  assert.equal(rec.getState().state, 'pinch');
  assert.ok(events.some((e) => e.type === 'scrub_cancel'));
  assert.equal(events.some((e) => e.type === 'scrub_end'), false);
  clock.advance(40);
  rec.onPointerMove({ pointerId: 2, pointerType: 'touch', clientX: 340, clientY: 300 });
  rec.onPointerUp({ pointerId: 2, pointerType: 'touch', clientX: 340, clientY: 300 });
  rec.onPointerUp({ pointerId: 1, pointerType: 'touch', clientX: 210, clientY: 300 });
  assert.equal(events.filter((e) => e.type === 'tap').length, 0);
  assert.equal(events.filter((e) => e.type === 'scrub_end').length, 0);
  const pinchEnd = events.find((e) => e.type === 'pinch_end');
  assert.equal(pinchEnd?.targetMode, 'fill');
  assert.equal(pinchEnd?.changed, true);
  events.length = 0;

  // 6. Second finger during hold ends hold and switches to pinch.
  rec.onPointerDown({ pointerId: 1, pointerType: 'touch', clientX: 150, clientY: 300 });
  clock.advance(350);
  assert.equal(rec.getState().state, 'hold');
  rec.onPointerDown({ pointerId: 2, pointerType: 'touch', clientX: 250, clientY: 300 });
  assert.equal(rec.getState().state, 'pinch');
  assert.deepEqual(events.slice(0, 3).map((e) => e.type), ['hold_start', 'hold_end', 'pinch_start']);
  rec.onPointerUp({ pointerId: 2, pointerType: 'touch', clientX: 250, clientY: 300 });
  rec.onPointerUp({ pointerId: 1, pointerType: 'touch', clientX: 150, clientY: 300 });
  assert.equal(events.filter((e) => e.type === 'tap').length, 0);
  events.length = 0;

  // 7. Mouse participates in press-and-hold and sideways drag scrub (ignores right click and edge margins).
  assert.equal(rec.onPointerDown({ pointerId: 9, pointerType: 'mouse', button: 2, clientX: 200, clientY: 300 }), false);
  assert.equal(rec.onPointerDown({ pointerId: 9, pointerType: 'mouse', button: 0, clientX: 10, clientY: 300 }), true);
  clock.advance(350);
  assert.equal(rec.getState().state, 'hold');
  rec.onPointerUp({ pointerId: 9, pointerType: 'mouse', button: 0, clientX: 10, clientY: 300 });
  assert.deepEqual(events, [{ type: 'hold_start' }, { type: 'hold_end' }]);
  events.length = 0;

  rec.onPointerDown({ pointerId: 9, pointerType: 'mouse', button: 0, clientX: 100, clientY: 300 });
  rec.onPointerMove({ pointerId: 9, pointerType: 'mouse', clientX: 160, clientY: 300 });
  rec.onPointerUp({ pointerId: 9, pointerType: 'mouse', clientX: 160, clientY: 300 });
  assert.equal(events.at(-1).type, 'scrub_end');
  assert.equal(events.at(-1).committed, true);
  assert.equal(events.at(-1).time, 160);

  rec.dispose();
});

test('scrub anywhere maps bar travel, applies three slower vertical rates and back, and cancels within 12 px', () => {
  const clock = createFakeClock();
  const moves = [];
  const ends = [];
  const rec = createPictureGestureRecognizer({
    clock,
    isLayerVisible: () => true,
    canScrub: () => true,
    getScrubBarWidth: () => 600,
    getCurrentTime: () => 120,
    getDuration: () => 600,
    onScrubMove: (p) => moves.push(p),
    onScrubEnd: (p) => ends.push(p),
  });

  // Rate boundaries: 80 vs 81, 160 vs 161, 240 vs 241.
  assert.deepEqual(scrubRateForOffset(80), { rate: 1, label: null });
  assert.deepEqual(scrubRateForOffset(81), { rate: 0.5, label: 'Half speed' });
  assert.deepEqual(scrubRateForOffset(160), { rate: 0.5, label: 'Half speed' });
  assert.deepEqual(scrubRateForOffset(161), { rate: 0.25, label: 'Quarter speed' });
  assert.deepEqual(scrubRateForOffset(240), { rate: 0.25, label: 'Quarter speed' });
  assert.deepEqual(scrubRateForOffset(241), { rate: 0.1, label: 'Fine' });
  assert.equal(formatScrubDelta(35), '+0:35');
  assert.equal(formatScrubDelta(-12), '−0:12');

  rec.onPointerDown({ pointerId: 1, pointerType: 'touch', clientX: 100, clientY: 100 });
  rec.onPointerMove({ pointerId: 1, pointerType: 'touch', clientX: 160, clientY: 100 });
  rec.onPointerMove({ pointerId: 1, pointerType: 'touch', clientX: 160, clientY: 181 });
  assert.equal(moves.at(-1).rateLabel, 'Half speed');
  assert.equal(moves.at(-1).rateChanged, true);
  rec.onPointerMove({ pointerId: 1, pointerType: 'touch', clientX: 200, clientY: 181 });
  rec.onPointerMove({ pointerId: 1, pointerType: 'touch', clientX: 200, clientY: 261 });
  assert.equal(moves.at(-1).rateLabel, 'Quarter speed');
  assert.equal(moves.at(-1).rateChanged, true);
  rec.onPointerMove({ pointerId: 1, pointerType: 'touch', clientX: 240, clientY: 261 });
  rec.onPointerMove({ pointerId: 1, pointerType: 'touch', clientX: 240, clientY: 341 });
  assert.equal(moves.at(-1).rateLabel, 'Fine');
  assert.equal(moves.at(-1).rateChanged, true);
  rec.onPointerMove({ pointerId: 1, pointerType: 'touch', clientX: 290, clientY: 341 });
  rec.onPointerMove({ pointerId: 1, pointerType: 'touch', clientX: 290, clientY: 260 });
  assert.equal(moves.at(-1).rateLabel, 'Half speed');
  assert.equal(moves.at(-1).rateChanged, true);
  rec.onPointerMove({ pointerId: 1, pointerType: 'touch', clientX: 310, clientY: 260 });
  rec.onPointerMove({ pointerId: 1, pointerType: 'touch', clientX: 310, clientY: 180 });
  assert.equal(moves.at(-1).rateLabel, null);
  assert.equal(moves.at(-1).rateChanged, true);
  rec.onPointerMove({ pointerId: 1, pointerType: 'touch', clientX: 320, clientY: 180 });
  rec.onPointerUp({ pointerId: 1, pointerType: 'touch', clientX: 320, clientY: 180 });
  assert.equal(ends.length, 1);
  assert.equal(ends[0].committed, true);
  assert.ok(Math.abs(ends[0].time - 235) < 0.001);
  assert.ok(Math.abs(ends[0].delta - 115) < 0.001);

  // Returning to within 12 px of origin cancels without seeking; 13 px commits.
  ends.length = 0;
  rec.onPointerDown({ pointerId: 2, pointerType: 'touch', clientX: 100, clientY: 100 });
  rec.onPointerMove({ pointerId: 2, pointerType: 'touch', clientX: 180, clientY: 100 });
  rec.onPointerMove({ pointerId: 2, pointerType: 'touch', clientX: 112, clientY: 100 });
  rec.onPointerUp({ pointerId: 2, pointerType: 'touch', clientX: 112, clientY: 100 });
  assert.equal(ends[0].committed, false);
  assert.equal(ends[0].time, 120);

  ends.length = 0;
  rec.onPointerDown({ pointerId: 3, pointerType: 'touch', clientX: 100, clientY: 100 });
  rec.onPointerMove({ pointerId: 3, pointerType: 'touch', clientX: 180, clientY: 100 });
  rec.onPointerMove({ pointerId: 3, pointerType: 'touch', clientX: 113, clientY: 100 });
  rec.onPointerUp({ pointerId: 3, pointerType: 'touch', clientX: 113, clientY: 100 });
  assert.equal(ends[0].committed, true);
  assert.ok(Math.abs(ends[0].time - 133) < 0.001);
  rec.dispose();
});

test('zoom to fill enforces 3% availability, 15% auto-apply rule, resistance, nearer vs velocity settle, storage, and Z key', () => {
  const exact3 = computeVideoZoomMetrics(1030, 1000, 1000, 1000);
  assert.equal(exact3.available, false);
  assert.equal(exact3.fillScale, 1);

  const over3 = computeVideoZoomMetrics(1035, 1000, 1000, 1000);
  assert.equal(over3.available, true);
  assert.ok(Math.abs(over3.fillScale - 1.035) < 0.0001);
  assert.equal(over3.canAutoApplyFill, true);

  // 16:9 on 18.5:9 screen (~13.5% crop <= 15% -> auto-applies remembered fill).
  const moderateWide = computeVideoZoomMetrics(1920, 1080, 2220, 1080);
  assert.equal(moderateWide.available, true);
  assert.ok(moderateWide.hiddenFraction < 0.15);
  assert.equal(moderateWide.canAutoApplyFill, true);

  // 4:3 film on a tall phone in landscape (~38% crop > 15% -> available to pinch, but starts whole).
  const fourByThreeOnTallPhone = computeVideoZoomMetrics(1440, 1080, 2340, 1080);
  assert.equal(fourByThreeOnTallPhone.available, true);
  assert.ok(fourByThreeOnTallPhone.hiddenFraction > 0.15);
  assert.equal(fourByThreeOnTallPhone.canAutoApplyFill, false);

  // Pinch resistance stays 1:1 in range and resists gently beyond either end.
  assert.equal(applyPinchResistance(1.12, 1.25), 1.12);
  assert.ok(applyPinchResistance(0.6, 1.25) < 1 && applyPinchResistance(0.6, 1.25) > 0.9);
  assert.ok(applyPinchResistance(1.8, 1.25) > 1.25 && applyPinchResistance(1.8, 1.25) < 1.4);

  // Nearer state decides when velocity is low; fast velocity decides even before midpoint.
  assert.equal(resolvePinchZoomTarget({ scale: 1.1, fillScale: 1.25, velocity: 0 }), 'fit');
  assert.equal(resolvePinchZoomTarget({ scale: 1.15, fillScale: 1.25, velocity: 0 }), 'fill');
  assert.equal(resolvePinchZoomTarget({ scale: 1.05, fillScale: 1.25, velocity: 0.8 }), 'fill');
  assert.equal(resolvePinchZoomTarget({ scale: 1.22, fillScale: 1.25, velocity: -0.8 }), 'fit');

  const store = createMemoryStorage();
  assert.equal(readZoomPreference(store), 'fit');
  writeZoomPreference(store, 'fill');
  assert.equal(readZoomPreference(store), 'fill');
  writeZoomPreference(store, 'fit');
  assert.equal(readZoomPreference(store), 'fit');

  // Z key toggles zoom ONLY in video full screen.
  assert.equal(classifyPlayerKey({ key: 'z' }, { mode: 'video', isFullscreen: true }), 'toggle_zoom');
  assert.equal(classifyPlayerKey({ key: 'Z' }, { mode: 'video', isFullscreen: true }), 'toggle_zoom');
  assert.equal(classifyPlayerKey({ key: 'z' }, { mode: 'video', isFullscreen: false }), null);
  assert.equal(classifyPlayerKey({ key: 'z' }, { mode: 'audio', isFullscreen: true }), null);
});

test('desktop Space hold engages 2x at 350 ms without toggling play/pause and short press < 350 ms toggles normally', () => {
  const clock = createFakeClock();
  let playing = true;
  let speed = 1.5;
  const log = [];
  const space = createSpaceHoldController({
    clock,
    canHold: () => playing && speed < 2,
    onTogglePlay: () => {
      playing = !playing;
      log.push({ type: 'toggle', playing });
    },
    onHoldStart: () => log.push({ type: 'hold_start', restoreTo: speed }),
    onHoldEnd: () => log.push({ type: 'hold_end', restored: speed }),
  });

  // Short press (349 ms) toggles play/pause on release.
  space.onKeyDown({ repeat: false });
  clock.advance(349);
  assert.equal(space.isHolding(), false);
  space.onKeyUp();
  assert.deepEqual(log, [{ type: 'toggle', playing: false }]);
  log.length = 0;

  // Pressing Space while paused toggles immediately and holding does NOT engage 2x.
  space.onKeyDown({ repeat: false });
  clock.advance(400);
  space.onKeyDown({ repeat: true });
  assert.equal(space.isHolding(), false);
  space.onKeyUp();
  assert.deepEqual(log, [{ type: 'toggle', playing: true }]);
  log.length = 0;

  // Holding Space >= 350 ms while playing at 1.5x engages 2x; key repeats are ignored; release restores 1.5x without pausing.
  space.onKeyDown({ repeat: false });
  clock.advance(350);
  assert.equal(space.isHolding(), true);
  space.onKeyDown({ repeat: true });
  space.onKeyUp();
  assert.equal(space.isHolding(), false);
  assert.equal(playing, true);
  assert.deepEqual(log, [
    { type: 'hold_start', restoreTo: 1.5 },
    { type: 'hold_end', restored: 1.5 },
  ]);
  log.length = 0;

  // Absent when chosen speed is already 2x (toggles immediately, never holds).
  speed = 2;
  space.onKeyDown({ repeat: false });
  clock.advance(400);
  assert.equal(space.isHolding(), false);
  space.onKeyUp();
  assert.deepEqual(log, [{ type: 'toggle', playing: false }]);
  space.dispose();
});

test('formatMiniPlayerDisplay shows title over artist and never invents Unknown artist', () => {
  assert.deepEqual(
    formatMiniPlayerDisplay({ title: 'Nocturne', artist: 'Clara', album: 'Evening' }, '01-nocturne.flac'),
    { title: 'Nocturne', artist: 'Clara' },
  );
  assert.deepEqual(
    formatMiniPlayerDisplay({ album: 'Evening Only' }, '02-untitled.mp3'),
    { title: '02-untitled', artist: '' },
  );
  assert.deepEqual(formatMiniPlayerDisplay(null, 'track.opus'), { title: 'track', artist: '' });
});

test('shuffle and repeat mode preferences cycle cleanly and persist across restarts', () => {
  assert.equal(cycleRepeatMode('off'), 'folder');
  assert.equal(cycleRepeatMode('folder'), 'one');
  assert.equal(cycleRepeatMode('one'), 'off');
  assert.equal(normalizeRepeatMode('bogus'), 'off');

  const store = new Map();
  const storage = {
    getItem: (k) => (store.has(k) ? store.get(k) : null),
    setItem: (k, v) => store.set(k, String(v)),
  };

  assert.deepEqual(readMusicModePreferences(storage), { shuffle: false, repeat: 'off' });
  writeMusicModePreferences(storage, { shuffle: true, repeat: 'folder' });
  assert.deepEqual(readMusicModePreferences(storage), { shuffle: true, repeat: 'folder' });
  assert.ok(store.has(MUSIC_MODES_STORAGE_KEY));

  writeMusicModePreferences(storage, { shuffle: false, repeat: 'one' });
  assert.deepEqual(readMusicModePreferences(storage), { shuffle: false, repeat: 'one' });
});

test('shuffle keeps current song first, preserves order while on, and Prev/Next follow shuffled order', () => {
  const songs = [
    { path: '/music/1.mp3', name: '1.mp3', kind: 'file' },
    { path: '/music/2.mp3', name: '2.mp3', kind: 'file' },
    { path: '/music/3.mp3', name: '3.mp3', kind: 'file' },
    { path: '/music/4.mp3', name: '4.mp3', kind: 'file' },
  ];

  // Deterministic RNG picks index 0 on every Fisher-Yates swap -> rotates rest predictably.
  const fixedRng = () => 0;
  const order = buildShuffledOrder(songs, '/music/2.mp3', fixedRng);
  assert.equal(order[0].path, '/music/2.mp3');
  assert.deepEqual(
    order.map((s) => s.path),
    ['/music/2.mp3', '/music/3.mp3', '/music/4.mp3', '/music/1.mp3'],
  );

  // Starting shuffle on track 2 keeps it at index 0 and persists shuffledPaths.
  const first = resolveMusicQueueState(songs, '/music/2.mp3', {
    shuffle: true,
    shuffledPaths: null,
    repeat: 'off',
    rng: fixedRng,
  });
  assert.equal(first.index, 0);
  assert.equal(first.prev, null);
  assert.equal(first.next?.path, '/music/3.mp3');
  assert.deepEqual(first.shuffledPaths, ['/music/2.mp3', '/music/3.mp3', '/music/4.mp3', '/music/1.mp3']);

  // Advancing to track 3 preserves the SAME shuffled order; Prev goes back to track 2, Next goes to track 4.
  const second = resolveMusicQueueState(songs, '/music/3.mp3', {
    shuffle: true,
    shuffledPaths: first.shuffledPaths,
    repeat: 'off',
    rng: () => 0.99,
  });
  assert.equal(second.index, 1);
  assert.equal(second.prev?.path, '/music/2.mp3');
  assert.equal(second.next?.path, '/music/4.mp3');
  assert.deepEqual(second.shuffledPaths, first.shuffledPaths);

  // Turning shuffle off restores folder order around the currently playing track.
  const unshuffled = resolveMusicQueueState(songs, '/music/3.mp3', {
    shuffle: false,
    shuffledPaths: first.shuffledPaths,
    repeat: 'off',
  });
  assert.equal(unshuffled.index, 2);
  assert.equal(unshuffled.prev?.path, '/music/2.mp3');
  assert.equal(unshuffled.next?.path, '/music/4.mp3');
  assert.equal(unshuffled.shuffledPaths, null);
});

test('repeat-folder wraps Next/Prev and track end; repeat-one repeats current song on end', () => {
  const songs = [
    { path: '/music/a.mp3', name: 'a.mp3', kind: 'file' },
    { path: '/music/b.mp3', name: 'b.mp3', kind: 'file' },
    { path: '/music/c.mp3', name: 'c.mp3', kind: 'file' },
  ];

  // Repeat off at end: no next, stops on end.
  const endOff = resolveMusicQueueState(songs, '/music/c.mp3', { repeat: 'off' });
  assert.equal(endOff.next, null);
  assert.deepEqual(resolveMusicEndedAction(endOff, 'off'), { action: 'stop', entry: null });

  // Repeat folder at end: wraps to first song on next and on track end.
  const endFolder = resolveMusicQueueState(songs, '/music/c.mp3', { repeat: 'folder' });
  assert.equal(endFolder.next?.path, '/music/a.mp3');
  assert.deepEqual(resolveMusicEndedAction(endFolder, 'folder'), {
    action: 'advance',
    entry: songs[0],
  });

  // Repeat folder at start: Prev wraps to last song (when <= 3s in), restarts when > 3s in.
  const startFolder = resolveMusicQueueState(songs, '/music/a.mp3', { repeat: 'folder' });
  assert.equal(startFolder.prev?.path, '/music/c.mp3');
  assert.deepEqual(resolveMusicPrevAction(startFolder, 1.0), { action: 'previous', entry: songs[2] });
  assert.deepEqual(resolveMusicPrevAction(startFolder, 5.0), { action: 'restart', entry: null });

  // Repeat one: natural end repeats the same song; explicit Next still steps when another exists.
  const midOne = resolveMusicQueueState(songs, '/music/b.mp3', { repeat: 'one' });
  assert.equal(midOne.next?.path, '/music/c.mp3');
  assert.deepEqual(resolveMusicEndedAction(midOne, 'one'), { action: 'repeat_current', entry: null });

  // Single-song folder: Next is absent, repeat-folder and repeat-one both repeat the sole song on end.
  const solo = [songs[0]];
  const soloFolder = resolveMusicQueueState(solo, '/music/a.mp3', { repeat: 'folder' });
  assert.equal(soloFolder.next, null);
  assert.equal(soloFolder.prev, null);
  assert.deepEqual(resolveMusicEndedAction(soloFolder, 'folder'), { action: 'repeat_current', entry: null });
  assert.deepEqual(resolveMusicEndedAction(soloFolder, 'off'), { action: 'stop', entry: null });
});

test('createPausedDismissTimer dismisses after 10 continuous paused minutes and cancels on resume', () => {
  const clock = createFakeClock();
  let dismissed = 0;
  const timer = createPausedDismissTimer({
    clock,
    onDismiss: () => {
      dismissed += 1;
    },
  });

  assert.equal(MUSIC_PAUSED_DISMISS_MS, 600_000);

  // Loaded & playing: no dismissal timer.
  timer.sync({ loaded: true, playing: true });
  assert.equal(timer.isPending(), false);
  clock.advance(MUSIC_PAUSED_DISMISS_MS * 2);
  assert.equal(dismissed, 0);

  // Paused for 9m59s, then resumed -> timer cancels, never dismisses.
  timer.sync({ loaded: true, playing: false });
  assert.equal(timer.isPending(), true);
  clock.advance(MUSIC_PAUSED_DISMISS_MS - 1000);
  assert.equal(dismissed, 0);
  timer.sync({ loaded: true, playing: true });
  assert.equal(timer.isPending(), false);
  clock.advance(5000);
  assert.equal(dismissed, 0);

  // Paused again for full 10 minutes -> dismisses exactly once at 600,000 ms.
  timer.sync({ loaded: true, playing: false });
  clock.advance(MUSIC_PAUSED_DISMISS_MS - 1);
  assert.equal(dismissed, 0);
  clock.advance(1);
  assert.equal(dismissed, 1);
  assert.equal(timer.isPending(), false);

  timer.dispose();
});
