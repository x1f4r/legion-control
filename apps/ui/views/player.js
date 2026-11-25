// Video and music players: Legion-drawn controls, single-layer video overlay, tap/double-tap
// gestures, seek-on-release position slider with honest slow-seek state, resume, queue, keys,
// native/browser immersive full screen, and music listening screen with tags, covers & MediaSession.

import { html, useCallback, useEffect, useMemo, useRef, useState } from '../vendor/preact-htm.js';
import { Icon } from '../icons.js';
import {
  clampVolume,
  classifyPlayerKey,
  createResumeStore,
  createVideoController,
  extractTagsMap,
  extractThumbMap,
  formatMusicDisplay,
  formatPlayerTime,
  formatPositionValueText,
  formatRemainingTime,
  formatSpeed,
  PLAYBACK_SPEEDS,
  preferredVideoOrientation,
  queueNeighbors,
  SEEK_STEP_S,
  shouldMusicPrevRestart,
  tapZoneForPosition,
  VOLUME_STEP,
} from '../lib/player.js';
import { thumbCacheKey } from '../lib/thumb-cache.js';

function safeStorage() {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

function getPlayerClock() {
  if (typeof window !== 'undefined' && window.__LEGION_TEST_PLAYER_CLOCK__) {
    return window.__LEGION_TEST_PLAYER_CLOCK__;
  }
  return undefined;
}

function isAndroidNative() {
  return Boolean(typeof window !== 'undefined' && window.__LEGION_ANDROID__?.setImmersive);
}

function isIosNative() {
  return Boolean(typeof window !== 'undefined' && window.webkit?.messageHandlers?.legionImmersive);
}

function isMobileTouchDevice() {
  if (isAndroidNative() || isIosNative()) return true;
  if (typeof navigator === 'undefined') return false;
  const ua = String(navigator.userAgent || '');
  if (/Android|iPhone|iPad|iPod/i.test(ua)) return true;
  if (navigator.platform === 'MacIntel' && Number(navigator.maxTouchPoints || 0) > 1) return true;
  return false;
}

function isTouchPortraitLandscapeDevice() {
  if (isMobileTouchDevice()) return true;
  if (typeof window !== 'undefined' && typeof window.matchMedia === 'function') {
    return window.matchMedia('(hover: none) and (pointer: coarse)').matches;
  }
  return false;
}

function readBufferedEnd(media) {
  if (!media || !media.buffered || media.buffered.length === 0) return 0;
  try {
    const cur = Number.isFinite(media.currentTime) ? media.currentTime : 0;
    for (let i = 0; i < media.buffered.length; i += 1) {
      const start = media.buffered.start(i);
      const end = media.buffered.end(i);
      if (cur >= start - 0.5 && cur <= end + 0.5) return end;
    }
    return media.buffered.end(media.buffered.length - 1);
  } catch {
    return 0;
  }
}

// ── Accessible position slider (drag previews under finger, commits ONE seek on release) ─────

export function PositionSlider({
  currentTime = 0,
  duration = 0,
  bufferedEnd = 0,
  onDragStart,
  onDragMove,
  onCommitSeek,
  onInteract,
}) {
  const trackRef = useRef(null);
  const draggingRef = useRef(false);
  const dragPointerIdRef = useRef(null);
  const dragTimeRef = useRef(0);
  const [dragging, setDragging] = useState(false);
  const [dragTime, setDragTime] = useState(0);

  const safeDuration = Number.isFinite(duration) && duration > 0 ? duration : 0;
  const displayTime = dragging
    ? dragTime
    : Math.max(0, Math.min(safeDuration || Infinity, Number.isFinite(currentTime) ? currentTime : 0));
  const pct = safeDuration > 0 ? Math.max(0, Math.min(100, (displayTime / safeDuration) * 100)) : 0;
  const bufPct =
    safeDuration > 0 && Number.isFinite(bufferedEnd) && bufferedEnd > 0
      ? Math.max(0, Math.min(100, (bufferedEnd / safeDuration) * 100))
      : 0;

  function timeFromClientX(clientX) {
    const el = trackRef.current;
    if (!el || safeDuration <= 0) return 0;
    const rect = el.getBoundingClientRect();
    if (rect.width <= 0) return 0;
    const ratio = Math.max(0, Math.min(1, (clientX - rect.left) / rect.width));
    return ratio * safeDuration;
  }

  function onPointerDown(e) {
    if (e.button > 0) return;
    e.stopPropagation();
    if (onInteract) onInteract();
    const t = timeFromClientX(e.clientX);
    draggingRef.current = true;
    dragPointerIdRef.current = e.pointerId;
    dragTimeRef.current = t;
    setDragging(true);
    setDragTime(t);
    try {
      e.currentTarget.setPointerCapture?.(e.pointerId);
    } catch {
      // Ignore synthetic events without active pointer id.
    }
    if (onDragStart) onDragStart(t);
  }

  function onPointerMove(e) {
    if (!draggingRef.current) return;
    if (dragPointerIdRef.current != null && e.pointerId !== dragPointerIdRef.current) return;
    e.stopPropagation();
    const t = timeFromClientX(e.clientX);
    dragTimeRef.current = t;
    setDragTime(t);
    if (onDragMove) onDragMove(t);
  }

  function finishDrag(e, commit) {
    if (!draggingRef.current) return;
    if (dragPointerIdRef.current != null && e.pointerId !== dragPointerIdRef.current) return;
    e.stopPropagation();
    const finalTime = commit && typeof e.clientX === 'number' ? timeFromClientX(e.clientX) : dragTimeRef.current;
    draggingRef.current = false;
    dragPointerIdRef.current = null;
    setDragging(false);
    try {
      e.currentTarget.releasePointerCapture?.(e.pointerId);
    } catch {
      // Ignore capture release errors.
    }
    if (commit && onCommitSeek) {
      onCommitSeek(finalTime);
    } else if (onDragMove) {
      onDragMove(null);
    }
  }

  function onKeyDown(e) {
    if (safeDuration <= 0) return;
    let next = null;
    if (e.key === 'ArrowLeft' || e.key === 'ArrowDown') {
      next = Math.max(0, displayTime - 5);
    } else if (e.key === 'ArrowRight' || e.key === 'ArrowUp') {
      next = Math.min(safeDuration, displayTime + 5);
    } else if (e.key === 'PageDown') {
      next = Math.max(0, displayTime - 30);
    } else if (e.key === 'PageUp') {
      next = Math.min(safeDuration, displayTime + 30);
    } else if (e.key === 'Home') {
      next = 0;
    } else if (e.key === 'End') {
      next = safeDuration;
    }
    if (next !== null) {
      e.preventDefault();
      e.stopPropagation();
      if (onInteract) onInteract();
      if (onCommitSeek) onCommitSeek(next);
    }
  }

  const valueText = formatPositionValueText(displayTime, safeDuration);

  return html`
    <div
      ref=${trackRef}
      class=${dragging ? 'player-scrub dragging' : 'player-scrub'}
      role="slider"
      tabIndex="0"
      aria-label="Position"
      aria-valuemin="0"
      aria-valuemax=${Math.max(0, Math.round(safeDuration))}
      aria-valuenow=${Math.max(0, Math.round(displayTime))}
      aria-valuetext=${valueText}
      onPointerDown=${onPointerDown}
      onPointerMove=${onPointerMove}
      onPointerUp=${(e) => finishDrag(e, true)}
      onPointerCancel=${(e) => finishDrag(e, false)}
      onKeyDown=${onKeyDown}
    >
      ${dragging
        ? html`
            <span class="player-scrub-tip" style=${{ left: `${pct}%` }} aria-hidden="true">
              ${formatPlayerTime(dragTime)}
            </span>
          `
        : null}
      <span class="player-scrub-track" aria-hidden="true">
        <span class="player-scrub-buf" style=${{ width: `${bufPct}%` }}></span>
        <span class="player-scrub-fill" style=${{ width: `${pct}%` }}></span>
        <span class="player-scrub-thumb" style=${{ left: `${pct}%` }}></span>
      </span>
    </div>
  `;
}

// ── Desktop / Browser volume slider ──────────────────────────────────────────────────────────

function VolumeControl({ volume, muted, onToggleMute, onSetVolume, onInteract }) {
  const barRef = useRef(null);
  const draggingRef = useRef(false);
  const effective = muted ? 0 : clampVolume(volume);
  const pct = Math.round(effective * 100);

  function volFromClientX(clientX) {
    const el = barRef.current;
    if (!el) return effective;
    const rect = el.getBoundingClientRect();
    if (rect.width <= 0) return effective;
    return clampVolume((clientX - rect.left) / rect.width);
  }

  return html`
    <div class="player-vol">
      <button
        type="button"
        class="btn icon player-btn"
        aria-label=${effective === 0 ? 'Unmute' : 'Mute'}
        aria-pressed=${effective === 0 ? 'true' : 'false'}
        onClick=${(e) => {
          e.stopPropagation();
          if (onInteract) onInteract();
          onToggleMute();
        }}
      >
        <${Icon} name=${effective === 0 ? 'volume-mute' : 'volume'} />
      </button>
      <div
        ref=${barRef}
        class="player-vol-slider"
        role="slider"
        tabIndex="0"
        aria-label="Volume"
        aria-valuemin="0"
        aria-valuemax="100"
        aria-valuenow=${pct}
        aria-valuetext=${`${pct} percent`}
        onPointerDown=${(e) => {
          if (e.button > 0) return;
          e.stopPropagation();
          if (onInteract) onInteract();
          draggingRef.current = true;
          try {
            e.currentTarget.setPointerCapture?.(e.pointerId);
          } catch {
            // Ignore synthetic events.
          }
          onSetVolume(volFromClientX(e.clientX));
        }}
        onPointerMove=${(e) => {
          if (!draggingRef.current) return;
          e.stopPropagation();
          onSetVolume(volFromClientX(e.clientX));
        }}
        onPointerUp=${(e) => {
          if (!draggingRef.current) return;
          e.stopPropagation();
          draggingRef.current = false;
          try {
            e.currentTarget.releasePointerCapture?.(e.pointerId);
          } catch {
            // Ignore synthetic events.
          }
        }}
        onPointerCancel=${() => {
          draggingRef.current = false;
        }}
        onKeyDown=${(e) => {
          let next = null;
          if (e.key === 'ArrowLeft' || e.key === 'ArrowDown') next = clampVolume(effective - VOLUME_STEP);
          else if (e.key === 'ArrowRight' || e.key === 'ArrowUp') next = clampVolume(effective + VOLUME_STEP);
          else if (e.key === 'Home') next = 0;
          else if (e.key === 'End') next = 1;
          if (next !== null) {
            e.preventDefault();
            e.stopPropagation();
            if (onInteract) onInteract();
            onSetVolume(next);
          }
        }}
      >
        <span class="player-vol-track" aria-hidden="true">
          <span class="player-vol-fill" style=${{ width: `${pct}%` }}></span>
          <span class="player-vol-thumb" style=${{ left: `${pct}%` }}></span>
        </span>
      </div>
    </div>
  `;
}

// ── Video Player ─────────────────────────────────────────────────────────────────────────────

export function VideoPlayer({
  machine,
  path,
  entry,
  name,
  mediaUrl,
  mediaRef = null,
  siblings = [],
  link,
  viewerRef,
  moreOpen = false,
  onOpenMore,
  onClose,
  onNavigateSibling,
  onMediaLoadedMetadata,
  onMediaTime,
  onMediaPlay,
  onMediaPause,
  onMediaError,
  onFullscreenStateChange,
}) {
  const videoRef = useRef(null);
  const bindVideoRef = useCallback(
    (el) => {
      videoRef.current = el;
      if (mediaRef) mediaRef.current = el;
    },
    [mediaRef],
  );
  const overlayRef = useRef(null);
  const stageTapRef = useRef(null);
  const resumeStore = useMemo(() => createResumeStore(safeStorage()), []);
  const identity = useMemo(
    () => ({ machine, path, size: entry?.size, mtime: entry?.mtime }),
    [machine, path, entry?.size, entry?.mtime],
  );
  const identityRef = useRef(identity);
  identityRef.current = identity;

  const queue = useMemo(() => queueNeighbors(siblings, path, 'video'), [siblings, path]);
  const showVolume = useMemo(() => !isMobileTouchDevice(), []);
  const touchOrientationDevice = useMemo(() => isTouchPortraitLandscapeDevice(), []);
  const mobileNative = useMemo(() => isAndroidNative() || isIosNative(), []);

  const [ctrl, setCtrl] = useState({
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
    resumeChip: null,
    seekFlash: null,
  });
  const [currentTime, setCurrentTime] = useState(0);
  const [duration, setDuration] = useState(entry?.duration_s > 0 ? entry.duration_s : 0);
  const [bufferedEnd, setBufferedEnd] = useState(0);
  const [playbackRate, setPlaybackRate] = useState(1);
  const [volume, setVolume] = useState(1);
  const [muted, setMuted] = useState(false);
  const [speedOpen, setSpeedOpen] = useState(false);
  const [isFullscreen, setIsFullscreen] = useState(false);
  const [fullscreenSource, setFullscreenSource] = useState(null); // null | 'button' | 'rotation'
  const [canFs, setCanFs] = useState(() =>
    typeof link?.canImmersive === 'function' ? Boolean(link.canImmersive({ element: viewerRef?.current })) : false,
  );

  const isFullscreenRef = useRef(false);
  isFullscreenRef.current = isFullscreen;
  const fullscreenSourceRef = useRef(null);
  fullscreenSourceRef.current = fullscreenSource;
  const pushedFsHistoryRef = useRef(false);
  const suppressNextPopRef = useRef(false);
  const leavingFsUntilRef = useRef(0);
  const activeSeekSeqRef = useRef(0);
  const lastSavedAtRef = useRef(0);
  const resumedAppliedRef = useRef(false);
  const lastKeyNavRef = useRef(false);
  const moreOpenRef = useRef(moreOpen);
  moreOpenRef.current = moreOpen;
  const speedOpenRef = useRef(speedOpen);
  speedOpenRef.current = speedOpen;
  const queueRef = useRef(queue);
  queueRef.current = queue;
  const durationRef = useRef(duration);
  durationRef.current = duration;

  const commitSeekOnMedia = useCallback((targetTime, seq) => {
    activeSeekSeqRef.current = seq;
    const v = videoRef.current;
    if (!v) return;
    const dur = Number.isFinite(v.duration) && v.duration > 0 ? v.duration : durationRef.current;
    const clamped = Math.max(0, dur > 0 ? Math.min(dur, targetTime) : targetTime);
    setCurrentTime(clamped);
    try {
      v.currentTime = clamped;
    } catch {
      // Media not ready yet; keep pending target displayed.
    }
  }, []);

  const controllerRef = useRef(null);
  if (!controllerRef.current) {
    controllerRef.current = createVideoController({
      clock: getPlayerClock(),
      onChange: (next) => setCtrl(next),
      onCommitSeek: (target, seq) => commitSeekOnMedia(target, seq),
      onSeekDelta: (delta) => {
        const v = videoRef.current;
        const st = controllerRef.current?.getState();
        const base =
          st?.pendingSeekTime != null
            ? st.pendingSeekTime
            : v && Number.isFinite(v.currentTime)
              ? v.currentTime
              : 0;
        const dur = v && Number.isFinite(v.duration) && v.duration > 0 ? v.duration : durationRef.current;
        const next = Math.max(0, dur > 0 ? Math.min(dur, base + delta) : base + delta);
        controllerRef.current?.beginSeek(next);
      },
    });
  }
  const controller = controllerRef.current;

  useEffect(() => () => controller.dispose(), [controller]);

  // Reset per-video state when path/url changes.
  useEffect(() => {
    resumedAppliedRef.current = false;
    lastSavedAtRef.current = 0;
    setCurrentTime(0);
    setDuration(entry?.duration_s > 0 ? entry.duration_s : 0);
    setBufferedEnd(0);
    setSpeedOpen(false);
    controller.setError(false);
    controller.setLoading(true);
    controller.dismissResumeChip();
  }, [machine, path, mediaUrl, controller]);

  // Re-evaluate full-screen availability once `<video>` and `.viewer` have mounted.
  useEffect(() => {
    if (typeof link?.canImmersive === 'function') {
      setCanFs(Boolean(link.canImmersive({ element: viewerRef?.current, video: videoRef.current })));
    } else {
      setCanFs(false);
    }
  }, [link, viewerRef, mediaUrl]);

  // Sync `inert` property on overlay container so hidden controls can never be focused or pressed.
  useEffect(() => {
    const el = overlayRef.current;
    if (!el) return;
    el.inert = !ctrl.visible;
    if (!ctrl.visible && document.activeElement && el.contains(document.activeElement)) {
      try {
        document.activeElement.blur();
      } catch {
        // Ignore blur errors.
      }
    }
  }, [ctrl.visible]);

  // Keep overlay visible while More sheet or speed popover is open.
  useEffect(() => {
    controller.setMenuOpen(Boolean(moreOpen || speedOpen));
  }, [moreOpen, speedOpen, controller]);

  // Notify parent viewer about fullscreen state for styling / external sheet coordination.
  useEffect(() => {
    if (onFullscreenStateChange) onFullscreenStateChange(isFullscreen, fullscreenSource);
  }, [isFullscreen, fullscreenSource, onFullscreenStateChange]);

  // Screen stays on while video plays (in or out of full screen); released on pause/end/close.
  useEffect(() => {
    link?.keepScreenOn?.(Boolean(ctrl.playing));
  }, [link, ctrl.playing]);

  const portraitImmersiveRef = useRef(false);

  // On mobile native apps, hiding the overlay in portrait hides system bars too ("one state").
  useEffect(() => {
    if (!mobileNative || isFullscreen) {
      portraitImmersiveRef.current = false;
      return;
    }
    if (!ctrl.visible) {
      portraitImmersiveRef.current = true;
      link?.immersive?.(true, { orientation: 'none', element: viewerRef?.current, video: videoRef.current });
    } else if (portraitImmersiveRef.current) {
      portraitImmersiveRef.current = false;
      link?.immersive?.(false, { element: viewerRef?.current, video: videoRef.current });
    }
  }, [mobileNative, isFullscreen, ctrl.visible, link, viewerRef]);

  const enterFullscreen = useCallback(
    async (source = 'button') => {
      if (typeof link?.immersive !== 'function') return;
      const v = videoRef.current;
      const orientation =
        source === 'button' ? preferredVideoOrientation(v?.videoWidth, v?.videoHeight) : 'none';
      const res = await link.immersive(true, {
        orientation,
        element: viewerRef?.current,
        video: v,
      });
      if (res && res.active === false) return;
      setIsFullscreen(true);
      setFullscreenSource(source);
      if (source === 'button' && !pushedFsHistoryRef.current && res?.mode !== 'webkit-video') {
        try {
          window.history.pushState({ legionFullscreen: true }, '');
          pushedFsHistoryRef.current = true;
        } catch {
          // Ignore restricted history APIs.
        }
      }
    },
    [link, viewerRef],
  );

  const leaveFullscreen = useCallback(
    async ({ fromPopState = false } = {}) => {
      const hadPushed = pushedFsHistoryRef.current;
      pushedFsHistoryRef.current = false;
      portraitImmersiveRef.current = false;
      const wasLandscapeNow = typeof window !== 'undefined' && window.innerWidth > window.innerHeight;
      leavingFsUntilRef.current = wasLandscapeNow ? Date.now() + 900 : 0;
      setIsFullscreen(false);
      setFullscreenSource(null);
      controller.notifyActivity({ show: true });
      if (typeof link?.immersive === 'function') {
        await link.immersive(false, { element: viewerRef?.current, video: videoRef.current });
      }
      if (hadPushed && !fromPopState) {
        suppressNextPopRef.current = true;
        try {
          window.history.back();
        } catch {
          suppressNextPopRef.current = false;
        }
      }
    },
    [controller, link, viewerRef],
  );

  // Sync platform-initiated full-screen exits (browser Esc, macOS window button, iOS webkitendfullscreen).
  useEffect(() => {
    const unsub =
      typeof link?.onImmersiveChange === 'function'
        ? link.onImmersiveChange((active) => {
            if (!active && isFullscreenRef.current) {
              const hadPushed = pushedFsHistoryRef.current;
              pushedFsHistoryRef.current = false;
              setIsFullscreen(false);
              setFullscreenSource(null);
              if (hadPushed) {
                suppressNextPopRef.current = true;
                try {
                  window.history.back();
                } catch {
                  suppressNextPopRef.current = false;
                }
              }
            }
          })
        : () => {};
    const v = videoRef.current;
    const onWebkitEnd = () => {
      if (isFullscreenRef.current) {
        setIsFullscreen(false);
        setFullscreenSource(null);
      }
    };
    v?.addEventListener?.('webkitendfullscreen', onWebkitEnd);
    return () => {
      unsub();
      v?.removeEventListener?.('webkitendfullscreen', onWebkitEnd);
    };
  }, [link, mediaUrl]);

  // Android / browser Back gesture while button-entered full screen leaves full screen without closing viewer.
  useEffect(() => {
    function onPopState() {
      if (suppressNextPopRef.current) {
        suppressNextPopRef.current = false;
        return;
      }
      if (pushedFsHistoryRef.current && !window.history.state?.legionFullscreen) {
        leaveFullscreen({ fromPopState: true });
      }
    }
    window.addEventListener('popstate', onPopState);
    return () => window.removeEventListener('popstate', onPopState);
  }, [leaveFullscreen]);

  // Turning a phone or tablet to landscape enters full screen; turning back leaves it if turning entered it.
  useEffect(() => {
    if (!touchOrientationDevice || typeof link?.immersive !== 'function') return undefined;

    let wasLandscape = false;
    const checkOrientation = (initial = false) => {
      const landscape = window.innerWidth > window.innerHeight;
      const turnedToLandscape = initial ? landscape : !wasLandscape && landscape;
      wasLandscape = landscape;
      if (!landscape) {
        leavingFsUntilRef.current = 0;
      }
      if (Date.now() < leavingFsUntilRef.current) {
        return;
      }
      if (turnedToLandscape && !isFullscreenRef.current) {
        enterFullscreen('rotation');
      } else if (!landscape && isFullscreenRef.current && fullscreenSourceRef.current === 'rotation') {
        leaveFullscreen();
      }
    };

    checkOrientation(true);
    const onResize = () => checkOrientation(false);
    window.addEventListener('resize', onResize);
    window.addEventListener('orientationchange', onResize);
    window.screen?.orientation?.addEventListener?.('change', onResize);
    return () => {
      window.removeEventListener('resize', onResize);
      window.removeEventListener('orientationchange', onResize);
      window.screen?.orientation?.removeEventListener?.('change', onResize);
    };
  }, [touchOrientationDevice, link, enterFullscreen, leaveFullscreen]);

  // Always release immersive, keep-screen-on, and save resume position on unmount.
  useEffect(() => {
    return () => {
      const v = videoRef.current;
      if (v && Number.isFinite(v.duration) && v.duration > 0) {
        resumeStore.save(identityRef.current, v.currentTime, v.duration);
      }
      link?.keepScreenOn?.(false);
      link?.immersive?.(false, { element: viewerRef?.current, video: v });
      if (v) {
        try {
          v.pause();
          v.removeAttribute('src');
          v.load();
        } catch {
          // Ignore teardown errors.
        }
      }
    };
  }, [link, resumeStore, viewerRef]);

  const togglePlay = useCallback(() => {
    const v = videoRef.current;
    if (!v) return;
    controller.notifyActivity({ show: true });
    if (v.paused || v.ended) {
      if (v.ended) {
        try {
          v.currentTime = 0;
        } catch {
          // Ignore seek errors.
        }
      }
      v.play().catch(() => {});
    } else {
      v.pause();
      controller.setPlaying(false);
    }
  }, [controller]);

  const seekBy = useCallback(
    (delta) => {
      const v = videoRef.current;
      const st = controller.getState();
      const base =
        st.pendingSeekTime != null
          ? st.pendingSeekTime
          : v && Number.isFinite(v.currentTime)
            ? v.currentTime
            : 0;
      const dur = v && Number.isFinite(v.duration) && v.duration > 0 ? v.duration : durationRef.current;
      const next = Math.max(0, dur > 0 ? Math.min(dur, base + delta) : base + delta);
      controller.notifyActivity({ show: true });
      controller.beginSeek(next);
    },
    [controller],
  );

  const applyVolume = useCallback((nextVol) => {
    const clamped = clampVolume(nextVol);
    setVolume(clamped);
    setMuted(clamped === 0);
    const v = videoRef.current;
    if (v) {
      v.volume = clamped;
      v.muted = clamped === 0;
    }
  }, []);

  const toggleMute = useCallback(() => {
    const v = videoRef.current;
    if (!v) return;
    if (v.muted || v.volume === 0) {
      const restored = volume > 0 ? volume : 1;
      v.muted = false;
      v.volume = restored;
      setMuted(false);
      setVolume(restored);
    } else {
      v.muted = true;
      setMuted(true);
    }
  }, [volume]);

  const applyRate = useCallback((rate) => {
    setPlaybackRate(rate);
    setSpeedOpen(false);
    const v = videoRef.current;
    if (v) v.playbackRate = rate;
  }, []);

  const handleStartOver = useCallback(() => {
    resumeStore.clear(identityRef.current);
    controller.dismissResumeChip();
    controller.beginSeek(0);
  }, [resumeStore, controller]);

  const handleBackClick = useCallback(() => {
    if (fullscreenSourceRef.current === 'button') {
      leaveFullscreen();
      return;
    }
    const closeNow = async () => {
      if (isFullscreenRef.current) {
        await leaveFullscreen();
      }
      if (onClose) onClose();
    };
    closeNow();
  }, [leaveFullscreen, onClose]);

  const handleToggleFullscreen = useCallback(() => {
    if (isFullscreenRef.current) {
      leaveFullscreen();
    } else {
      enterFullscreen('button');
    }
  }, [enterFullscreen, leaveFullscreen]);

  // Track keyboard vs pointer interaction so only keyboard focus holds the overlay visible.
  useEffect(() => {
    const onKey = () => {
      lastKeyNavRef.current = true;
    };
    const onPtr = () => {
      lastKeyNavRef.current = false;
    };
    window.addEventListener('keydown', onKey, true);
    window.addEventListener('pointerdown', onPtr, true);
    return () => {
      window.removeEventListener('keydown', onKey, true);
      window.removeEventListener('pointerdown', onPtr, true);
    };
  }, []);

  // Keyboard shortcuts (desktop & browser).
  useEffect(() => {
    function onKeyDown(e) {
      if (moreOpenRef.current) return;
      if (speedOpenRef.current && e.key === 'Escape') {
        e.preventDefault();
        setSpeedOpen(false);
        return;
      }
      const hasScrim = Boolean(document.querySelector('.scrim'));
      const action = classifyPlayerKey(e, {
        mode: 'video',
        isFullscreen: isFullscreenRef.current,
        hasOpenOverlay: hasScrim,
      });
      if (!action) return;
      e.preventDefault();
      e.stopImmediatePropagation?.();
      controller.notifyActivity({ show: true });

      switch (action) {
        case 'toggle_play':
          togglePlay();
          break;
        case 'seek_back':
          seekBy(-SEEK_STEP_S);
          break;
        case 'seek_forward':
          seekBy(SEEK_STEP_S);
          break;
        case 'volume_up':
          if (showVolume) applyVolume((muted ? 0 : volume) + VOLUME_STEP);
          break;
        case 'volume_down':
          if (showVolume) applyVolume((muted ? 0 : volume) - VOLUME_STEP);
          break;
        case 'toggle_mute':
          if (showVolume) toggleMute();
          break;
        case 'toggle_fullscreen':
          if (canFs) handleToggleFullscreen();
          break;
        case 'next':
          if (queueRef.current.next && onNavigateSibling) {
            onNavigateSibling(queueRef.current.next);
          }
          break;
        case 'prev':
          if (queueRef.current.prev && onNavigateSibling) {
            onNavigateSibling(queueRef.current.prev);
          }
          break;
        case 'exit_fullscreen':
          leaveFullscreen();
          break;
        case 'close':
          if (onClose) onClose();
          break;
        default:
          break;
      }
    }
    window.addEventListener('keydown', onKeyDown, true);
    return () => window.removeEventListener('keydown', onKeyDown, true);
  }, [
    controller,
    togglePlay,
    seekBy,
    showVolume,
    applyVolume,
    muted,
    volume,
    toggleMute,
    canFs,
    handleToggleFullscreen,
    onNavigateSibling,
    leaveFullscreen,
    onClose,
  ]);

  // Stage pointer handlers: detect clean tap vs drag, and show overlay on desktop pointer movement.
  function onStagePointerDown(e) {
    if (e.button > 0) return;
    if (speedOpen) {
      setSpeedOpen(false);
      return;
    }
    stageTapRef.current = { id: e.pointerId, x: e.clientX, y: e.clientY, moved: false };
  }

  function onStagePointerMove(e) {
    if (e.pointerType === 'mouse' && !stageTapRef.current) {
      controller.notifyActivity({ show: true });
      return;
    }
    const tap = stageTapRef.current;
    if (tap && tap.id === e.pointerId) {
      if (Math.hypot(e.clientX - tap.x, e.clientY - tap.y) > 12) {
        tap.moved = true;
      }
    }
  }

  function onStagePointerUp(e) {
    const tap = stageTapRef.current;
    if (!tap || tap.id !== e.pointerId) return;
    stageTapRef.current = null;
    if (tap.moved) return;
    const rect = e.currentTarget.getBoundingClientRect();
    const zone = tapZoneForPosition(e.clientX, rect.left, rect.width);
    controller.handlePictureTap(zone);
  }

  function onStagePointerCancel() {
    stageTapRef.current = null;
  }

  const displayTime = ctrl.pendingSeekTime != null ? ctrl.pendingSeekTime : currentTime;

  return html`
    <div
      class=${['player-video-wrap', ctrl.cursorHidden ? 'cursor-hidden' : '', isFullscreen ? 'is-fullscreen' : '']
        .filter(Boolean)
        .join(' ')}
      onPointerMove=${(e) => {
        if (e.pointerType === 'mouse') controller.notifyActivity({ show: true });
      }}
    >
      <video
        ref=${bindVideoRef}
        class="viewer-video"
        src=${mediaUrl}
        playsinline
        preload="metadata"
        aria-label=${name}
        onLoadedMetadata=${(e) => {
          const v = e.currentTarget;
          const dur = Number.isFinite(v.duration) && v.duration > 0 ? v.duration : entry?.duration_s || 0;
          if (dur > 0) setDuration(dur);
          setBufferedEnd(readBufferedEnd(v));
          v.playbackRate = playbackRate;
          const recovered = Boolean(onMediaLoadedMetadata?.(v));
          if (!recovered && !resumedAppliedRef.current && dur > 0) {
            resumedAppliedRef.current = true;
            const saved = resumeStore.load(identityRef.current, dur);
            if (saved != null) {
              try {
                v.currentTime = saved;
                setCurrentTime(saved);
              } catch {
                // Ignore seek before ready.
              }
              controller.showResumeChip(saved);
            }
          }
          controller.setLoading(false);
        }}
        onLoadedData=${(e) => {
          setBufferedEnd(readBufferedEnd(e.currentTarget));
          controller.onCanPlay();
        }}
        onCanPlay=${(e) => {
          setBufferedEnd(readBufferedEnd(e.currentTarget));
          controller.onCanPlay();
        }}
        onPlay=${(e) => {
          onMediaPlay?.(e.currentTarget);
          controller.setPlaying(true);
        }}
        onPlaying=${(e) => {
          onMediaPlay?.(e.currentTarget);
          controller.setPlaying(true);
          controller.onCanPlay();
        }}
        onPause=${(e) => {
          const v = e.currentTarget;
          onMediaPause?.(v);
          if (Number.isFinite(v.duration) && v.duration > 0) {
            resumeStore.save(identityRef.current, v.currentTime, v.duration);
          }
          if (!v.ended) controller.setPlaying(false);
        }}
        onWaiting=${() => controller.onWaiting()}
        onStalled=${() => controller.onWaiting()}
        onSeeked=${(e) => {
          const v = e.currentTarget;
          onMediaTime?.(v);
          setCurrentTime(Number.isFinite(v.currentTime) ? v.currentTime : 0);
          setBufferedEnd(readBufferedEnd(v));
          controller.onSeeked(activeSeekSeqRef.current);
          if (Number.isFinite(v.duration) && v.duration > 0) {
            resumeStore.save(identityRef.current, v.currentTime, v.duration);
          }
        }}
        onTimeUpdate=${(e) => {
          const v = e.currentTarget;
          onMediaTime?.(v);
          if (controller.getState().pendingSeekTime != null) return;
          const t = Number.isFinite(v.currentTime) ? v.currentTime : 0;
          setCurrentTime(t);
          setBufferedEnd(readBufferedEnd(v));
          const now = Date.now();
          if (now - lastSavedAtRef.current >= 2000 && Number.isFinite(v.duration) && v.duration > 0) {
            lastSavedAtRef.current = now;
            resumeStore.save(identityRef.current, t, v.duration, now);
          }
        }}
        onProgress=${(e) => setBufferedEnd(readBufferedEnd(e.currentTarget))}
        onEnded=${(e) => {
          onMediaPause?.(e.currentTarget);
          resumeStore.clear(identityRef.current);
          controller.setEnded(true);
        }}
        onError=${(e) => {
          controller.setError(true);
          if (onMediaError) onMediaError(e.currentTarget);
        }}
      />

      <div
        class="player-stage-hit"
        onPointerDown=${onStagePointerDown}
        onPointerMove=${onStagePointerMove}
        onPointerUp=${onStagePointerUp}
        onPointerCancel=${onStagePointerCancel}
      />

      ${ctrl.seekFlash
        ? html`
            <div
              class=${`player-seek-flash ${ctrl.seekFlash.direction === 'back' ? 'left' : 'right'}`}
              aria-hidden="true"
            >
              ${ctrl.seekFlash.direction === 'back' ? `−${ctrl.seekFlash.delta} s` : `+${ctrl.seekFlash.delta} s`}
            </div>
          `
        : null}

      ${ctrl.stalled
        ? html`<div class="player-spinner" role="status" aria-label="Buffering"><span></span></div>`
        : null}

      ${ctrl.resumeChip
        ? html`
            <div class="player-resume-chip" role="status">
              <span>Resumed at ${formatPlayerTime(ctrl.resumeChip.position)}</span>
              <span aria-hidden="true">·</span>
              <button type="button" class="player-resume-btn" onClick=${handleStartOver}>
                Start over
              </button>
            </div>
          `
        : null}

      <div
        ref=${overlayRef}
        class=${ctrl.visible ? 'player-overlay' : 'player-overlay hidden'}
        aria-hidden=${ctrl.visible ? undefined : 'true'}
        onFocusCapture=${(e) => {
          const isKeyboard =
            lastKeyNavRef.current ||
            (typeof e.target?.matches === 'function' && e.target.matches(':focus-visible'));
          if (isKeyboard) {
            controller.setFocusInside(true);
          }
        }}
        onBlurCapture=${(e) => {
          const next = e.relatedTarget;
          if (!next || !overlayRef.current?.contains(next)) {
            controller.setFocusInside(false);
          }
        }}
      >
        <div class="viewer-top player-top">
          <button
            type="button"
            class="btn icon"
            aria-label=${fullscreenSource === 'button' ? 'Back' : 'Close'}
            onClick=${handleBackClick}
          >
            <${Icon} name=${fullscreenSource === 'button' ? 'back' : 'down'} />
          </button>
          <span class="name">${name}</span>
          <button
            type="button"
            class="btn icon"
            aria-label="More options"
            onClick=${() => {
              setSpeedOpen(false);
              if (onOpenMore) onOpenMore();
            }}
          >
            <${Icon} name="more" />
          </button>
        </div>

        <div class="player-center" aria-label="Playback controls">
          <button
            type="button"
            class="btn icon player-center-btn"
            aria-label="Back 10 seconds"
            onClick=${() => seekBy(-SEEK_STEP_S)}
          >
            <${Icon} name="skip-back-10" />
          </button>
          <button
            type="button"
            class="btn icon player-center-btn main"
            aria-label=${ctrl.playing ? 'Pause' : 'Play'}
            onClick=${togglePlay}
          >
            <${Icon} name=${ctrl.playing ? 'pause' : 'play'} />
          </button>
          <button
            type="button"
            class="btn icon player-center-btn"
            aria-label="Forward 10 seconds"
            onClick=${() => seekBy(SEEK_STEP_S)}
          >
            <${Icon} name="skip-fwd-10" />
          </button>
        </div>

        <div class="player-bottom">
          <${PositionSlider}
            currentTime=${displayTime}
            duration=${duration}
            bufferedEnd=${bufferedEnd}
            onInteract=${() => controller.notifyActivity({ show: true })}
            onDragStart=${() => controller.setDragging(true)}
            onDragMove=${(t) => {
              if (t == null) controller.setDragging(false);
            }}
            onCommitSeek=${(target) => controller.beginSeek(target)}
          />
          <div class="player-times" aria-hidden="true">
            <span>${formatPlayerTime(displayTime)}</span>
            <span>${formatRemainingTime(displayTime, duration)}</span>
          </div>

          <div class="player-controls">
            <div class="player-controls-group">
              ${queue.prev
                ? html`
                    <button
                      type="button"
                      class="btn icon player-btn"
                      aria-label="Previous video"
                      onClick=${() => onNavigateSibling && onNavigateSibling(queue.prev)}
                    >
                      <${Icon} name="prev" />
                    </button>
                  `
                : null}
              ${queue.next
                ? html`
                    <button
                      type="button"
                      class="btn icon player-btn"
                      aria-label="Next video"
                      onClick=${() => onNavigateSibling && onNavigateSibling(queue.next)}
                    >
                      <${Icon} name="next" />
                    </button>
                  `
                : null}
              ${showVolume
                ? html`
                    <${VolumeControl}
                      volume=${volume}
                      muted=${muted}
                      onToggleMute=${toggleMute}
                      onSetVolume=${applyVolume}
                      onInteract=${() => controller.notifyActivity({ show: true })}
                    />
                  `
                : null}
            </div>

            <div class="player-controls-group">
              <div class="player-speed-wrap">
                <button
                  type="button"
                  class="btn small player-speed-btn"
                  aria-label=${`Playback speed ${formatSpeed(playbackRate)}`}
                  aria-haspopup="menu"
                  aria-expanded=${speedOpen ? 'true' : 'false'}
                  onClick=${() => {
                    controller.notifyActivity({ show: true });
                    setSpeedOpen((v) => !v);
                  }}
                >
                  ${formatSpeed(playbackRate)}
                </button>
                ${speedOpen
                  ? html`
                      <div class="player-speed-menu" role="menu" aria-label="Playback speed">
                        ${PLAYBACK_SPEEDS.map(
                          (rate) => html`
                            <button
                              key=${rate}
                              type="button"
                              role="menuitemradio"
                              aria-checked=${rate === playbackRate ? 'true' : 'false'}
                              class=${rate === playbackRate ? 'active' : ''}
                              onClick=${() => applyRate(rate)}
                            >
                              ${formatSpeed(rate)}
                            </button>
                          `,
                        )}
                      </div>
                    `
                  : null}
              </div>

              ${canFs
                ? html`
                    <button
                      type="button"
                      class="btn icon player-btn"
                      aria-label=${isFullscreen ? 'Exit full screen' : 'Full screen'}
                      onClick=${handleToggleFullscreen}
                    >
                      <${Icon} name=${isFullscreen ? 'exit-fullscreen' : 'fullscreen'} />
                    </button>
                  `
                : null}
            </div>
          </div>
        </div>
      </div>
    </div>
  `;
}

// ── Music Player ─────────────────────────────────────────────────────────────────────────────

export function MusicPlayer({
  machine,
  path,
  entry,
  name,
  mediaUrl,
  mediaRef = null,
  autoPlayNextRef: externalAutoPlayRef = null,
  siblings = [],
  link,
  capsStore = null,
  caps,
  thumbCache = null,
  moreOpen = false,
  playbackRate = 1,
  onOpenMore,
  onClose,
  onNavigateSibling,
  onMediaLoadedMetadata,
  onMediaTime,
  onMediaPlay,
  onMediaPause,
  onMediaError,
}) {
  const audioRef = useRef(null);
  const bindAudioRef = useCallback(
    (el) => {
      audioRef.current = el;
      if (mediaRef) mediaRef.current = el;
    },
    [mediaRef],
  );
  const resumeStore = useMemo(() => createResumeStore(safeStorage()), []);
  const identity = useMemo(
    () => ({ machine, path, size: entry?.size, mtime: entry?.mtime }),
    [machine, path, entry?.size, entry?.mtime],
  );
  const identityRef = useRef(identity);
  identityRef.current = identity;

  const queue = useMemo(() => queueNeighbors(siblings, path, 'audio'), [siblings, path]);
  const queueRef = useRef(queue);
  queueRef.current = queue;
  const showVolume = useMemo(() => !isMobileTouchDevice(), []);

  const [tags, setTags] = useState(null);
  const [coverUrl, setCoverUrl] = useState(() => {
    if (!thumbCache) return null;
    const largeKey = thumbCacheKey(machine, path, entry?.mtime || '', 1024);
    const largeHit = thumbCache.get(largeKey);
    if (largeHit && !largeHit.failed) return largeHit.url;
    const smallKey = thumbCacheKey(machine, path, entry?.mtime || '', 128);
    const smallHit = thumbCache.get(smallKey);
    if (smallHit && !smallHit.failed) return smallHit.url;
    return null;
  });
  const [playing, setPlaying] = useState(false);
  const [currentTime, setCurrentTime] = useState(0);
  const [duration, setDuration] = useState(entry?.duration_s > 0 ? entry.duration_s : 0);
  const [bufferedEnd, setBufferedEnd] = useState(0);
  const [pendingSeekTime, setPendingSeekTime] = useState(null);
  const [stalled, setStalled] = useState(false);
  const [resumeChip, setResumeChip] = useState(null);
  const [volume, setVolume] = useState(1);
  const [muted, setMuted] = useState(false);

  const localAutoPlayNextRef = useRef(null);
  const autoPlayNextRef = externalAutoPlayRef || localAutoPlayNextRef;
  const resumedAppliedRef = useRef(false);
  const lastSavedAtRef = useRef(0);
  const stallTimerRef = useRef(null);
  const chipTimerRef = useRef(null);
  const durationRef = useRef(duration);
  durationRef.current = duration;
  const currentTimeRef = useRef(currentTime);
  currentTimeRef.current = currentTime;
  const moreOpenRef = useRef(moreOpen);
  moreOpenRef.current = moreOpen;

  const display = useMemo(() => formatMusicDisplay(tags, name), [tags, name]);

  // Apply playbackRate changes (set via More sheet) to `<audio>` immediately and on effect flush.
  if (audioRef.current && audioRef.current.playbackRate !== playbackRate) {
    audioRef.current.playbackRate = playbackRate;
  }
  useEffect(() => {
    const a = audioRef.current;
    if (a) a.playbackRate = playbackRate;
  }, [playbackRate]);

  // Reset per-track state when switching songs.
  useEffect(() => {
    resumedAppliedRef.current = false;
    lastSavedAtRef.current = 0;
    setTags(null);
    setCurrentTime(0);
    setDuration(entry?.duration_s > 0 ? entry.duration_s : 0);
    setBufferedEnd(0);
    setPendingSeekTime(null);
    setStalled(false);
    setResumeChip(null);
    if (stallTimerRef.current) {
      clearTimeout(stallTimerRef.current);
      stallTimerRef.current = null;
    }
    if (chipTimerRef.current) {
      clearTimeout(chipTimerRef.current);
      chipTimerRef.current = null;
    }
    if (thumbCache) {
      const largeHit = thumbCache.get(thumbCacheKey(machine, path, entry?.mtime || '', 1024));
      const smallHit = thumbCache.get(thumbCacheKey(machine, path, entry?.mtime || '', 128));
      setCoverUrl(
        largeHit && !largeHit.failed ? largeHit.url : smallHit && !smallHit.failed ? smallHit.url : null,
      );
    } else {
      setCoverUrl(null);
    }
  }, [machine, path, entry?.mtime, thumbCache]);

  // Load tags (`fs.tags`) and high-res cover (`fs.thumb`) via per-machine capabilities, silently
  // falling back to filename + music tile on machines that lack them.
  useEffect(() => {
    const ctrl = new AbortController();
    let alive = true;

    const runCall = (tool, args) =>
      capsStore
        ? capsStore.call(tool, args, { on: machine, signal: ctrl.signal }).then((r) => r.value)
        : link.call(tool, args, { on: machine, signal: ctrl.signal });

    const loadCover = async () => {
      if (!caps?.hasAudioCovers) return;
      const reqSize = Math.min(1024, caps?.thumbMaxSize || 512);
      const cacheKey = thumbCacheKey(machine, path, entry?.mtime || '', reqSize);
      const cached = thumbCache?.get(cacheKey);
      if (cached) {
        if (alive && !cached.failed) setCoverUrl(cached.url);
        return;
      }
      try {
        const res = await runCall('fs.thumb', { paths: [path], size: reqSize, fit: 'contain' });
        if (!alive || ctrl.signal.aborted) return;
        const map = extractThumbMap(res);
        const item = map[path];
        if (item?.content && item?.mime) {
          const url = `data:${item.mime};base64,${item.content}`;
          thumbCache?.set(cacheKey, { url });
          setCoverUrl(url);
        } else {
          thumbCache?.set(cacheKey, { failed: true });
        }
      } catch {
        // Older machines or songs without embedded/folder covers stay on the music tile quietly.
      }
    };

    (async () => {
      let hasCoverHint = null;
      if (caps?.hasTags) {
        try {
          const res = await runCall('fs.tags', { paths: [path] });
          if (!alive || ctrl.signal.aborted) return;
          const map = extractTagsMap(res);
          const item = map[path];
          if (item?.ok) {
            setTags(item.ok);
            if (typeof item.ok.duration_s === 'number' && item.ok.duration_s > 0 && durationRef.current <= 0) {
              setDuration(item.ok.duration_s);
            }
            hasCoverHint = Boolean(item.ok.cover);
          }
        } catch {
          // Silently show filename when tags are unavailable.
        }
      }
      if (!alive || ctrl.signal.aborted) return;
      if (hasCoverHint !== false) {
        await loadCover();
      }
    })();

    return () => {
      alive = false;
      ctrl.abort();
    };
  }, [machine, path, entry?.mtime, caps?.hasTags, caps?.hasAudioCovers, caps?.thumbMaxSize, capsStore, link, thumbCache]);

  // Save resume position & clean up timers/audio on unmount.
  useEffect(() => {
    return () => {
      if (stallTimerRef.current) clearTimeout(stallTimerRef.current);
      if (chipTimerRef.current) clearTimeout(chipTimerRef.current);
      const a = audioRef.current;
      if (a && Number.isFinite(a.duration) && a.duration > 0) {
        resumeStore.save(identityRef.current, a.currentTime, a.duration);
      }
      if (a) {
        try {
          a.pause();
          a.removeAttribute('src');
          a.load();
        } catch {
          // Ignore teardown errors.
        }
      }
    };
  }, [resumeStore]);

  const startStallTimer = useCallback(() => {
    if (stallTimerRef.current) clearTimeout(stallTimerRef.current);
    stallTimerRef.current = window.setTimeout(() => {
      stallTimerRef.current = null;
      setStalled(true);
    }, 400);
  }, []);

  const clearStallTimer = useCallback(() => {
    if (stallTimerRef.current) {
      clearTimeout(stallTimerRef.current);
      stallTimerRef.current = null;
    }
    setStalled(false);
  }, []);

  const commitSeek = useCallback(
    (targetTime) => {
      const a = audioRef.current;
      const dur = a && Number.isFinite(a.duration) && a.duration > 0 ? a.duration : durationRef.current;
      const clamped = Math.max(0, dur > 0 ? Math.min(dur, targetTime) : targetTime);
      setPendingSeekTime(clamped);
      setCurrentTime(clamped);
      startStallTimer();
      if (a) {
        try {
          a.currentTime = clamped;
        } catch {
          // Ignore seek before ready.
        }
      }
    },
    [startStallTimer],
  );

  const togglePlay = useCallback(() => {
    const a = audioRef.current;
    if (!a) return;
    if (a.paused || a.ended) {
      if (a.ended) {
        try {
          a.currentTime = 0;
        } catch {
          // Ignore seek errors.
        }
      }
      a.play().catch(() => {});
    } else {
      a.pause();
      setPlaying(false);
      setPendingSeekTime(null);
      clearStallTimer();
    }
  }, [clearStallTimer]);

  const handlePrev = useCallback(() => {
    const a = audioRef.current;
    const t = pendingSeekTime != null ? pendingSeekTime : a && Number.isFinite(a.currentTime) ? a.currentTime : currentTimeRef.current;
    if (shouldMusicPrevRestart(t)) {
      commitSeek(0);
      return;
    }
    if (queueRef.current.prev && onNavigateSibling) {
      autoPlayNextRef.current = !a?.paused ? { path: queueRef.current.prev.path, play: true } : null;
      onNavigateSibling(queueRef.current.prev);
    }
  }, [pendingSeekTime, commitSeek, onNavigateSibling, autoPlayNextRef]);

  const handleNext = useCallback(() => {
    const a = audioRef.current;
    if (queueRef.current.next && onNavigateSibling) {
      autoPlayNextRef.current = !a?.paused ? { path: queueRef.current.next.path, play: true } : null;
      onNavigateSibling(queueRef.current.next);
    }
  }, [onNavigateSibling, autoPlayNextRef]);

  const handleStartOver = useCallback(() => {
    resumeStore.clear(identityRef.current);
    setResumeChip(null);
    if (chipTimerRef.current) {
      clearTimeout(chipTimerRef.current);
      chipTimerRef.current = null;
    }
    commitSeek(0);
  }, [resumeStore, commitSeek]);

  const applyVolume = useCallback((nextVol) => {
    const clamped = clampVolume(nextVol);
    setVolume(clamped);
    setMuted(clamped === 0);
    const a = audioRef.current;
    if (a) {
      a.volume = clamped;
      a.muted = clamped === 0;
    }
  }, []);

  const toggleMute = useCallback(() => {
    const a = audioRef.current;
    if (!a) return;
    if (a.muted || a.volume === 0) {
      const restored = volume > 0 ? volume : 1;
      a.muted = false;
      a.volume = restored;
      setMuted(false);
      setVolume(restored);
    } else {
      a.muted = true;
      setMuted(true);
    }
  }, [volume]);

  // Platform media keys & lock-screen player (`navigator.mediaSession`).
  useEffect(() => {
    const ms = typeof navigator !== 'undefined' ? navigator.mediaSession : null;
    if (!ms) return undefined;
    try {
      if (typeof window !== 'undefined' && typeof window.MediaMetadata === 'function') {
        ms.metadata = new window.MediaMetadata({
          title: display.title,
          artist: tags?.artist || '',
          album: tags?.album || '',
          artwork: coverUrl ? [{ src: coverUrl }] : [],
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
    safeSet('play', () => audioRef.current?.play?.().catch(() => {}));
    safeSet('pause', () => audioRef.current?.pause?.());
    safeSet('previoustrack', queue.prev || currentTimeRef.current > 3 ? handlePrev : null);
    safeSet('nexttrack', queue.next ? handleNext : null);
    safeSet('seekto', (details) => {
      if (typeof details?.seekTime === 'number' && Number.isFinite(details.seekTime)) {
        commitSeek(details.seekTime);
      }
    });
    safeSet('seekbackward', (details) => {
      const offset = typeof details?.seekOffset === 'number' && details.seekOffset > 0 ? details.seekOffset : SEEK_STEP_S;
      commitSeek(Math.max(0, currentTimeRef.current - offset));
    });
    safeSet('seekforward', (details) => {
      const offset = typeof details?.seekOffset === 'number' && details.seekOffset > 0 ? details.seekOffset : SEEK_STEP_S;
      commitSeek(currentTimeRef.current + offset);
    });

    return () => {
      for (const action of ['play', 'pause', 'previoustrack', 'nexttrack', 'seekto', 'seekbackward', 'seekforward']) {
        safeSet(action, null);
      }
    };
  }, [display.title, tags?.artist, tags?.album, coverUrl, queue.prev, queue.next, handlePrev, handleNext, commitSeek]);

  useEffect(() => {
    const ms = typeof navigator !== 'undefined' ? navigator.mediaSession : null;
    if (!ms) return;
    try {
      ms.playbackState = playing ? 'playing' : 'paused';
      if (typeof ms.setPositionState === 'function' && Number.isFinite(duration) && duration > 0) {
        ms.setPositionState({
          duration,
          playbackRate,
          position: Math.max(0, Math.min(duration, currentTime)),
        });
      }
    } catch {
      // Ignore position state clamping errors.
    }
  }, [playing, duration, playbackRate, currentTime]);

  // Keyboard shortcuts (desktop & browser).
  useEffect(() => {
    function onKeyDown(e) {
      if (moreOpenRef.current) return;
      const hasScrim = Boolean(document.querySelector('.scrim'));
      const action = classifyPlayerKey(e, { mode: 'audio', isFullscreen: false, hasOpenOverlay: hasScrim });
      if (!action) return;
      e.preventDefault();
      e.stopImmediatePropagation?.();
      switch (action) {
        case 'toggle_play':
          togglePlay();
          break;
        case 'seek_back':
          commitSeek(Math.max(0, currentTimeRef.current - SEEK_STEP_S));
          break;
        case 'seek_forward':
          commitSeek(currentTimeRef.current + SEEK_STEP_S);
          break;
        case 'volume_up':
          if (showVolume) applyVolume((muted ? 0 : volume) + VOLUME_STEP);
          break;
        case 'volume_down':
          if (showVolume) applyVolume((muted ? 0 : volume) - VOLUME_STEP);
          break;
        case 'toggle_mute':
          if (showVolume) toggleMute();
          break;
        case 'next':
          handleNext();
          break;
        case 'prev':
          handlePrev();
          break;
        case 'close':
          if (onClose) onClose();
          break;
        default:
          break;
      }
    }
    window.addEventListener('keydown', onKeyDown, true);
    return () => window.removeEventListener('keydown', onKeyDown, true);
  }, [togglePlay, commitSeek, showVolume, applyVolume, muted, volume, toggleMute, handleNext, handlePrev, onClose]);

  const displayTime = pendingSeekTime != null ? pendingSeekTime : currentTime;
  const showPrev = Boolean(queue.prev || shouldMusicPrevRestart(displayTime));
  const showNext = Boolean(queue.next);

  return html`
    <div class=${coverUrl ? 'music-screen has-cover' : 'music-screen'}>
      ${coverUrl
        ? html`
            <div class="music-backdrop" aria-hidden="true">
              <img src=${coverUrl} alt="" />
            </div>
          `
        : null}

      <div class="viewer-top music-top">
        <button type="button" class="btn icon" aria-label="Close" onClick=${onClose}>
          <${Icon} name="down" />
        </button>
        <span class="name">
          ${queue.items.length > 1 && queue.index >= 0 ? `${queue.index + 1} of ${queue.items.length}` : ''}
        </span>
        <button type="button" class="btn icon" aria-label="More options" onClick=${onOpenMore}>
          <${Icon} name="more" />
        </button>
      </div>

      <div class="music-body">
        <div class="music-cover-wrap">
          <div class="music-cover-box">
            ${coverUrl
              ? html`<img class="music-cover" src=${coverUrl} alt=${display.title} />`
              : html`
                  <div class="music-cover-tile" aria-hidden="true">
                    <${Icon} name="music" />
                  </div>
                `}
          </div>
        </div>

        <div class="music-meta-panel">
          <div class="music-headings">
            <h1 class="music-title">${display.title}</h1>
            ${display.subtitle ? html`<p class="music-subtitle">${display.subtitle}</p>` : null}
          </div>

          ${resumeChip
            ? html`
                <div class="player-resume-chip inline" role="status">
                  <span>Resumed at ${formatPlayerTime(resumeChip.position)}</span>
                  <span aria-hidden="true">·</span>
                  <button type="button" class="player-resume-btn" onClick=${handleStartOver}>
                    Start over
                  </button>
                </div>
              `
            : null}

          <div class="music-scrub-block">
            <${PositionSlider}
              currentTime=${displayTime}
              duration=${duration}
              bufferedEnd=${bufferedEnd}
              onCommitSeek=${commitSeek}
            />
            <div class="player-times" aria-hidden="true">
              <span>${formatPlayerTime(displayTime)}</span>
              <span>${formatRemainingTime(displayTime, duration)}</span>
            </div>
          </div>

          <div class="music-transport">
            ${showPrev
              ? html`
                  <button
                    type="button"
                    class="btn icon music-ctl-btn"
                    aria-label=${shouldMusicPrevRestart(displayTime) ? 'Restart track' : 'Previous track'}
                    onClick=${handlePrev}
                  >
                    <${Icon} name="prev" />
                  </button>
                `
              : null}
            <button
              type="button"
              class="btn icon music-play-btn"
              aria-label=${playing ? 'Pause' : 'Play'}
              onClick=${togglePlay}
            >
              ${stalled
                ? html`<span class="music-spinner" aria-hidden="true"></span>`
                : html`<${Icon} name=${playing ? 'pause' : 'play'} />`}
            </button>
            ${showNext
              ? html`
                  <button
                    type="button"
                    class="btn icon music-ctl-btn"
                    aria-label="Next track"
                    onClick=${handleNext}
                  >
                    <${Icon} name="next" />
                  </button>
                `
              : null}
          </div>

          ${showVolume
            ? html`
                <div class="music-vol-row">
                  <${VolumeControl}
                    volume=${volume}
                    muted=${muted}
                    onToggleMute=${toggleMute}
                    onSetVolume=${applyVolume}
                  />
                </div>
              `
            : null}
        </div>
      </div>

      <audio
        ref=${bindAudioRef}
        src=${mediaUrl}
        preload="metadata"
        aria-label=${display.title}
        onLoadedMetadata=${(e) => {
          const a = e.currentTarget;
          const dur = Number.isFinite(a.duration) && a.duration > 0 ? a.duration : entry?.duration_s || 0;
          if (dur > 0) setDuration(dur);
          setBufferedEnd(readBufferedEnd(a));
          a.playbackRate = playbackRate;
          const recovered = Boolean(onMediaLoadedMetadata?.(a));
          if (!recovered && !resumedAppliedRef.current && dur > 0) {
            resumedAppliedRef.current = true;
            const saved = resumeStore.load(identityRef.current, dur);
            if (saved != null) {
              try {
                a.currentTime = saved;
                setCurrentTime(saved);
              } catch {
                // Ignore seek before ready.
              }
              setResumeChip({ position: saved });
              chipTimerRef.current = window.setTimeout(() => {
                chipTimerRef.current = null;
                setResumeChip(null);
              }, 5000);
            }
          }
          const pendingAuto = autoPlayNextRef.current;
          if (!recovered && pendingAuto?.play && (!pendingAuto.path || pendingAuto.path === path)) {
            autoPlayNextRef.current = null;
            a.play().catch(() => {});
          }
        }}
        onPlay=${(e) => {
          onMediaPlay?.(e.currentTarget);
          setPlaying(true);
        }}
        onPlaying=${(e) => {
          onMediaPlay?.(e.currentTarget);
          setPlaying(true);
          clearStallTimer();
        }}
        onPause=${(e) => {
          const a = e.currentTarget;
          onMediaPause?.(a);
          if (Number.isFinite(a.duration) && a.duration > 0) {
            resumeStore.save(identityRef.current, a.currentTime, a.duration);
          }
          setPlaying(false);
        }}
        onWaiting=${() => startStallTimer()}
        onCanPlay=${(e) => {
          setBufferedEnd(readBufferedEnd(e.currentTarget));
          clearStallTimer();
        }}
        onSeeked=${(e) => {
          const a = e.currentTarget;
          onMediaTime?.(a);
          setPendingSeekTime(null);
          setCurrentTime(Number.isFinite(a.currentTime) ? a.currentTime : 0);
          setBufferedEnd(readBufferedEnd(a));
          clearStallTimer();
          if (Number.isFinite(a.duration) && a.duration > 0) {
            resumeStore.save(identityRef.current, a.currentTime, a.duration);
          }
        }}
        onTimeUpdate=${(e) => {
          const a = e.currentTarget;
          onMediaTime?.(a);
          if (pendingSeekTime != null) return;
          const t = Number.isFinite(a.currentTime) ? a.currentTime : 0;
          setCurrentTime(t);
          setBufferedEnd(readBufferedEnd(a));
          const now = Date.now();
          if (now - lastSavedAtRef.current >= 2000 && Number.isFinite(a.duration) && a.duration > 0) {
            lastSavedAtRef.current = now;
            resumeStore.save(identityRef.current, t, a.duration, now);
          }
        }}
        onProgress=${(e) => setBufferedEnd(readBufferedEnd(e.currentTarget))}
        onEnded=${(e) => {
          onMediaPause?.(e.currentTarget);
          resumeStore.clear(identityRef.current);
          clearStallTimer();
          if (queueRef.current.next && onNavigateSibling) {
            autoPlayNextRef.current = { path: queueRef.current.next.path, play: true };
            onNavigateSibling(queueRef.current.next);
          } else {
            setPlaying(false);
          }
        }}
        onError=${(e) => {
          clearStallTimer();
          setPlaying(false);
          if (onMediaError) onMediaError(e.currentTarget);
        }}
      />
    </div>
  `;
}
