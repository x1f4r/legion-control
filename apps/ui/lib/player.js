// Pure player logic: controlled-clock video overlay/gesture/stall/seek state machine,
// 200-file resume store, folder queue helpers, music metadata formatting, and key mapping.

import { baseFileName, entryCategory } from './files.js';

export const OVERLAY_HIDE_DELAY_MS = 3000;
export const DOUBLE_TAP_WINDOW_MS = 300;
export const STALL_SPINNER_DELAY_MS = 400;
export const TURN_SETTLE_MS = 200;
export const TURN_LEAVE_GUARD_MS = 600;
export const RESUME_MIN_DURATION_S = 600; // 10 minutes
export const RESUME_END_THRESHOLD = 0.95; // last 5%
export const RESUME_CHIP_DURATION_MS = 5000;
export const RESUME_SAVE_INTERVAL_MS = 10_000;
export const RESUME_MAX_ENTRIES = 200;
export const MUSIC_RESTART_THRESHOLD_S = 3;
export const SEEK_STEP_S = 10;
export const VOLUME_STEP = 0.1;
export const SEEK_FLASH_MS = 600;
export const PLAYBACK_SPEEDS = Object.freeze([0.5, 0.75, 1, 1.25, 1.5, 1.75, 2]);

export const RESUME_STORAGE_KEY = 'legion.player.resume.v1';

// ── Time & spoken value formatting ───────────────────────────────────────────────────────────

