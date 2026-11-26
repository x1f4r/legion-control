// Single app-wide music playback controller.
// Owns the loaded track, folder queue, shuffle/repeat modes, HTML5 <audio> (desktop/browser/iOS)
// or native Android Media3 backend, MediaSession, 10-minute paused auto-dismiss, tags/cover
// fetching, resume store, and desktop window-hide pausing. Leaving the song's screen never stops
// or unloads playback; dismissing the small player stops playback and revokes the stream.

import { baseFileName, extractThumbMap, mimeFromName } from './files.js';
import {
  buildMediaQueue,
  clampVolume,
  createPausedDismissTimer,
  createResumeStore,
  cycleRepeatMode,
  extractTagsMap,
  formatMusicDisplay,
  MUSIC_PAUSED_DISMISS_MS,
  normalizeRepeatMode,
  readMusicModePreferences,
  resolveMusicEndedAction,
  resolveMusicPrevAction,
  resolveMusicQueueState,
  RESUME_CHIP_DURATION_MS,
  STALL_SPINNER_DELAY_MS,
  writeMusicModePreferences,
} from './player.js';
import { thumbCacheKey } from './thumb-cache.js';

const MEDIA_RECOVERY_WINDOW_MS = 10_000;
const SEEK_STEP_S = 10;

function safeStorage() {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

function isLocalMediaUrl(url) {
  return typeof url === 'string' && (url.startsWith('data:') || url.startsWith('blob:'));
}

function defaultClock() {
  if (typeof window !== 'undefined' && window.__LEGION_TEST_PLAYER_CLOCK__) {
    return window.__LEGION_TEST_PLAYER_CLOCK__;
  }
  return {
    now: () => Date.now(),
    setTimeout: (fn, ms) => globalThis.setTimeout(fn, ms),
    clearTimeout: (id) => globalThis.clearTimeout(id),
  };
}

function contentTypeMatchesAudio(contentType) {
  if (typeof contentType !== 'string' || !contentType.trim()) return false;
  const base = contentType.split(';')[0].trim().toLowerCase();
  return base.startsWith('audio/') || base === 'application/ogg';
}

function classifyMintError(err) {
  if (err?.details?.reason === 'too_large_for_inline' || err?.status === 413) {
    return {
      code: 'too_large',
      status: err?.status || 413,
      message: err?.message || '',
      hint: err?.hint || '',
      details: err?.details || null,
      machine: err?.machine || null,
    };
  }
  if (err?.code === 'unsupported' || err?.code === 'needs_newer_legion') {
    return {
      code: err.code,
      message: err.message || '',
      hint: err.hint || '',
      details: err.details || null,
      machine: err.machine || null,
    };
  }
  if (err?.code === 'denied') {
    return {
      code: 'denied',
      message: err.message || "You're not allowed to open this file.",
      hint: err.hint || '',
      details: err.details || null,
      machine: err.machine || null,
    };
  }
  if (err?.code === 'not_found') {
    return {
      code: 'not_found',
      message: err.message || '',
      hint: err.hint || '',
      details: err.details || null,
      machine: err.machine || null,
    };
  }
  return {
    code: err?.code === 'timeout' ? 'timeout' : 'unreachable',
    message: err?.message || '',
    hint: err?.hint || '',
    details: err?.details || null,
    machine: err?.machine || null,
  };
}

function readBufferedEnd(el) {
  if (!el || !el.buffered || el.buffered.length === 0) return 0;
  try {
    const t = Number.isFinite(el.currentTime) ? el.currentTime : 0;
    for (let i = 0; i < el.buffered.length; i += 1) {
      const start = el.buffered.start(i);
      const end = el.buffered.end(i);
      if (t >= start - 0.5 && t <= end + 0.5) return end;
    }
    return el.buffered.end(el.buffered.length - 1);
  } catch {
    return 0;
  }
}

function makeEmptyQueue() {
  return {
    items: [],
    index: -1,
    prev: null,
    next: null,
    hasOther: false,
    shuffledPaths: null,
  };
}

export function createMusicController({
  link,
  capsStore = null,
  thumbCache = null,
  getClock = defaultClock,
  pausedDismissMs = MUSIC_PAUSED_DISMISS_MS,
  storage = null,
} = {}) {
  const store = storage || safeStorage();
  const resumeStore = createResumeStore(store);
  const initialModes = readMusicModePreferences(store);
  const nativeAudio = link?.nativeAudio && typeof link.nativeAudio === 'object' ? link.nativeAudio : null;
  const iosAudio = link?.iosAudio && typeof link.iosAudio === 'object' ? link.iosAudio : null;
  const usingNative = Boolean(nativeAudio);

  let state = {
    loaded: false,
    machine: null,
    path: null,
    entry: null,
    name: '',
    siblings: [],
    caps: null,
    queue: makeEmptyQueue(),
    shuffle: initialModes.shuffle,
    repeat: initialModes.repeat,
    tags: null,
    coverUrl: null,
    mediaUrl: null,
    mediaLoading: false,
    mediaError: null,
    playing: false,
    currentTime: 0,
    duration: 0,
    bufferedEnd: 0,
    pendingSeekTime: null,
    stalled: false,
    playbackRate: 1,
    volume: 1,
    muted: false,
    resumeChip: null,
    usingNative,
  };

  const listeners = new Set();
  let audioEl = null;
  let loadAbort = null;
  let metaAbort = null;
  let mintedUrl = null;
  let mintInFlightKey = null;
  let autoPlayTarget = null; // { path, play: true } | null
  let pendingResume = null; // { key, url, time, play } | null
  let playbackTrack = { key: '', time: 0, wantPlay: false };
  let recovery = { key: '', lastAttemptAt: 0, inFlight: false };
  let windowHiddenPause = null;
  let resumedAppliedKey = '';
  let stallTimer = null;
  let chipTimer = null;
  let dismissed = false;
  let unsubWindowHidden = null;
  let unsubNativeState = null;
  let unsubIosRemote = null;

  function activeClock() {
    return typeof getClock === 'function' ? getClock() : defaultClock();
  }

  function fileKeyOf(machine, path) {
    return `${machine || ''}\0${path || ''}`;
  }

  function identityOf(st = state) {
    if (!st.loaded || !st.machine || !st.path) return null;
    return {
      machine: st.machine,
      path: st.path,
      size: st.entry?.size,
      mtime: st.entry?.mtime,
    };
  }

  function emit() {
    for (const fn of listeners) {
      try {
        fn(state);
      } catch {
        // Keep notifying remaining listeners.
      }
    }
  }

  function clearStallTimer() {
    if (stallTimer != null) {
      activeClock().clearTimeout(stallTimer);
      stallTimer = null;
    }
    if (state.stalled) {
      state = { ...state, stalled: false };
    }
  }

  function startStallTimer() {
    if (stallTimer != null) {
      activeClock().clearTimeout(stallTimer);
    }
    stallTimer = activeClock().setTimeout(() => {
      stallTimer = null;
      if (state.loaded && !state.stalled) {
        state = { ...state, stalled: true };
        emit();
      }
    }, STALL_SPINNER_DELAY_MS);
  }

  function clearChipTimer() {
    if (chipTimer != null) {
      activeClock().clearTimeout(chipTimer);
      chipTimer = null;
    }
  }

  // Paused auto-dismiss timer: delegates through a dynamic clock adapter so browser tests can
  // install `window.__LEGION_TEST_PLAYER_CLOCK__` before pausing.
  let pauseDismissClock = null;
  const pausedDismissTimer = createPausedDismissTimer({
    clock: {
      setTimeout: (fn, ms) => {
        const clk = activeClock();
        pauseDismissClock = clk;
        return clk.setTimeout(fn, ms);
      },
      clearTimeout: (id) => {
        const clk = pauseDismissClock || activeClock();
        pauseDismissClock = null;
        clk.clearTimeout(id);
      },
    },
    delayMs: pausedDismissMs,
    onDismiss: () => {
      pauseDismissClock = null;
      if (state.loaded && !state.playing) {
        dismiss();
      }
    },
  });

  function cancelPausedDismiss() {
    pausedDismissTimer.cancel();
  }

  function syncPausedDismiss() {
    if (usingNative) {
      // Android native service owns the 10-minute paused auto-dismiss so it works even while
      // the WebView is asleep in the background.
      pausedDismissTimer.cancel();
      return;
    }
    pausedDismissTimer.sync({
      loaded: state.loaded && !state.mediaLoading,
      playing: state.playing,
    });
  }

  function setState(patch, { syncMedia = true } = {}) {
    const prevLoaded = state.loaded;
    const prevPlaying = state.playing;
    const prevPath = state.path;
    const prevMachine = state.machine;
    const prevTitle = state.tags?.title;
    const prevArtist = state.tags?.artist;
    const prevAlbum = state.tags?.album;
    const prevCover = state.coverUrl;
    const prevNext = state.queue?.next?.path || null;
    const prevPrev = state.queue?.prev?.path || null;
    const prevRate = state.playbackRate;
    const prevDuration = state.duration;
    const prevTime = state.currentTime;

    state = { ...state, ...patch };
    syncPausedDismiss();

    if (syncMedia && !usingNative) {
      const metaChanged =
        prevLoaded !== state.loaded ||
        prevPath !== state.path ||
        prevMachine !== state.machine ||
        prevTitle !== state.tags?.title ||
        prevArtist !== state.tags?.artist ||
        prevAlbum !== state.tags?.album ||
        prevCover !== state.coverUrl ||
        prevNext !== (state.queue?.next?.path || null) ||
        prevPrev !== (state.queue?.prev?.path || null);
      const posChanged =
        prevLoaded !== state.loaded ||
        prevPlaying !== state.playing ||
        prevRate !== state.playbackRate ||
        prevDuration !== state.duration ||
        Math.abs(prevTime - state.currentTime) >= 0.4;
      if (metaChanged) {
        syncPlatformMediaMetadata();
      }
      if (metaChanged || posChanged) {
        syncPlatformMediaPosition();
      }
    }
    emit();
  }

  function revokeContentUrlNow(url) {
    if (!url || typeof link?.revokeContentUrl !== 'function') return;
    if (mintedUrl === url) mintedUrl = null;
    try {
      const res = link.revokeContentUrl(url);
      if (res && typeof res.catch === 'function') res.catch(() => {});
    } catch {
      // Best-effort ticket cleanup.
    }
  }

  function revokeActiveMintedUrl() {
    const url = mintedUrl;
    if (url) {
      mintedUrl = null;
      revokeContentUrlNow(url);
    }
  }

  function clearAudioElement() {
    if (!audioEl) return;
    try {
      audioEl.pause();
      audioEl.removeAttribute('src');
      audioEl.load();
    } catch {
      // Ignore teardown errors.
    }
  }

  function clearPlatformMediaSession() {
    const ms = typeof navigator !== 'undefined' ? navigator.mediaSession : null;
    if (ms) {
      for (const action of [
        'play',
        'pause',
        'previoustrack',
        'nexttrack',
        'seekto',
        'seekbackward',
        'seekforward',
      ]) {
        try {
          ms.setActionHandler(action, null);
        } catch {
          // Ignore unsupported action names.
        }
      }
      try {
        ms.metadata = null;
      } catch {
        // Ignore browsers that reject null metadata.
      }
      try {
        ms.playbackState = 'none';
      } catch {
        // Ignore playbackState errors.
      }
    }
    iosAudio?.clearNowPlaying?.();
  }

  function syncPlatformMediaMetadata() {
    if (!state.loaded) {
      clearPlatformMediaSession();
      return;
    }
    const display = formatMusicDisplay(state.tags, state.name);
    const ms = typeof navigator !== 'undefined' ? navigator.mediaSession : null;
    if (ms) {
      try {
        if (typeof window !== 'undefined' && typeof window.MediaMetadata === 'function') {
          ms.metadata = new window.MediaMetadata({
            title: display.title,
            artist: state.tags?.artist || '',
            album: state.tags?.album || '',
            artwork: state.coverUrl ? [{ src: state.coverUrl }] : [],
          });
        }
      } catch {
        // Ignore MediaMetadata errors on unsupported platforms.
      }
      const safeSet = (action, handler) => {
        try {
          ms.setActionHandler(action, handler);
        } catch {
          // Some browsers do not support every action name.
        }
      };
      safeSet('play', () => {
        if (!state.playing) togglePlay();
      });
      safeSet('pause', () => pause());
      safeSet('previoustrack', () => prev());
      safeSet('nexttrack', state.queue.next ? () => next() : null);
      safeSet('seekto', (details) => {
        if (typeof details?.seekTime === 'number' && Number.isFinite(details.seekTime)) {
          seek(details.seekTime);
        }
      });
      safeSet('seekbackward', (details) => {
        const offset =
          typeof details?.seekOffset === 'number' && details.seekOffset > 0 ? details.seekOffset : SEEK_STEP_S;
        seek(Math.max(0, state.currentTime - offset));
      });
      safeSet('seekforward', (details) => {
        const offset =
          typeof details?.seekOffset === 'number' && details.seekOffset > 0 ? details.seekOffset : SEEK_STEP_S;
        seek(state.currentTime + offset);
      });
    }

    iosAudio?.updateNowPlaying?.({
      title: display.title,
      artist: state.tags?.artist || '',
      album: state.tags?.album || '',
      artworkDataUrl: state.coverUrl || null,
      duration: state.duration > 0 ? state.duration : 0,
      elapsed: state.currentTime >= 0 ? state.currentTime : 0,
      playing: state.playing,
      rate: state.playing ? state.playbackRate : 0,
      canNext: Boolean(state.queue.next),
      canPrev: true,
    });
  }

  function syncPlatformMediaPosition() {
    if (!state.loaded) return;
    const ms = typeof navigator !== 'undefined' ? navigator.mediaSession : null;
    if (ms) {
      try {
        ms.playbackState = state.playing ? 'playing' : 'paused';
        if (typeof ms.setPositionState === 'function' && Number.isFinite(state.duration) && state.duration > 0) {
          ms.setPositionState({
            duration: state.duration,
            playbackRate: state.playbackRate,
            position: Math.max(0, Math.min(state.duration, state.currentTime)),
          });
        }
      } catch {
        // Ignore position state clamping errors.
      }
    }
    iosAudio?.updatePlaybackState?.({
      duration: state.duration > 0 ? state.duration : 0,
      elapsed: state.currentTime >= 0 ? state.currentTime : 0,
      playing: state.playing,
      rate: state.playing ? state.playbackRate : 0,
      canNext: Boolean(state.queue.next),
      canPrev: true,
    });
  }

  function cachedCoverFor(machine, path, entry) {
    if (!thumbCache) return null;
    const largeHit = thumbCache.get(thumbCacheKey(machine, path, entry?.mtime || '', 1024));
    if (largeHit && !largeHit.failed) return largeHit.url;
    const smallHit = thumbCache.get(thumbCacheKey(machine, path, entry?.mtime || '', 128));
    if (smallHit && !smallHit.failed) return smallHit.url;
    return null;
  }

  function loadTrackMetadata(machine, path, entry, caps) {
    if (metaAbort) metaAbort.abort();
    const ctrl = new AbortController();
    metaAbort = ctrl;
    const targetKey = fileKeyOf(machine, path);

    const runCall = (tool, args) =>
      capsStore
        ? capsStore.call(tool, args, { on: machine, signal: ctrl.signal }).then((r) => r.value)
        : link.call(tool, args, { on: machine, signal: ctrl.signal });

    const loadCover = async () => {
      if (!caps?.hasAudioThumb) return;
      const reqSize = Math.min(1024, caps?.thumbMaxSize || 512);
      const cacheKey = thumbCacheKey(machine, path, entry?.mtime || '', reqSize);
      const cached = thumbCache?.get(cacheKey);
      if (cached) {
        if (!ctrl.signal.aborted && fileKeyOf(state.machine, state.path) === targetKey && !cached.failed) {
          setState({ coverUrl: cached.url });
        }
        return;
      }
      try {
        const res = await runCall('fs.thumb', { paths: [path], size: reqSize, fit: 'contain' });
        if (ctrl.signal.aborted || fileKeyOf(state.machine, state.path) !== targetKey) return;
        const map = extractThumbMap(res);
        const item = map[path];
        if (item?.content && item?.mime) {
          const url = `data:${item.mime};base64,${item.content}`;
          thumbCache?.set(cacheKey, { url });
          setState({ coverUrl: url });
        } else {
          thumbCache?.set(cacheKey, { failed: true });
        }
      } catch {
        // Older machines or untagged songs stay on the music tile quietly.
      }
    };

    (async () => {
      let hasCoverHint = null;
      if (caps?.hasTags) {
        try {
          const res = await runCall('fs.tags', { paths: [path] });
          if (ctrl.signal.aborted || fileKeyOf(state.machine, state.path) !== targetKey) return;
          const map = extractTagsMap(res);
          const item = map[path];
          if (item?.ok) {
            const patch = { tags: item.ok };
            if (typeof item.ok.duration_s === 'number' && item.ok.duration_s > 0 && state.duration <= 0) {
              patch.duration = item.ok.duration_s;
            }
            setState(patch);
            hasCoverHint = Boolean(item.ok.cover);
          }
        } catch {
          // Silently show filename when tags are unavailable.
        }
      }
      if (ctrl.signal.aborted || fileKeyOf(state.machine, state.path) !== targetKey) return;
      if (hasCoverHint !== false) {
        await loadCover();
      }
    })();
  }

  function mintTrackUrl(machine, path, caps) {
    if (loadAbort) loadAbort.abort();
    const ctrl = new AbortController();
    loadAbort = ctrl;
    const targetKey = fileKeyOf(machine, path);

    if (caps && caps.known === false) {
      mintInFlightKey = null;
      setState({ mediaUrl: null, mediaLoading: true, mediaError: null });
      return;
    }

    if (!caps?.hasContentUrl || typeof link?.contentUrl !== 'function') {
      mintInFlightKey = null;
      setState({
        mediaUrl: null,
        mediaLoading: false,
        mediaError: { code: 'unsupported', message: '' },
      });
      return;
    }

    mintInFlightKey = targetKey;
    setState({ mediaUrl: null, mediaLoading: true, mediaError: null });
    link
      .contentUrl({ machine, path, signal: ctrl.signal })
      .then((res) => {
        if (mintInFlightKey === targetKey) mintInFlightKey = null;
        if (ctrl.signal.aborted || fileKeyOf(state.machine, state.path) !== targetKey) {
          if (res?.url) revokeContentUrlNow(res.url);
          return;
        }
        if (!res?.url) {
          setState({
            mediaUrl: null,
            mediaLoading: false,
            mediaError: { code: 'unreachable', message: 'Preview is not available right now.' },
          });
          return;
        }
        revokeActiveMintedUrl();
        mintedUrl = res.url;
        setState({ mediaUrl: res.url, mediaLoading: false, mediaError: null });
        syncAudioElementSrc(res.url);
      })
      .catch((err) => {
        if (mintInFlightKey === targetKey) mintInFlightKey = null;
        if (ctrl.signal.aborted || fileKeyOf(state.machine, state.path) !== targetKey) return;
        setState({
          mediaUrl: null,
          mediaLoading: false,
          mediaError: classifyMintError(err),
        });
      });
  }

  function syncAudioElementSrc(url) {
    if (usingNative || !audioEl) return;
    if (url) {
      if (audioEl.getAttribute('src') !== url) {
        audioEl.setAttribute('src', url);
      }
      audioEl.playbackRate = state.playbackRate;
      audioEl.volume = state.volume;
      audioEl.muted = state.muted;
    } else if (audioEl.hasAttribute('src')) {
      clearAudioElement();
    }
  }

  function saveCurrentResume() {
    const id = identityOf(state);
    if (!id) return;
    const dur =
      !usingNative && audioEl && Number.isFinite(audioEl.duration) && audioEl.duration > 0
        ? audioEl.duration
        : state.duration;
    const pos =
      !usingNative && audioEl && Number.isFinite(audioEl.currentTime)
        ? audioEl.currentTime
        : state.currentTime;
    if (Number.isFinite(dur) && dur > 0) {
      resumeStore.save(id, pos, dur);
    }
  }

  function openTrack({
    machine,
    path,
    entry,
    siblings = [],
    caps = null,
    autoPlay = null,
    preserveShuffleOrder = false,
  } = {}) {
    if (!machine || !path) return;
    const name = entry?.name || baseFileName(path);
    const effectiveEntry = entry || { name, path, kind: 'file' };
    const audioSiblings = buildMediaQueue(siblings, 'audio');
    const folderTracks =
      audioSiblings.length > 0 && audioSiblings.some((s) => s.path === path)
        ? audioSiblings
        : [effectiveEntry];
    const targetKey = fileKeyOf(machine, path);
    const sameTrack =
      state.loaded && state.machine === machine && state.path === path && !state.mediaError;

    // Reopening the exact same already-loaded track keeps its stream, position, and play state!
    if (sameTrack) {
      const nextQueue = resolveMusicQueueState(folderTracks, path, {
        shuffle: state.shuffle,
        shuffledPaths: state.queue.shuffledPaths,
        repeat: state.repeat,
      });
      const mergedCaps = caps || state.caps;
      const nextCapsReady = Boolean(mergedCaps?.known !== false && mergedCaps?.hasContentUrl);
      const patch = {
        entry: effectiveEntry,
        name,
        siblings: folderTracks,
        caps: mergedCaps,
        queue: nextQueue,
      };
      setState(patch);
      if (!usingNative && !state.mediaUrl && nextCapsReady && mintInFlightKey !== targetKey) {
        mintTrackUrl(machine, path, mergedCaps);
      }
      if (!state.tags && mergedCaps?.hasTags) {
        loadTrackMetadata(machine, path, effectiveEntry, mergedCaps);
      }
      return;
    }

    // Switching from another loaded song: save its resume position, stop & revoke it cleanly.
    const wasLoaded = state.loaded;
    const wasPlaying = state.playing;
    if (wasLoaded) {
      saveCurrentResume();
      clearStallTimer();
      clearChipTimer();
      if (!usingNative) {
        if (loadAbort) loadAbort.abort();
        if (metaAbort) metaAbort.abort();
        clearAudioElement();
        revokeActiveMintedUrl();
      }
    }

    const shouldAutoPlay =
      typeof autoPlay === 'boolean' ? autoPlay : Boolean(wasLoaded && wasPlaying);
    const sameFolder =
      preserveShuffleOrder &&
      state.loaded &&
      state.machine === machine &&
      Array.isArray(state.queue.shuffledPaths) &&
      state.queue.shuffledPaths.includes(path);

    const nextQueue = resolveMusicQueueState(folderTracks, path, {
      shuffle: state.shuffle,
      shuffledPaths: sameFolder ? state.queue.shuffledPaths : null,
      repeat: state.repeat,
    });

    playbackTrack = { key: targetKey, time: 0, wantPlay: shouldAutoPlay };
    pendingResume = null;
    recovery = { key: targetKey, lastAttemptAt: 0, inFlight: false };
    windowHiddenPause = null;
    resumedAppliedKey = '';
    autoPlayTarget = shouldAutoPlay ? { path, play: true } : null;

    const initialCover = cachedCoverFor(machine, path, effectiveEntry);
    const initialDuration = effectiveEntry?.duration_s > 0 ? effectiveEntry.duration_s : 0;

    setState({
      loaded: true,
      machine,
      path,
      entry: effectiveEntry,
      name,
      siblings: folderTracks,
      caps: caps || state.caps,
      queue: nextQueue,
      tags: null,
      coverUrl: initialCover,
      mediaUrl: null,
      mediaLoading: true,
      mediaError: null,
      playing: shouldAutoPlay,
      currentTime: 0,
      duration: initialDuration,
      bufferedEnd: 0,
      pendingSeekTime: null,
      stalled: false,
      resumeChip: null,
    });

    loadTrackMetadata(machine, path, effectiveEntry, caps || state.caps);

    if (usingNative) {
      // On Android, Media3 MediaSessionService owns the queue, fresh URL minting, cover updates,
      // lock-screen controls, audio focus, becoming-noisy, and 10-minute paused auto-dismiss.
      nativeAudio
        .loadQueue({
          machine,
          tracks: nextQueue.items.map((item) => ({
            path: item.path,
            name: item.name || baseFileName(item.path),
            size: item.size ?? null,
            mtime: item.mtime ?? null,
            mime: item.mime || mimeFromName(item.name || item.path),
            duration_s: item.duration_s ?? null,
          })),
          startIndex: Math.max(0, nextQueue.index),
          shuffle: state.shuffle,
          repeat: state.repeat,
          playbackRate: state.playbackRate,
          autoPlay: shouldAutoPlay,
        })
        .catch((err) => {
          if (fileKeyOf(state.machine, state.path) !== targetKey) return;
          setState({
            mediaLoading: false,
            playing: false,
            mediaError: classifyMintError(err),
          });
        });
      return;
    }

    mintTrackUrl(machine, path, caps || state.caps);
  }

  function applyNativeState(raw) {
    if (!raw || typeof raw !== 'object') return;
    if (raw.loaded === false) {
      clearStallTimer();
      clearChipTimer();
      setState(
        {
          loaded: false,
          machine: null,
          path: null,
          entry: null,
          name: '',
          siblings: [],
          queue: makeEmptyQueue(),
          tags: null,
          coverUrl: null,
          mediaUrl: null,
          mediaLoading: false,
          mediaError: null,
          playing: false,
          currentTime: 0,
          duration: 0,
          bufferedEnd: 0,
          pendingSeekTime: null,
          stalled: false,
          resumeChip: null,
        },
        { syncMedia: false },
      );
      return;
    }

    const nextMachine = typeof raw.machine === 'string' ? raw.machine : state.machine;
    const nextPath = typeof raw.path === 'string' ? raw.path : state.path;
    const hydratedSiblings =
      state.siblings.length > 0
        ? state.siblings
        : Array.isArray(raw.tracks) && raw.tracks.length > 0
          ? raw.tracks
          : state.siblings;
    const trackChanged = nextPath && nextPath !== state.path;
    const matchingSibling =
      (nextPath && hydratedSiblings.find((s) => s.path === nextPath)) ||
      (trackChanged
        ? { name: raw.name || baseFileName(nextPath), path: nextPath, kind: 'file' }
        : state.entry);
    const nextName = raw.name || matchingSibling?.name || (nextPath ? baseFileName(nextPath) : state.name);
    const nextShuffle = typeof raw.shuffle === 'boolean' ? raw.shuffle : state.shuffle;
    const nextRepeat = raw.repeat ? normalizeRepeatMode(raw.repeat) : state.repeat;
    const nextShuffledPaths = Array.isArray(raw.order)
      ? raw.order
      : state.queue.shuffledPaths;
    const nextQueue = nextPath
      ? resolveMusicQueueState(
          hydratedSiblings.length > 0 ? hydratedSiblings : matchingSibling ? [matchingSibling] : [],
          nextPath,
          {
            shuffle: nextShuffle,
            shuffledPaths: nextShuffle ? nextShuffledPaths : null,
            repeat: nextRepeat,
          },
        )
      : state.queue;

    let nextTags = state.tags;
    if (raw.tags && typeof raw.tags === 'object') {
      nextTags = raw.tags;
    } else if (trackChanged) {
      nextTags = null;
    }
    let nextCover = state.coverUrl;
    if (typeof raw.coverDataUrl === 'string' && raw.coverDataUrl) {
      nextCover = raw.coverDataUrl;
    } else if (trackChanged) {
      nextCover = cachedCoverFor(nextMachine, nextPath, matchingSibling);
    }

    const nextError =
      raw.error && typeof raw.error === 'object'
        ? {
            code: raw.error.code || 'unreachable',
            message: raw.error.message || '',
            machine: nextMachine,
          }
        : raw.error === null
          ? null
          : state.mediaError;

    const patch = {
      loaded: true,
      machine: nextMachine,
      path: nextPath,
      entry: matchingSibling,
      name: nextName,
      siblings: hydratedSiblings,
      queue: nextQueue,
      shuffle: nextShuffle,
      repeat: nextRepeat,
      tags: nextTags,
      coverUrl: nextCover,
      mediaLoading: Boolean(raw.loading),
      mediaError: nextError,
      playing: Boolean(raw.playing),
      currentTime: Number.isFinite(raw.positionMs) ? Math.max(0, raw.positionMs / 1000) : state.currentTime,
      duration: Number.isFinite(raw.durationMs) && raw.durationMs > 0 ? raw.durationMs / 1000 : state.duration,
      bufferedEnd: Number.isFinite(raw.bufferedMs) ? Math.max(0, raw.bufferedMs / 1000) : state.bufferedEnd,
      pendingSeekTime: null,
      stalled: Boolean(raw.buffering),
      playbackRate: Number.isFinite(raw.playbackRate) && raw.playbackRate > 0 ? raw.playbackRate : state.playbackRate,
    };

    setState(patch, { syncMedia: false });

    if (trackChanged && nextMachine && nextPath && !raw.tags) {
      loadTrackMetadata(nextMachine, nextPath, matchingSibling, state.caps);
    }
  }

  async function inspectFailedAudioUrl(expectedUrl, el, signal) {
    const isMediaNetworkErr = el?.error?.code === 2;
    if (typeof link?.contentStatus === 'function') {
      const st = await link.contentStatus(expectedUrl);
      if (signal?.aborted) return null;
      if (!st || typeof st !== 'object') {
        return { confirmedDecode: false, code: 'unreachable', retryable: true, message: '' };
      }
      if (st.status === 413 || st.error_code === 'too_large_for_inline') {
        return {
          confirmedDecode: false,
          code: 'too_large',
          retryable: false,
          status: st.status || 413,
          errorCode: st.error_code || null,
          message: st.error_message || '',
        };
      }
      if (st.status === 403) {
        return {
          confirmedDecode: false,
          code: 'denied',
          retryable: false,
          status: 403,
          errorCode: st.error_code || null,
          message: st.error_message || '',
        };
      }
      if (st.status === 404 || st.status === 410 || st.ticket_live === false) {
        return {
          confirmedDecode: false,
          code: 'not_found',
          retryable: true,
          status: st.status || null,
          errorCode: st.error_code || null,
          message: st.error_message || '',
        };
      }
      if (
        st.stream_failed ||
        st.request_count === 0 ||
        st.status === 429 ||
        (typeof st.status === 'number' && st.status >= 500)
      ) {
        return {
          confirmedDecode: false,
          code: 'unreachable',
          retryable: true,
          status: st.status || null,
          errorCode: st.stream_failed ? 'stream_failed' : st.error_code || null,
          message: st.error_message || '',
        };
      }
      if ((st.status === 200 || st.status === 206) && !st.stream_failed && contentTypeMatchesAudio(st.content_type)) {
        if (isMediaNetworkErr) {
          return {
            confirmedDecode: false,
            code: 'unreachable',
            retryable: true,
            errorCode: 'media_err_network',
            message: '',
          };
        }
        return { confirmedDecode: true, code: 'decode', retryable: false, message: '' };
      }
      return {
        confirmedDecode: false,
        code: 'unreachable',
        retryable: true,
        status: st.status || null,
        errorCode: st.error_code || null,
        message: st.error_message || '',
      };
    }

    if (isLocalMediaUrl(expectedUrl)) {
      return { confirmedDecode: true, code: 'decode', retryable: false, message: '' };
    }

    try {
      const probe = await fetch(expectedUrl, { method: 'HEAD', signal });
      if (signal?.aborted) return null;
      if (probe.status === 413) return { confirmedDecode: false, code: 'too_large', retryable: false, status: 413, message: '' };
      if (probe.status === 403) return { confirmedDecode: false, code: 'denied', retryable: false, status: 403, message: '' };
      if (probe.status === 404 || probe.status === 410) {
        return { confirmedDecode: false, code: 'not_found', retryable: true, status: probe.status, message: '' };
      }
      if (probe.status === 429 || probe.status >= 500) {
        return { confirmedDecode: false, code: 'unreachable', retryable: true, status: probe.status, message: '' };
      }
      if (probe.ok && contentTypeMatchesAudio(probe.headers.get('content-type'))) {
        if (isMediaNetworkErr) {
          return { confirmedDecode: false, code: 'unreachable', retryable: true, errorCode: 'media_err_network', message: '' };
        }
        return { confirmedDecode: true, code: 'decode', retryable: false, message: '' };
      }
      return { confirmedDecode: false, code: 'unreachable', retryable: true, status: probe.status, message: '' };
    } catch {
      if (signal?.aborted) return null;
      return { confirmedDecode: false, code: 'unreachable', retryable: true, errorCode: 'network_error', message: '' };
    }
  }

  function handleBeforeResumePlay(el) {
    const expectedKey = fileKeyOf(state.machine, state.path);
    const expectedUrl = state.mediaUrl;
    const hidden = windowHiddenPause;
    if (!hidden || hidden.key !== expectedKey || hidden.url !== expectedUrl) {
      return false;
    }
    if (hidden.refreshInFlight) return true;

    const brokenAfterHide = hidden.teardownDetected || Boolean(el?.error) || el?.networkState === 3;
    const canRefresh = Boolean(
      !isLocalMediaUrl(expectedUrl) && state.caps?.hasContentUrl && typeof link?.contentUrl === 'function',
    );
    const savedTime =
      el && typeof el.currentTime === 'number' && Number.isFinite(el.currentTime) && el.currentTime > 0
        ? el.currentTime
        : hidden.time > 0
          ? hidden.time
          : playbackTrack.key === expectedKey
            ? playbackTrack.time
            : 0;
    if (!brokenAfterHide || !canRefresh) {
      windowHiddenPause = null;
      playbackTrack = { key: expectedKey, time: savedTime, wantPlay: true };
      return false;
    }

    hidden.refreshInFlight = true;
    const signal = loadAbort?.signal;
    recovery = { key: expectedKey, lastAttemptAt: Date.now(), inFlight: true };
    link
      .contentUrl({ machine: state.machine, path: state.path, signal })
      .then((res) => {
        if (windowHiddenPause === hidden) windowHiddenPause = null;
        if (signal?.aborted || fileKeyOf(state.machine, state.path) !== expectedKey) {
          if (res?.url) revokeContentUrlNow(res.url);
          return;
        }
        recovery.inFlight = false;
        if (!res?.url) {
          revokeContentUrlNow(expectedUrl);
          setState({ mediaUrl: null, mediaLoading: false, playing: false, mediaError: { code: 'unreachable', message: '' } });
          return;
        }
        if (expectedUrl && expectedUrl !== res.url) revokeContentUrlNow(expectedUrl);
        mintedUrl = res.url;
        pendingResume = { key: expectedKey, url: res.url, time: savedTime, play: true };
        setState({ mediaUrl: res.url, mediaLoading: false, mediaError: null });
        syncAudioElementSrc(res.url);
      })
      .catch((err) => {
        if (windowHiddenPause === hidden) windowHiddenPause = null;
        if (err?.code === 'cancelled' || signal?.aborted || fileKeyOf(state.machine, state.path) !== expectedKey) return;
        recovery.inFlight = false;
        revokeContentUrlNow(expectedUrl);
        setState({ mediaUrl: null, mediaLoading: false, playing: false, mediaError: classifyMintError(err) });
      });
    return true;
  }

  async function handleAudioErrorAsync(expectedKey, expectedUrl, el) {
    const now = Date.now();
    const sameFile = recovery.key === expectedKey;
    if (sameFile && recovery.inFlight) return;
    const recentlyTried = sameFile && recovery.lastAttemptAt > 0 && now - recovery.lastAttemptAt < MEDIA_RECOVERY_WINDOW_MS;
    const canRefresh = Boolean(state.caps?.hasContentUrl && typeof link?.contentUrl === 'function');
    const signal = loadAbort?.signal;

    const inspection = await inspectFailedAudioUrl(expectedUrl, el, signal);
    if (!inspection || fileKeyOf(state.machine, state.path) !== expectedKey || state.mediaUrl !== expectedUrl) {
      return;
    }

    if (
      !isLocalMediaUrl(expectedUrl) &&
      inspection.code !== 'too_large' &&
      inspection.code !== 'denied' &&
      !recentlyTried &&
      canRefresh
    ) {
      const savedTime =
        el && typeof el.currentTime === 'number' && Number.isFinite(el.currentTime) && el.currentTime > 0
          ? el.currentTime
          : playbackTrack.key === expectedKey
            ? playbackTrack.time
            : 0;
      const savedPlay = Boolean(
        (playbackTrack.key === expectedKey && playbackTrack.wantPlay) || (el && !el.paused && !el.ended),
      );
      recovery = { key: expectedKey, lastAttemptAt: now, inFlight: true };
      try {
        const res = await link.contentUrl({ machine: state.machine, path: state.path, signal });
        if (signal?.aborted || fileKeyOf(state.machine, state.path) !== expectedKey) {
          if (res?.url) revokeContentUrlNow(res.url);
          return;
        }
        recovery.inFlight = false;
        if (!res?.url) {
          revokeContentUrlNow(expectedUrl);
          setState({
            mediaUrl: null,
            mediaLoading: false,
            playing: false,
            mediaError: { code: 'unreachable', machine: state.machine, message: '' },
          });
          return;
        }
        if (expectedUrl && expectedUrl !== res.url) revokeContentUrlNow(expectedUrl);
        mintedUrl = res.url;
        pendingResume = { key: expectedKey, url: res.url, time: savedTime, play: savedPlay };
        setState({ mediaUrl: res.url, mediaLoading: false, mediaError: null });
        syncAudioElementSrc(res.url);
      } catch (err) {
        if (err?.code === 'cancelled' || signal?.aborted || fileKeyOf(state.machine, state.path) !== expectedKey) return;
        recovery.inFlight = false;
        revokeContentUrlNow(expectedUrl);
        setState({
          mediaUrl: null,
          mediaLoading: false,
          playing: false,
          mediaError: classifyMintError(err),
        });
      }
      return;
    }

    if (fileKeyOf(state.machine, state.path) !== expectedKey || state.mediaUrl !== expectedUrl) return;
    revokeContentUrlNow(expectedUrl);
    setState({
      mediaUrl: null,
      mediaLoading: false,
      playing: false,
      mediaError: {
        code: inspection.code,
        machine: state.machine,
        message: inspection.message || '',
        status: inspection.status || null,
        errorCode: inspection.errorCode || null,
      },
    });
  }

  function applyPendingResume(expectedKey, expectedUrl, el) {
    const resume = pendingResume;
    if (!resume || resume.key !== expectedKey || resume.url !== expectedUrl || !el) return false;
    pendingResume = null;
    const duration = Number.isFinite(el.duration) && el.duration > 0 ? el.duration : null;
    const targetTime =
      resume.time > 0
        ? duration !== null
          ? Math.min(resume.time, Math.max(0, duration - 0.05))
          : resume.time
        : 0;
    playbackTrack = { key: expectedKey, time: targetTime, wantPlay: Boolean(resume.play) };
    if (targetTime > 0 && Math.abs(el.currentTime - targetTime) > 0.05) {
      try {
        el.currentTime = targetTime;
      } catch {
        // Ignore seek failures.
      }
    }
    if (resume.play) {
      const p = el.play();
      if (p && typeof p.catch === 'function') p.catch(() => {});
    }
    return true;
  }

  function play() {
    if (!state.loaded) return;
    if (usingNative) {
      nativeAudio.command('play').catch(() => {});
      setState({ playing: true }, { syncMedia: false });
      return;
    }
    const a = audioEl;
    if (a && state.mediaUrl && (a.paused || a.ended || a.error || a.readyState < 1) && handleBeforeResumePlay(a)) {
      autoPlayTarget = { path: state.path, play: true };
      setState({ playing: true });
      return;
    }
    if (!a || !state.mediaUrl || a.readyState < 1) {
      autoPlayTarget = { path: state.path, play: true };
      setState({ playing: true });
      return;
    }
    if (a.ended) {
      try {
        a.currentTime = 0;
      } catch {
        // Ignore seek errors.
      }
    }
    iosAudio?.activateSession?.();
    a.play().catch((err) => {
      if (err?.name !== 'NotAllowedError' && (a.error || a.networkState === 3)) {
        handleAudioError(a);
      }
    });
  }

  function pause() {
    if (!state.loaded) return;
    autoPlayTarget = null;
    if (usingNative) {
      nativeAudio.command('pause').catch(() => {});
      setState({ playing: false }, { syncMedia: false });
      return;
    }
    const a = audioEl;
    if (a && !a.paused) {
      try {
        a.pause();
      } catch {
        // Ignore pause errors.
      }
    }
    clearStallTimer();
    setState({ playing: false, pendingSeekTime: null });
  }

  function togglePlay() {
    if (!state.loaded) return;
    if (usingNative) {
      if (state.playing) pause();
      else play();
      return;
    }
    const a = audioEl;
    if (a && state.mediaUrl && (a.paused || a.ended || a.error || a.readyState < 1) && handleBeforeResumePlay(a)) {
      autoPlayTarget = { path: state.path, play: true };
      setState({ playing: true });
      return;
    }
    const loadingNext = !a || !state.mediaUrl || a.readyState < 1;
    if (loadingNext) {
      const currentWant = Boolean(
        autoPlayTarget?.play && (!autoPlayTarget.path || autoPlayTarget.path === state.path),
      );
      const nextWant = !currentWant;
      autoPlayTarget = nextWant ? { path: state.path, play: true } : null;
      setState({ playing: nextWant });
      return;
    }
    if (a.paused || a.ended) {
      play();
    } else {
      pause();
    }
  }

  function seek(targetTime) {
    if (!state.loaded) return;
    const dur =
      !usingNative && audioEl && Number.isFinite(audioEl.duration) && audioEl.duration > 0
        ? audioEl.duration
        : state.duration;
    const clamped = Math.max(0, dur > 0 ? Math.min(dur, targetTime) : targetTime);
    if (usingNative) {
      setState({ currentTime: clamped, pendingSeekTime: clamped }, { syncMedia: false });
      nativeAudio.command('seek', { positionMs: Math.round(clamped * 1000) }).catch(() => {});
      return;
    }
    setState({ pendingSeekTime: clamped, currentTime: clamped });
    startStallTimer();
    if (audioEl) {
      try {
        audioEl.currentTime = clamped;
      } catch {
        // Ignore seek before ready.
      }
    }
  }

  function prev() {
    if (!state.loaded) return;
    if (usingNative) {
      nativeAudio.command('prev').catch(() => {});
      return;
    }
    const a = audioEl;
    const t =
      state.pendingSeekTime != null
        ? state.pendingSeekTime
        : a && Number.isFinite(a.currentTime)
          ? a.currentTime
          : state.currentTime;
    const decision = resolveMusicPrevAction(state.queue, t);
    if (decision.action === 'restart' || !decision.entry) {
      seek(0);
      return;
    }
    const wantContinue = Boolean(
      (state.mediaUrl && a && !a.paused) ||
        (autoPlayTarget?.play && (!autoPlayTarget.path || autoPlayTarget.path === state.path)),
    );
    openTrack({
      machine: state.machine,
      path: decision.entry.path,
      entry: decision.entry,
      siblings: state.siblings,
      caps: state.caps,
      autoPlay: wantContinue,
      preserveShuffleOrder: true,
    });
  }

  function next() {
    if (!state.loaded) return;
    if (usingNative) {
      if (state.queue.next) {
        nativeAudio.command('next').catch(() => {});
      }
      return;
    }
    if (!state.queue.next) return;
    const a = audioEl;
    const wantContinue = Boolean(
      (state.mediaUrl && a && !a.paused) ||
        (autoPlayTarget?.play && (!autoPlayTarget.path || autoPlayTarget.path === state.path)),
    );
    openTrack({
      machine: state.machine,
      path: state.queue.next.path,
      entry: state.queue.next,
      siblings: state.siblings,
      caps: state.caps,
      autoPlay: wantContinue,
      preserveShuffleOrder: true,
    });
  }

  function toggleShuffle() {
    const nextShuffle = !state.shuffle;
    writeMusicModePreferences(store, { shuffle: nextShuffle, repeat: state.repeat });
    const nextQueue = state.loaded
      ? resolveMusicQueueState(state.siblings, state.path, {
          shuffle: nextShuffle,
          shuffledPaths: null,
          repeat: state.repeat,
        })
      : state.queue;
    setState({ shuffle: nextShuffle, queue: nextQueue });
    if (usingNative && state.loaded) {
      nativeAudio
        .setModes({
          shuffle: nextShuffle,
          repeat: state.repeat,
          order: nextQueue.shuffledPaths || nextQueue.items.map((item) => item.path),
        })
        .catch(() => {});
    }
  }

  function cycleRepeat() {
    const nextRepeat = cycleRepeatMode(state.repeat);
    writeMusicModePreferences(store, { shuffle: state.shuffle, repeat: nextRepeat });
    const nextQueue = state.loaded
      ? resolveMusicQueueState(state.siblings, state.path, {
          shuffle: state.shuffle,
          shuffledPaths: state.queue.shuffledPaths,
          repeat: nextRepeat,
        })
      : state.queue;
    setState({ repeat: nextRepeat, queue: nextQueue });
    if (usingNative && state.loaded) {
      nativeAudio
        .setModes({
          shuffle: state.shuffle,
          repeat: nextRepeat,
          order: nextQueue.shuffledPaths || nextQueue.items.map((item) => item.path),
        })
        .catch(() => {});
    }
  }

  function setPlaybackRate(rate) {
    const valid = Number.isFinite(rate) && rate > 0 ? rate : 1;
    if (!usingNative && audioEl) {
      audioEl.playbackRate = valid;
    }
    setState({ playbackRate: valid });
    if (usingNative && state.loaded) {
      nativeAudio.command('speed', { playbackRate: valid }).catch(() => {});
    }
  }

  function setVolume(nextVol) {
    const clamped = clampVolume(nextVol);
    if (!usingNative && audioEl) {
      audioEl.volume = clamped;
      audioEl.muted = clamped === 0;
    }
    setState({ volume: clamped, muted: clamped === 0 });
  }

  function toggleMute() {
    if (usingNative) return;
    const a = audioEl;
    if (!a) return;
    if (a.muted || a.volume === 0) {
      const restored = state.volume > 0 ? state.volume : 1;
      a.muted = false;
      a.volume = restored;
      setState({ muted: false, volume: restored });
    } else {
      a.muted = true;
      setState({ muted: true });
    }
  }

  function startOver() {
    const id = identityOf(state);
    if (id) resumeStore.clear(id);
    clearChipTimer();
    setState({ resumeChip: null });
    seek(0);
  }

  function retry() {
    if (!state.loaded || !state.machine || !state.path) return;
    recovery = { key: fileKeyOf(state.machine, state.path), lastAttemptAt: 0, inFlight: false };
    pendingResume = null;
    if (usingNative) {
      openTrack({
        machine: state.machine,
        path: state.path,
        entry: state.entry,
        siblings: state.siblings,
        caps: state.caps,
        autoPlay: true,
        preserveShuffleOrder: true,
      });
      return;
    }
    mintTrackUrl(state.machine, state.path, state.caps);
  }

  function dismiss() {
    if (!state.loaded) return;
    saveCurrentResume();
    cancelPausedDismiss();
    clearStallTimer();
    clearChipTimer();
    if (loadAbort) {
      loadAbort.abort();
      loadAbort = null;
    }
    if (metaAbort) {
      metaAbort.abort();
      metaAbort = null;
    }
    autoPlayTarget = null;
    pendingResume = null;
    windowHiddenPause = null;
    mintInFlightKey = null;
    if (usingNative) {
      nativeAudio.dismiss().catch(() => {});
    } else {
      clearAudioElement();
      revokeActiveMintedUrl();
      clearPlatformMediaSession();
    }
    setState(
      {
        loaded: false,
        machine: null,
        path: null,
        entry: null,
        name: '',
        siblings: [],
        queue: makeEmptyQueue(),
        tags: null,
        coverUrl: null,
        mediaUrl: null,
        mediaLoading: false,
        mediaError: null,
        playing: false,
        currentTime: 0,
        duration: 0,
        bufferedEnd: 0,
        pendingSeekTime: null,
        stalled: false,
        resumeChip: null,
      },
      { syncMedia: false },
    );
  }

  function handleAudioError(el) {
    const expectedKey = fileKeyOf(state.machine, state.path);
    const expectedUrl = state.mediaUrl;
    if (!expectedUrl) return;
    const hidden = windowHiddenPause;
    if (hidden && hidden.key === expectedKey && hidden.url === expectedUrl) {
      hidden.teardownDetected = true;
      if (el && typeof el.currentTime === 'number' && Number.isFinite(el.currentTime) && el.currentTime > 0) {
        hidden.time = el.currentTime;
      }
      return;
    }
    clearStallTimer();
    setState({ playing: false });
    void handleAudioErrorAsync(expectedKey, expectedUrl, el);
  }

  const audioHandlers = {
    onLoadedMetadata(e) {
      const a = e.currentTarget;
      const expectedKey = fileKeyOf(state.machine, state.path);
      const expectedUrl = state.mediaUrl;
      if (!expectedUrl || !a.getAttribute('src')) return;
      const dur = Number.isFinite(a.duration) && a.duration > 0 ? a.duration : state.entry?.duration_s || 0;
      const patch = { bufferedEnd: readBufferedEnd(a) };
      if (dur > 0) patch.duration = dur;
      a.playbackRate = state.playbackRate;
      a.volume = state.volume;
      a.muted = state.muted;
      const recovered = applyPendingResume(expectedKey, expectedUrl, a);
      if (!recovered && resumedAppliedKey !== expectedKey && dur > 0) {
        resumedAppliedKey = expectedKey;
        const id = identityOf(state);
        const saved = id ? resumeStore.load(id, dur) : null;
        if (saved != null) {
          try {
            a.currentTime = saved;
            patch.currentTime = saved;
          } catch {
            // Ignore seek before ready.
          }
          patch.resumeChip = { position: saved };
          clearChipTimer();
          chipTimer = activeClock().setTimeout(() => {
            chipTimer = null;
            setState({ resumeChip: null });
          }, RESUME_CHIP_DURATION_MS);
        }
      }
      const pendingAuto = autoPlayTarget;
      autoPlayTarget = null;
      if (!recovered) {
        if (pendingAuto?.play && (!pendingAuto.path || pendingAuto.path === state.path)) {
          iosAudio?.activateSession?.();
          a.play().catch(() => {});
        } else {
          if (!a.paused) {
            try {
              a.pause();
            } catch {
              // Ignore pause errors.
            }
          }
          patch.playing = false;
        }
      }
      setState(patch);
    },
    onPlay(e) {
      const a = e.currentTarget;
      if (!state.mediaUrl || !a.getAttribute('src')) return;
      const expectedKey = fileKeyOf(state.machine, state.path);
      playbackTrack = {
        key: expectedKey,
        time: !pendingResume && Number.isFinite(a.currentTime) && a.currentTime > 0 ? a.currentTime : playbackTrack.time,
        wantPlay: true,
      };
      setState({ playing: true });
    },
    onPlaying(e) {
      const a = e.currentTarget;
      if (!state.mediaUrl || !a.getAttribute('src')) return;
      const expectedKey = fileKeyOf(state.machine, state.path);
      playbackTrack = {
        key: expectedKey,
        time: !pendingResume && Number.isFinite(a.currentTime) && a.currentTime > 0 ? a.currentTime : playbackTrack.time,
        wantPlay: true,
      };
      clearStallTimer();
      setState({ playing: true, stalled: false });
    },
    onPause(e) {
      const a = e.currentTarget;
      if (!state.mediaUrl || !a.getAttribute('src')) return;
      if (!a.error && pendingResume?.key !== fileKeyOf(state.machine, state.path)) {
        playbackTrack.wantPlay = false;
        if (!a.ended && Number.isFinite(a.currentTime)) {
          playbackTrack.time = a.currentTime;
        }
      }
      if (Number.isFinite(a.duration) && a.duration > 0) {
        const id = identityOf(state);
        if (id) resumeStore.save(id, a.currentTime, a.duration);
      }
      setState({ playing: false });
    },
    onWaiting() {
      if (state.mediaUrl) startStallTimer();
    },
    onCanPlay(e) {
      if (!state.mediaUrl || !e.currentTarget.getAttribute('src')) return;
      clearStallTimer();
      setState({ bufferedEnd: readBufferedEnd(e.currentTarget), stalled: false });
    },
    onSeeked(e) {
      const a = e.currentTarget;
      if (!state.mediaUrl || !a.getAttribute('src')) return;
      const t = Number.isFinite(a.currentTime) ? a.currentTime : 0;
      if (!pendingResume && !a.error) {
        playbackTrack.time = t;
      }
      clearStallTimer();
      if (Number.isFinite(a.duration) && a.duration > 0) {
        const id = identityOf(state);
        if (id) resumeStore.save(id, t, a.duration);
      }
      setState({ pendingSeekTime: null, currentTime: t, bufferedEnd: readBufferedEnd(a), stalled: false });
    },
    onTimeUpdate(e) {
      const a = e.currentTarget;
      if (!state.mediaUrl || !a.getAttribute('src')) return;
      const t = Number.isFinite(a.currentTime) ? a.currentTime : 0;
      if (!pendingResume && !a.error) {
        playbackTrack.time = t;
      }
      if (state.pendingSeekTime != null) return;
      if (Number.isFinite(a.duration) && a.duration > 0) {
        const id = identityOf(state);
        if (id) resumeStore.savePeriodic(id, t, a.duration, activeClock().now());
      }
      setState({ currentTime: t, bufferedEnd: readBufferedEnd(a) });
    },
    onProgress(e) {
      if (!state.mediaUrl || !e.currentTarget.getAttribute('src')) return;
      setState({ bufferedEnd: readBufferedEnd(e.currentTarget) }, { syncMedia: false });
    },
    onEnded(e) {
      const a = e.currentTarget;
      if (!state.mediaUrl || !a.getAttribute('src')) return;
      playbackTrack.wantPlay = false;
      const id = identityOf(state);
      if (id) resumeStore.clear(id);
      clearStallTimer();
      const action = resolveMusicEndedAction(state.queue, state.repeat);
      if (action.action === 'repeat_current') {
        try {
          a.currentTime = 0;
        } catch {
          // Ignore seek errors.
        }
        setState({ currentTime: 0, playing: true });
        a.play().catch(() => {
          setState({ playing: false });
        });
      } else if (action.action === 'advance' && action.entry) {
        openTrack({
          machine: state.machine,
          path: action.entry.path,
          entry: action.entry,
          siblings: state.siblings,
          caps: state.caps,
          autoPlay: true,
          preserveShuffleOrder: true,
        });
      } else {
        setState({ playing: false });
      }
    },
    onError(e) {
      if (!state.mediaUrl || !e.currentTarget.getAttribute('src')) return;
      handleAudioError(e.currentTarget);
    },
  };

  function bindAudioElement(el) {
    audioEl = el || null;
    if (audioEl && !usingNative && state.mediaUrl) {
      syncAudioElementSrc(state.mediaUrl);
    }
  }

  function renameMachine(oldName, newName) {
    if (!oldName || !newName || oldName === newName) return;
    resumeStore.renameMachine?.(oldName, newName);
    if (state.loaded && state.machine === oldName) {
      setState({ machine: newName });
    }
  }

  // Subscribe to desktop window-hide events: pause music in place and keep it resumable.
  if (typeof link?.onWindowHidden === 'function') {
    unsubWindowHidden = link.onWindowHidden(() => {
      if (!state.loaded) return;
      autoPlayTarget = null;
      playbackTrack.wantPlay = false;
      if (pendingResume) pendingResume = { ...pendingResume, play: false };
      let savedTime = state.currentTime;
      if (!usingNative && audioEl) {
        if (Number.isFinite(audioEl.currentTime) && audioEl.currentTime > 0) {
          savedTime = audioEl.currentTime;
        }
        if (!audioEl.paused) {
          try {
            audioEl.pause();
          } catch {
            // Ignore pause errors.
          }
        }
      }
      if (!usingNative && state.mediaUrl && !isLocalMediaUrl(state.mediaUrl)) {
        windowHiddenPause = {
          key: fileKeyOf(state.machine, state.path),
          url: state.mediaUrl,
          time: savedTime,
          teardownDetected: false,
          refreshInFlight: false,
        };
      }
      if (usingNative) {
        nativeAudio.command('pause').catch(() => {});
      }
      setState({ playing: false });
    });
  }

  // Subscribe to native Android Media3 audio state updates.
  if (usingNative && typeof nativeAudio.onState === 'function') {
    unsubNativeState = nativeAudio.onState((raw) => {
      applyNativeState(raw);
    });
    if (typeof nativeAudio.getState === 'function') {
      nativeAudio
        .getState()
        .then((raw) => {
          if (raw && raw.loaded) applyNativeState(raw);
        })
        .catch(() => {});
    }
  }

  // Subscribe to iOS remote commands (lock screen, Control Center, headphone unplug, call interruption).
  if (iosAudio && typeof iosAudio.onRemoteCommand === 'function') {
    let resumeAfterInterruption = false;
    unsubIosRemote = iosAudio.onRemoteCommand((cmd) => {
      if (!cmd || typeof cmd !== 'object' || !state.loaded) return;
      switch (cmd.action) {
        case 'play':
          play();
          break;
        case 'pause':
          pause();
          break;
        case 'toggle':
          togglePlay();
          break;
        case 'next':
          next();
          break;
        case 'prev':
          prev();
          break;
        case 'seek':
          if (typeof cmd.position === 'number' && Number.isFinite(cmd.position)) {
            seek(cmd.position);
          }
          break;
        case 'interruption_began':
          resumeAfterInterruption = state.playing;
          if (state.playing) pause();
          break;
        case 'interruption_ended':
          if (cmd.shouldResume && resumeAfterInterruption) {
            resumeAfterInterruption = false;
            play();
          } else {
            resumeAfterInterruption = false;
          }
          break;
        case 'route_unavailable':
          resumeAfterInterruption = false;
          if (state.playing) pause();
          break;
        default:
          break;
      }
    });
  }

  function getState() {
    return state;
  }

  function subscribe(fn) {
    if (typeof fn !== 'function') return () => {};
    listeners.add(fn);
    return () => {
      listeners.delete(fn);
    };
  }

  function dispose() {
    if (dismissed) return;
    dismissed = true;
    cancelPausedDismiss();
    clearStallTimer();
    clearChipTimer();
    if (loadAbort) loadAbort.abort();
    if (metaAbort) metaAbort.abort();
    if (typeof unsubWindowHidden === 'function') unsubWindowHidden();
    if (typeof unsubNativeState === 'function') unsubNativeState();
    if (typeof unsubIosRemote === 'function') unsubIosRemote();
    if (!usingNative) {
      saveCurrentResume();
      clearAudioElement();
      revokeActiveMintedUrl();
      clearPlatformMediaSession();
    }
    listeners.clear();
  }

  return {
    getState,
    subscribe,
    bindAudioElement,
    audioHandlers,
    openTrack,
    play,
    pause,
    togglePlay,
    seek,
    prev,
    next,
    toggleShuffle,
    cycleRepeat,
    setPlaybackRate,
    setVolume,
    toggleMute,
    startOver,
    retry,
    dismiss,
    renameMachine,
    dispose,
  };
}