export function formatPlayerTime(seconds) {
  if (!Number.isFinite(seconds) || seconds < 0) return '0:00';
  const total = Math.floor(seconds);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  if (h > 0) {
    return `${h}:${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
  }
  return `${m}:${String(s).padStart(2, '0')}`;
}

export function formatRemainingTime(currentSeconds, durationSeconds) {
  if (!Number.isFinite(durationSeconds) || durationSeconds <= 0) return '';
  const cur = Number.isFinite(currentSeconds) && currentSeconds > 0 ? currentSeconds : 0;
  const remaining = Math.max(0, Math.ceil(durationSeconds - cur));
  return `\u2212${formatPlayerTime(remaining)}`;
}

export function formatSpokenDuration(seconds) {
  if (!Number.isFinite(seconds) || seconds <= 0) return '0 seconds';
  const total = Math.floor(seconds);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const parts = [];
  if (h > 0) parts.push(`${h} ${h === 1 ? 'hour' : 'hours'}`);
  if (m > 0) parts.push(`${m} ${m === 1 ? 'minute' : 'minutes'}`);
  if (s > 0 || parts.length === 0) parts.push(`${s} ${s === 1 ? 'second' : 'seconds'}`);
  return parts.join(' ');
}

export function formatPositionValueText(currentSeconds, durationSeconds) {
  const cur = formatSpokenDuration(currentSeconds);
  if (!Number.isFinite(durationSeconds) || durationSeconds <= 0) return cur;
  return `${cur} of ${formatSpokenDuration(durationSeconds)}`;
}

export function formatSpeed(rate) {
  if (!Number.isFinite(rate) || rate <= 0) return '1×';
  const rounded = Math.round(rate * 100) / 100;
  return `${rounded}×`;
}

// ── Music metadata formatting ────────────────────────────────────────────────────────────────

export function stripFileExtension(name) {
  const base = baseFileName(String(name || ''));
  const dot = base.lastIndexOf('.');
  if (dot <= 0 || dot === base.length - 1) return base;
  return base.slice(0, dot);
}

/**
 * Resolves the title and optional "artist · album" subtitle for the music screen.
 * Without tags, title is the file name without its ending and subtitle is empty.
 * Never produces placeholder text such as "Unknown artist".
 */
export function formatMusicDisplay(tags, fileName) {
  const cleanTitle = typeof tags?.title === 'string' ? tags.title.trim() : '';
  const cleanArtist = typeof tags?.artist === 'string' ? tags.artist.trim() : '';
  const cleanAlbum = typeof tags?.album === 'string' ? tags.album.trim() : '';
  const title = cleanTitle || stripFileExtension(fileName) || String(fileName || '');
  const subtitle = [cleanArtist, cleanAlbum].filter(Boolean).join(' · ');
  return { title, subtitle };
}

// ── Wire extractor for fs.tags ───────────────────────────────────────────────────────────────

/**
 * Extracts per-path entries from `fs.tags` (`{ results: [{ path, ok | error }] }`).
 */
export function extractTagsMap(res) {
  const map = Object.create(null);
  if (!res || typeof res !== 'object' || !Array.isArray(res.results)) return map;
  for (const item of res.results) {
    if (!item || typeof item.path !== 'string') continue;
    if (item.ok && typeof item.ok === 'object') {
      map[item.path] = { ok: item.ok };
    } else if (item.error) {
      map[item.path] = { error: item.error };
    }
  }
  return map;
}

// ── Folder queue helpers ─────────────────────────────────────────────────────────────────────

export function buildMediaQueue(siblings, category) {
  if (!Array.isArray(siblings) || !category) return [];
  return siblings.filter((entry) => entry && entry.kind !== 'dir' && entryCategory(entry) === category);
}

export function queueNeighbors(siblings, currentPath, category) {
  const items = buildMediaQueue(siblings, category);
  const index = items.findIndex((item) => item.path === currentPath);
  return {
    items,
    index,
    prev: index > 0 ? items[index - 1] : null,
    next: index >= 0 && index + 1 < items.length ? items[index + 1] : null,
    hasOther: items.length > 1,
  };
}

export function shouldMusicPrevRestart(currentTime) {
  return Number.isFinite(currentTime) && currentTime > MUSIC_RESTART_THRESHOLD_S;
}

export function preferredVideoOrientation(videoWidth, videoHeight) {
  if (Number.isFinite(videoWidth) && Number.isFinite(videoHeight) && videoWidth > 0 && videoHeight > 0) {
    return videoHeight > videoWidth ? 'portrait' : 'landscape';
  }
  return 'landscape';
}

// ── Resume store (last 200 files, keyed by machine, path, size and mtime) ────────────────────

export function resumeEntryKey({ machine, path, size, mtime } = {}) {
  const m = String(machine || '');
  const p = String(path || '');
  const s = Number.isFinite(size) ? String(size) : '';
  const t = mtime ? String(mtime) : '';
  return `${m}\0${p}\0${s}\0${t}`;
}

export function isResumeEligible(durationSeconds) {
  return Number.isFinite(durationSeconds) && durationSeconds >= RESUME_MIN_DURATION_S;
}

export function isNearEnd(positionSeconds, durationSeconds) {
  if (!Number.isFinite(durationSeconds) || durationSeconds <= 0) return false;
  if (!Number.isFinite(positionSeconds) || positionSeconds < 0) return false;
  return positionSeconds / durationSeconds >= RESUME_END_THRESHOLD;
}

export function computeResumePosition(savedPosition, durationSeconds) {
  if (!isResumeEligible(durationSeconds)) return null;
  if (!Number.isFinite(savedPosition) || savedPosition <= 1) return null;
  if (isNearEnd(savedPosition, durationSeconds)) return null;
  return Math.max(0, Math.min(durationSeconds, savedPosition));
}

export function createResumeStore(
  storage,
  { maxEntries = RESUME_MAX_ENTRIES, saveIntervalMs = RESUME_SAVE_INTERVAL_MS } = {},
) {
  let lastWriteKey = null;
  let lastWriteAt = -Infinity;

  function readAll() {
    if (!storage) return {};
    try {
      const raw = storage.getItem(RESUME_STORAGE_KEY);
      if (!raw) return {};
      const parsed = JSON.parse(raw);
      return parsed && typeof parsed === 'object' && !Array.isArray(parsed) ? parsed : {};
    } catch {
      return {};
    }
  }

  function writeAll(map) {
    if (!storage) return;
    try {
      storage.setItem(RESUME_STORAGE_KEY, JSON.stringify(map));
    } catch {
      // Ignore storage quota or private-mode failures.
    }
  }

  function prune(map) {
    const keys = Object.keys(map);
    if (keys.length <= maxEntries) return map;
    keys.sort((a, b) => (map[b]?.updatedAt || 0) - (map[a]?.updatedAt || 0));
    const trimmed = {};
    for (const k of keys.slice(0, maxEntries)) {
      trimmed[k] = map[k];
    }
    return trimmed;
  }

  function load(identity, durationSeconds) {
    if (!identity?.machine || !identity?.path) return null;
    const map = readAll();
    const record = map[resumeEntryKey(identity)];
    if (!record || typeof record.position !== 'number') return null;
    return computeResumePosition(record.position, durationSeconds);
  }

  function save(identity, positionSeconds, durationSeconds, nowMs = Date.now()) {
    if (!identity?.machine || !identity?.path) return false;
    if (!isResumeEligible(durationSeconds)) return false;
    const key = resumeEntryKey(identity);
    lastWriteKey = key;
    lastWriteAt = nowMs;
    const map = readAll();
    if (!Number.isFinite(positionSeconds) || positionSeconds <= 1 || isNearEnd(positionSeconds, durationSeconds)) {
      if (Object.prototype.hasOwnProperty.call(map, key)) {
        delete map[key];
        writeAll(map);
      }
      return false;
    }
    map[key] = {
      position: Math.round(positionSeconds * 10) / 10,
      updatedAt: nowMs,
    };
    writeAll(prune(map));
    return true;
  }

  function savePeriodic(identity, positionSeconds, durationSeconds, nowMs = Date.now()) {
    if (!identity?.machine || !identity?.path) return false;
    if (!isResumeEligible(durationSeconds)) return false;
    const key = resumeEntryKey(identity);
    if (lastWriteKey === key && nowMs - lastWriteAt < saveIntervalMs) {
      return false;
    }
    return save(identity, positionSeconds, durationSeconds, nowMs);
  }

  function clear(identity) {
    if (!identity?.machine || !identity?.path) return;
    const key = resumeEntryKey(identity);
    if (lastWriteKey === key) {
      lastWriteKey = null;
      lastWriteAt = -Infinity;
    }
    const map = readAll();
    if (Object.prototype.hasOwnProperty.call(map, key)) {
      delete map[key];
      writeAll(map);
    }
  }

  function renameMachine(oldName, newName) {
    const from = String(oldName || '').trim();
    const to = String(newName || '').trim();
    if (!from || !to || from === to) return false;
    const prefix = `${from}\0`;
    const map = readAll();
    let changed = false;
    const next = { ...map };
    for (const key of Object.keys(map)) {
      if (!key.startsWith(prefix)) continue;
      const destKey = `${to}\0${key.slice(prefix.length)}`;
      if (!Object.prototype.hasOwnProperty.call(next, destKey)) {
        next[destKey] = map[key];
      }
      delete next[key];
      changed = true;
    }
    if (lastWriteKey && lastWriteKey.startsWith(prefix)) {
      lastWriteKey = `${to}\0${lastWriteKey.slice(prefix.length)}`;
    }
    if (changed) {
      writeAll(prune(next));
    }
    return changed;
  }

  function count() {
    return Object.keys(readAll()).length;
  }

  return { load, save, savePeriodic, clear, renameMachine, count };
}

// ── Controlled-clock video overlay, tap/double-tap, pending-seek, and stall controller ───────

function defaultClock() {
  return {
    now: () => Date.now(),
    setTimeout: (fn, ms) => (typeof window !== 'undefined' ? window.setTimeout(fn, ms) : setTimeout(fn, ms)),
    clearTimeout: (id) => (typeof window !== 'undefined' ? window.clearTimeout(id) : clearTimeout(id)),
  };
}

/**
 * Classifies a horizontal position inside the video stage into `'left'`, `'center'`, or `'right'` thirds.
 */
export function tapZoneForPosition(clientX, rectLeft, rectWidth) {
  if (!Number.isFinite(rectWidth) || rectWidth <= 0) return 'center';
  const rel = (clientX - rectLeft) / rectWidth;
  if (rel < 1 / 3) return 'left';
  if (rel > 2 / 3) return 'right';
  return 'center';
}

export function createVideoController({
  clock = defaultClock(),
  onChange = () => {},
  onSeekDelta = () => {},
  onCommitSeek = () => {},
} = {}) {
  let state = {
    visible: true,
    cursorHidden: false,
    playing: false,
    loading: true,
    ended: false,
    error: false,
    focusInside: false,
    menuOpen: false,
    dragging: false,
    stalled: false,
    pendingSeekTime: null,
    resumeChip: null, // { position: number } | null
    seekFlash: null, // { direction: 'back' | 'forward', delta: number } | null
  };

  let hideTimer = null;
  let stallTimer = null;
  let chipTimer = null;
  let flashTimer = null;
  let lastTap = null; // { at, zone, layerBeforeFirstTap }
  let seekSeq = 0;

  function emit() {
    onChange({ ...state });
  }

  function clearHideTimer() {
    if (hideTimer != null) {
      clock.clearTimeout(hideTimer);
      hideTimer = null;
    }
  }

  function clearStallTimer() {
    if (stallTimer != null) {
      clock.clearTimeout(stallTimer);
      stallTimer = null;
    }
  }

  function clearChipTimer() {
    if (chipTimer != null) {
      clock.clearTimeout(chipTimer);
      chipTimer = null;
    }
  }

  function clearFlashTimer() {
    if (flashTimer != null) {
      clock.clearTimeout(flashTimer);
      flashTimer = null;
    }
  }

  function canAutoHide() {
    return (
      state.playing &&
      !state.loading &&
      !state.ended &&
      !state.error &&
      !state.focusInside &&
      !state.menuOpen &&
      !state.dragging &&
      !state.stalled &&
      state.pendingSeekTime == null
    );
  }

  function syncHideTimer() {
    clearHideTimer();
    if (state.visible && canAutoHide()) {
      hideTimer = clock.setTimeout(() => {
        hideTimer = null;
        if (!canAutoHide()) return;
        state = {
          ...state,
          visible: false,
          cursorHidden: true,
        };
        emit();
      }, OVERLAY_HIDE_DELAY_MS);
    }
  }

  function startStallCountdown() {
    if (state.stalled || stallTimer != null) return;
    stallTimer = clock.setTimeout(() => {
      stallTimer = null;
      if (!state.loading && state.pendingSeekTime == null) return;
      state = {
        ...state,
        stalled: true,
        visible: true,
        cursorHidden: false,
      };
      clearHideTimer();
      emit();
    }, STALL_SPINNER_DELAY_MS);
  }

  function stopStall() {
    clearStallTimer();
    if (state.stalled) {
      state = { ...state, stalled: false };
    }
  }

  function setPlaying(playing) {
    const nextPlaying = Boolean(playing);
    if (!nextPlaying) {
      // Pausing keeps controls visible and cancels any in-flight pending seek spinner/wait so
      // controls remain responsive on slow seeks.
      seekSeq += 1;
      clearStallTimer();
      state = {
        ...state,
        playing: false,
        visible: true,
        cursorHidden: false,
        stalled: false,
        loading: false,
        pendingSeekTime: null,
      };
      clearHideTimer();
      emit();
      return;
    }
    state = {
      ...state,
      playing: true,
      ended: false,
      error: false,
      cursorHidden: !state.visible,
    };
    syncHideTimer();
    emit();
  }

  function setLoading(loading) {
    const next = Boolean(loading);
    if (next) {
      state = {
        ...state,
        loading: true,
        visible: true,
        cursorHidden: false,
      };
      clearHideTimer();
      if (state.playing || state.pendingSeekTime != null) {
        startStallCountdown();
      }
      emit();
    } else {
      clearStallTimer();
      state = {
        ...state,
        loading: false,
        stalled: false,
      };
      syncHideTimer();
      emit();
    }
  }

  function onWaiting() {
    state = {
      ...state,
      loading: true,
    };
    clearHideTimer();
    startStallCountdown();
    emit();
  }

  function onCanPlay() {
    if (state.pendingSeekTime != null) return;
    clearStallTimer();
    state = {
      ...state,
      loading: false,
      stalled: false,
    };
    syncHideTimer();
    emit();
  }

  function setEnded(ended) {
    const next = Boolean(ended);
    clearStallTimer();
    if (next) {
      state = {
        ...state,
        playing: false,
        loading: false,
        stalled: false,
        pendingSeekTime: null,
        ended: true,
        visible: true,
        cursorHidden: false,
      };
      clearHideTimer();
    } else {
      state = { ...state, ended: false };
      syncHideTimer();
    }
    emit();
  }

  function setError(error) {
    clearStallTimer();
    state = {
      ...state,
      playing: false,
      loading: false,
      stalled: false,
      pendingSeekTime: null,
      error: Boolean(error),
      visible: true,
      cursorHidden: false,
    };
    if (error) clearHideTimer();
    else syncHideTimer();
    emit();
  }

  function setFocusInside(inside) {
    const next = Boolean(inside);
    if (state.focusInside === next) return;
    state = {
      ...state,
      focusInside: next,
      visible: next ? true : state.visible,
      cursorHidden: next ? false : state.cursorHidden,
    };
    syncHideTimer();
    emit();
  }

  function setMenuOpen(open) {
    const next = Boolean(open);
    if (state.menuOpen === next) return;
    state = {
      ...state,
      menuOpen: next,
      visible: next ? true : state.visible,
      cursorHidden: next ? false : state.cursorHidden,
    };
    syncHideTimer();
    emit();
  }

  function setDragging(dragging) {
    const next = Boolean(dragging);
    if (state.dragging === next) return;
    state = {
      ...state,
      dragging: next,
      visible: next ? true : state.visible,
      cursorHidden: next ? false : state.cursorHidden,
    };
    syncHideTimer();
    emit();
  }

  /**
   * Commits a seek on pointer release or skip action. The displayed time remains at `targetTime`
   * until `onSeeked(seq)` fires, cancelled by a newer seek, or cancelled by pause.
   */
  function beginSeek(targetTime) {
    seekSeq += 1;
    const seq = seekSeq;
    clearStallTimer();
    state = {
      ...state,
      dragging: false,
      ended: false,
      loading: true,
      stalled: false,
      pendingSeekTime: targetTime,
      visible: state.visible,
    };
    clearHideTimer();
    startStallCountdown();
    emit();
    onCommitSeek(targetTime, seq);
    return seq;
  }

  function onSeeked(seq = null) {
    if (seq != null && seq !== seekSeq) return;
    clearStallTimer();
    state = {
      ...state,
      loading: false,
      stalled: false,
      pendingSeekTime: null,
    };
    syncHideTimer();
    emit();
  }

  function notifyActivity({ show = true } = {}) {
    if (show) {
      state = {
        ...state,
        visible: true,
        cursorHidden: false,
      };
    }
    syncHideTimer();
    emit();
  }

  function flashSeek(direction, delta = SEEK_STEP_S) {
    clearFlashTimer();
    state = {
      ...state,
      seekFlash: { direction, delta },
    };
    emit();
    flashTimer = clock.setTimeout(() => {
      flashTimer = null;
      state = { ...state, seekFlash: null };
      emit();
    }, SEEK_FLASH_MS);
  }

  /**
   * Handles a tap on the video picture:
   * - Single tap toggles the overlay layer immediately.
   * - Second tap within 300 ms on the left or right third seeks -10s/+10s instead and puts
   *   the layer back as it was before the first tap.
   */
  function handlePictureTap(zone = 'center') {
    const now = clock.now();
    const isSide = zone === 'left' || zone === 'right';
    if (
      lastTap &&
      isSide &&
      lastTap.zone === zone &&
      now - lastTap.at <= DOUBLE_TAP_WINDOW_MS
    ) {
      const restoredVisible = lastTap.layerBeforeFirstTap;
      lastTap = {
        at: now,
        zone,
        layerBeforeFirstTap: restoredVisible,
      };
      const delta = zone === 'left' ? -SEEK_STEP_S : SEEK_STEP_S;
      state = {
        ...state,
        visible: restoredVisible,
        cursorHidden: !restoredVisible && state.playing,
      };
      syncHideTimer();
      flashSeek(zone === 'left' ? 'back' : 'forward', SEEK_STEP_S);
      onSeekDelta(delta);
      return { action: 'seek', delta };
    }

    const layerBeforeFirstTap = state.visible;
    const nextVisible = !state.visible;
    lastTap = {
      at: now,
      zone,
      layerBeforeFirstTap,
    };
    state = {
      ...state,
      visible: nextVisible,
      cursorHidden: !nextVisible && state.playing,
    };
    syncHideTimer();
    emit();
    return { action: 'toggle', visible: nextVisible };
  }

  function showResumeChip(position) {
    clearChipTimer();
    state = {
      ...state,
      resumeChip: { position },
    };
    emit();
    chipTimer = clock.setTimeout(() => {
      chipTimer = null;
      state = { ...state, resumeChip: null };
      emit();
    }, RESUME_CHIP_DURATION_MS);
  }

  function dismissResumeChip() {
    clearChipTimer();
    if (!state.resumeChip) return;
    state = { ...state, resumeChip: null };
    emit();
  }

  function getState() {
    return { ...state };
  }

  function dispose() {
    clearHideTimer();
    clearStallTimer();
    clearChipTimer();
    clearFlashTimer();
  }

  return {
    getState,
    setPlaying,
    setLoading,
    onWaiting,
    onCanPlay,
    setEnded,
    setError,
    setFocusInside,
    setMenuOpen,
    setDragging,
    beginSeek,
    onSeeked,
    notifyActivity,
    handlePictureTap,
    showResumeChip,
    dismissResumeChip,
    stopStall,
    dispose,
  };
}

// ── Keyboard shortcut classification (desktop and browser) ───────────────────────────────────

/**
 * Classifies a keydown event for the video or music player.
 * Returns null when the event should be left to the browser (e.g., modified keys, text inputs,
 * or Space/Enter on a focused button).
 */
export function classifyPlayerKey(
  event,
  { mode = 'video', isFullscreen = false, hasOpenOverlay = false } = {},
) {
  if (!event || event.ctrlKey || event.metaKey || event.altKey) return null;
  const key = String(event.key || '');
  const target = event.target;
  const tag = target?.tagName ? String(target.tagName).toUpperCase() : '';
  const inputType = tag === 'INPUT' ? String(target?.type || '').toLowerCase() : '';
  const isTextInput =
    tag === 'TEXTAREA' ||
    tag === 'SELECT' ||
    target?.isContentEditable === true ||
    (tag === 'INPUT' && inputType !== 'range' && inputType !== 'button' && inputType !== 'checkbox');

  if (isTextInput) return null;
  if (hasOpenOverlay) return null;

  if (key === 'Escape') {
    return isFullscreen ? 'exit_fullscreen' : 'close';
  }

  const isButtonFocus = tag === 'BUTTON' || tag === 'A';
  const isSliderFocus = tag === 'INPUT' && inputType === 'range' || target?.getAttribute?.('role') === 'slider';

  if (key === ' ' || key === 'Spacebar') {
    if (isButtonFocus) return null;
    return 'toggle_play';
  }
  const lower = key.toLowerCase();
  if (lower === 'k') return 'toggle_play';
  if (key === 'ArrowLeft') {
    if (isSliderFocus) return null;
    return 'seek_back';
  }
  if (key === 'ArrowRight') {
    if (isSliderFocus) return null;
    return 'seek_forward';
  }
  if (key === 'ArrowUp') {
    if (isSliderFocus) return null;
    return 'volume_up';
  }
  if (key === 'ArrowDown') {
    if (isSliderFocus) return null;
    return 'volume_down';
  }
  if (lower === 'm') return 'toggle_mute';
  if (lower === 'f' && mode === 'video') return 'toggle_fullscreen';
  if (lower === 'n') return 'next';
  if (lower === 'p') return 'prev';
  return null;
}

/**
 * Computes the next playback speed in `PLAYBACK_SPEEDS`.
 */
export function clampVolume(v) {
  if (!Number.isFinite(v)) return 1;
  return Math.max(0, Math.min(1, Math.round(v * 100) / 100));
}

// ── Viewer-level full-screen controller (single owner of immersive state and history entry) ──

/**
 * Single owner of full-screen / immersive state for a mounted viewer.
 *
 * Holds active state, who entered it (`'button' | 'rotation' | 'picture' | 'bars'`), and the
 * optional `{ legionFullscreen: true }` history entry. The player, picture viewer, and all sheets
 * or actions (Rename, Delete, Send to, Open in another app) ask this controller; nothing else in
 * the viewer calls `link.immersive`.
 */
export function createFullscreenController({
  link = null,
  history = typeof window !== 'undefined' ? window.history : null,
  getHash = () => (typeof window !== 'undefined' ? window.location.hash : ''),
  getElement = () => null,
  getVideo = () => null,
  onChange = () => {},
  now = () => Date.now(),
} = {}) {
  let state = {
    active: false,
    source: null,
    pushedHistory: false,
    inFlight: false,
  };
  let disposed = false;
  let historyToken = 0;
  let suppressNextPop = false;
  let activeViewerHash = getHash() || '';
  let pendingPopResolvers = [];
  let pendingPopTimer = null;
  let lastPlatformExitAt = -Infinity;

  function emit() {
    if (disposed) return;
    try {
      onChange({ ...state });
    } catch {
      // Keep controller state intact if a listener throws.
    }
  }

  function flushPopWaiters() {
    if (pendingPopTimer !== null) {
      clearTimeout(pendingPopTimer);
      pendingPopTimer = null;
    }
    const waiters = pendingPopResolvers;
    pendingPopResolvers = [];
    for (const resolve of waiters) {
      resolve();
    }
  }

  function syncUnderlyingHash() {
    if (!history || !activeViewerHash) return;
    const currentHash = getHash() || '';
    if (currentHash !== activeViewerHash) {
      try {
        history.replaceState(history.state, '', activeViewerHash);
      } catch {
        // Ignore restricted history APIs.
      }
    }
  }

  function popFullscreenHistory() {
    if (!history || typeof history.back !== 'function') {
      return Promise.resolve();
    }
    suppressNextPop = true;
    return new Promise((resolve) => {
      pendingPopResolvers.push(resolve);
      if (pendingPopTimer !== null) clearTimeout(pendingPopTimer);
      pendingPopTimer = setTimeout(() => {
        suppressNextPop = false;
        flushPopWaiters();
      }, 120);
      try {
        history.back();
      } catch {
        suppressNextPop = false;
        flushPopWaiters();
      }
    });
  }

  function noteViewerHash(hash) {
    if (typeof hash === 'string' && hash) {
      activeViewerHash = hash;
    }
  }

  async function enter(source = 'button', { orientation = 'none' } = {}) {
    if (disposed || typeof link?.immersive !== 'function') {
      return { active: false, mode: 'none' };
    }
    if (state.inFlight) {
      return { active: state.active, mode: 'in-flight' };
    }
    if (state.active && state.source === source && source !== 'button') {
      return { active: true, mode: 'existing' };
    }
    state = { ...state, inFlight: true };
    emit();

    let res = null;
    try {
      res = await link.immersive(true, {
        orientation,
        element: getElement?.() || null,
        video: getVideo?.() || null,
      });
    } catch {
      res = { active: false, mode: 'none' };
    }

    if (disposed) {
      if (res?.active !== false) {
        link.immersive(false, { element: getElement?.() || null, video: getVideo?.() || null }).catch?.(() => {});
      }
      return { active: false, mode: 'disposed' };
    }

    if (res && res.active === false) {
      state = { ...state, inFlight: false };
      emit();
      return { active: false, mode: res.mode || 'none' };
    }

    let pushedHistory = state.pushedHistory;
    if (source === 'button' && !pushedHistory && res?.mode !== 'webkit-video' && history) {
      try {
        historyToken += 1;
        const token = historyToken;
        const baseState = history.state && typeof history.state === 'object' ? history.state : {};
        history.replaceState({ ...baseState, legionViewerEntry: token }, '');
        history.pushState({ legionFullscreen: true, legionFullscreenToken: token }, '');
        pushedHistory = true;
      } catch {
        // Ignore restricted history APIs.
      }
    }

    state = {
      active: true,
      source,
      pushedHistory,
      inFlight: false,
    };
    lastPlatformExitAt = -Infinity;
    emit();
    return { active: true, mode: res?.mode || 'none' };
  }

  async function leave({ fromPopState = false } = {}) {
    const wasActive = state.active;
    const hadPushed = state.pushedHistory;
    state = {
      active: false,
      source: null,
      pushedHistory: false,
      inFlight: false,
    };
    emit();

    if (wasActive && typeof link?.immersive === 'function') {
      try {
        await link.immersive(false, {
          element: getElement?.() || null,
          video: getVideo?.() || null,
        });
      } catch {
        // Best-effort exit.
      }
    }

    if (hadPushed && !fromPopState) {
      await popFullscreenHistory();
    }
    return { active: false };
  }

  function syncFromPlatform(platformActive) {
    if (disposed) return;
    if (!platformActive && state.active) {
      const hadPushed = state.pushedHistory;
      lastPlatformExitAt = now();
      state = {
        active: false,
        source: null,
        pushedHistory: false,
        inFlight: false,
      };
      emit();
      if (hadPushed) {
        popFullscreenHistory();
      }
    }
  }

  function recentlyExitedFromPlatform(windowMs = 250) {
    return now() - lastPlatformExitAt < windowMs;
  }

  function handlePopState() {
    const st = history?.state;
    const landedOnViewerEntry = Boolean(
      historyToken && st && typeof st === 'object' && st.legionViewerEntry === historyToken,
    );
    if (suppressNextPop) {
      suppressNextPop = false;
      if (landedOnViewerEntry) {
        syncUnderlyingHash();
      }
      flushPopWaiters();
      return landedOnViewerEntry;
    }
    if (state.pushedHistory && !st?.legionFullscreen && landedOnViewerEntry) {
      syncUnderlyingHash();
      leave({ fromPopState: true });
      return true;
    }
    return false;
  }

  const unsubPlatform =
    typeof link?.onImmersiveChange === 'function'
      ? link.onImmersiveChange((active) => syncFromPlatform(active))
      : () => {};

  function getState() {
    return { ...state };
  }

  function dispose() {
    if (disposed) return;
    disposed = true;
    unsubPlatform();
    flushPopWaiters();
    const wasActive = state.active;
    state = { active: false, source: null, pushedHistory: false, inFlight: false };
    if (wasActive && typeof link?.immersive === 'function') {
      try {
        const res = link.immersive(false, {
          element: getElement?.() || null,
          video: getVideo?.() || null,
        });
        if (res && typeof res.catch === 'function') res.catch(() => {});
      } catch {
        // Best-effort cleanup on unmount.
      }
    }
  }

  return {
    getState,
    noteViewerHash,
    enter,
    leave,
    syncFromPlatform,
    recentlyExitedFromPlatform,
    handlePopState,
    dispose,
  };
}

// ── Orientation turn watcher (injectable clock, orientation-event first, in-flight guard) ───

/**
 * Watches device turns between portrait and landscape to enter/leave rotation full screen.
 *
 * Uses orientation change events where the platform provides them (ignoring plain `resize`
 * events such as soft keyboards or address bars), applies a short settle debounce and post-leave
 * guard on an injectable clock (never cleared early by a transient portrait reading), and never
 * starts a second entry while an `orientation.lock` / enter operation is already in flight.
 */
export function createTurnWatcher({
  clock = defaultClock(),
  settleMs = TURN_SETTLE_MS,
  leaveGuardMs = TURN_LEAVE_GUARD_MS,
  hasOrientationEvent = false,
  isLandscape = () => false,
  isFullscreen = () => false,
  getFullscreenSource = () => null,
  isInFlight = () => false,
  onEnterRotation = () => {},
  onLeaveRotation = () => {},
} = {}) {
  let disposed = false;
  let wasLandscape = false;
  let guardUntil = 0;
  let settleTimer = null;
  let pendingOp = false;
  let recheckAfterOp = false;

  function clearSettleTimer() {
    if (settleTimer != null) {
      clock.clearTimeout(settleTimer);
      settleTimer = null;
    }
  }

  function afterOpSettled() {
    if (disposed || pendingOp || isInFlight()) return;
    if (recheckAfterOp) {
      recheckAfterOp = false;
      evaluate(false);
      return;
    }
    if (settleTimer == null && !Boolean(isLandscape()) && isFullscreen() && getFullscreenSource() === 'rotation') {
      evaluate(false);
    }
  }

  function runOp(fn) {
    pendingOp = true;
    let res;
    try {
      res = fn();
    } catch {
      pendingOp = false;
      afterOpSettled();
      return;
    }
    if (res && typeof res.then === 'function') {
      res.then(
        () => {
          pendingOp = false;
          afterOpSettled();
        },
        () => {
          pendingOp = false;
          afterOpSettled();
        },
      );
    } else {
      pendingOp = false;
      afterOpSettled();
    }
  }

  function evaluate(initial = false) {
    if (disposed) return;
    if (pendingOp || isInFlight()) {
      recheckAfterOp = true;
      return;
    }
    const landscape = Boolean(isLandscape());
    const turnedToLandscape = initial ? landscape : !wasLandscape && landscape;
    wasLandscape = landscape;

    if (!initial && clock.now() < guardUntil) {
      return;
    }

    if (turnedToLandscape && !isFullscreen()) {
      runOp(onEnterRotation);
    } else if (!landscape && isFullscreen() && getFullscreenSource() === 'rotation') {
      runOp(onLeaveRotation);
    }
  }

  function scheduleEvaluation() {
    if (disposed) return;
    clearSettleTimer();
    settleTimer = clock.setTimeout(() => {
      settleTimer = null;
      evaluate(false);
    }, settleMs);
  }

  function start() {
    evaluate(true);
  }

  function onOrientationEvent() {
    scheduleEvaluation();
  }

  function onResize() {
    if (hasOrientationEvent) return;
    scheduleEvaluation();
  }

  function notifySettled() {
    afterOpSettled();
  }

  function noteLeave() {
    if (disposed) return;
    clearSettleTimer();
    recheckAfterOp = false;
    guardUntil = clock.now() + leaveGuardMs;
  }

  function dispose() {
    disposed = true;
    recheckAfterOp = false;
    clearSettleTimer();
  }

  return {
    start,
    onOrientationEvent,
    onResize,
    notifySettled,
    noteLeave,
    dispose,
  };
}
