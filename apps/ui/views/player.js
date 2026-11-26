// Video and music players: Legion-drawn controls, single-layer video overlay, tap/double-tap
// gestures, seek-on-release position slider with honest slow-seek state, resume, queue, keys,
// native/browser immersive full screen, and music listening screen with tags, covers & MediaSession.

import { html, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from '../vendor/preact-htm.js';
import { Icon } from '../icons.js';
import {
  clampVolume,
  classifyPlayerKey,
  computeRenderedVideoRect,
  computeVideoZoomMetrics,
  createPictureGestureRecognizer,
  createResumeStore,
  createSpaceHoldController,
  createTurnWatcher,
  createVideoController,
  detectVideoPipSupport,
  formatMusicDisplay,
  formatPlayerTime,
  formatPositionValueText,
  formatRemainingTime,
  formatScrubDelta,
  formatSpeed,
  GESTURE_EDGE_MARGIN_PX,
  HOLD_PILL_FADE_MS,
  HOLD_PLAYBACK_RATE,
  MINI_PLAYER_COVER_RADIUS_PX,
  MINI_PLAYER_TRANSITION_MS,
  PLAYBACK_SPEEDS,
  preferredVideoOrientation,
  queueNeighbors,
  readZoomPreference,
  SEEK_STEP_S,
  shouldMusicPrevRestart,
  VOLUME_STEP,
  writeZoomPreference,
  ZOOM_CHIP_DURATION_MS,
  ZOOM_SETTLE_MS,
} from '../lib/player.js';
import { FileNameSingle } from './file-name.js';

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
  externalDragging = false,
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

  const isVisualDragging = dragging || Boolean(externalDragging);
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
      class=${isVisualDragging ? 'player-scrub dragging' : 'player-scrub'}
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
  mediaLoading = false,
  mediaRef = null,
  siblings = [],
  link,
  viewerRef,
  fullscreenController = null,
  fullscreenState = null,
  moreOpen = false,
  onOpenMore,
  onClose,
  onNavigateSibling,
  onZoomActionChange = null,
  onPipActiveChange = null,
  onMediaLoadedMetadata,
  onMediaTime,
  onMediaPlay,
  onMediaPause,
  onMediaEnded = null,
  onMediaPipDismiss = null,
  onBeforeSeek = null,
  onBeforePlay,
  onMediaError,
}) {
  const videoRef = useRef(null);
  const wrapRef = useRef(null);
  const scrubBarWrapRef = useRef(null);
  const onBeforePlayRef = useRef(onBeforePlay);
  onBeforePlayRef.current = onBeforePlay;
  const onBeforeSeekRef = useRef(onBeforeSeek);
  onBeforeSeekRef.current = onBeforeSeek;
  const onMediaEndedRef = useRef(onMediaEnded);
  onMediaEndedRef.current = onMediaEnded;
  const onMediaPipDismissRef = useRef(onMediaPipDismiss);
  onMediaPipDismissRef.current = onMediaPipDismiss;
  const onPipActiveChangeRef = useRef(onPipActiveChange);
  onPipActiveChangeRef.current = onPipActiveChange;
  const onMediaErrorRef = useRef(onMediaError);
  onMediaErrorRef.current = onMediaError;
  const onZoomActionChangeRef = useRef(onZoomActionChange);
  onZoomActionChangeRef.current = onZoomActionChange;
  const bindVideoRef = useCallback(
    (el) => {
      videoRef.current = el;
      if (mediaRef) mediaRef.current = el;
    },
    [mediaRef],
  );
  const overlayRef = useRef(null);
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
  const [canFs, setCanFs] = useState(() =>
    typeof link?.canImmersive === 'function' ? Boolean(link.canImmersive({ element: viewerRef?.current })) : false,
  );
  const [pipMode, setPipMode] = useState(null);
  const [pipActive, setPipActive] = useState(false);
  const pipModeRef = useRef(null);
  pipModeRef.current = pipMode;
  const pipActiveRef = useRef(false);
  pipActiveRef.current = pipActive;
  const [viewportSize, setViewportSize] = useState({ width: 0, height: 0 });
  const [videoDims, setVideoDims] = useState({ width: 0, height: 0 });
  const [zoomMode, setZoomMode] = useState('fit');
  const [pinchScale, setPinchScale] = useState(null);
  const [zoomSettling, setZoomSettling] = useState(false);
  const [zoomChip, setZoomChip] = useState(null);
  const [holdActive, setHoldActive] = useState(false);
  const [holdPillMounted, setHoldPillMounted] = useState(false);
  const [scrubHud, setScrubHud] = useState(null);

  const zoomMetrics = useMemo(
    () => computeVideoZoomMetrics(videoDims.width, videoDims.height, viewportSize.width, viewportSize.height),
    [videoDims.width, videoDims.height, viewportSize.width, viewportSize.height],
  );

  const fsState = fullscreenState || { active: false, source: null, pushedHistory: false, inFlight: false };
  const isFullscreen = Boolean(fsState.active && (fsState.source === 'button' || fsState.source === 'rotation'));
  const fullscreenSource = isFullscreen ? fsState.source : null;
  const isFullscreenRef = useRef(isFullscreen);
  isFullscreenRef.current = isFullscreen;
  const fullscreenSourceRef = useRef(fullscreenSource);
  fullscreenSourceRef.current = fullscreenSource;
  const turnWatcherRef = useRef(null);
  const prevIsFullscreenRef = useRef(isFullscreen);
  const prevFsInFlightRef = useRef(Boolean(fsState.inFlight));
  const activeSeekSeqRef = useRef(0);
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
  const currentTimeRef = useRef(currentTime);
  currentTimeRef.current = currentTime;
  const mediaUrlRef = useRef(mediaUrl);
  mediaUrlRef.current = mediaUrl;
  const prevIdentityRef = useRef(identity);
  const ctrlPlayingRef = useRef(ctrl.playing);
  ctrlPlayingRef.current = ctrl.playing;
  const wantPlayAfterSwapRef = useRef(false);
  const wasPlayingBeforeTeardownRef = useRef(false);
  const [pendingPlayIntent, setPendingPlayIntent] = useState(null);
  const pendingPlayIntentRef = useRef(null);
  pendingPlayIntentRef.current = pendingPlayIntent;
  const playbackRateRef = useRef(playbackRate);
  playbackRateRef.current = playbackRate;
  const zoomMetricsRef = useRef(zoomMetrics);
  zoomMetricsRef.current = zoomMetrics;
  const zoomModeRef = useRef(zoomMode);
  zoomModeRef.current = zoomMode;
  const zoomAutoAppliedRef = useRef(false);
  const zoomSettleTimerRef = useRef(null);
  const zoomChipTimerRef = useRef(null);
  const holdActiveRef = useRef(false);
  const holdFadeTimerRef = useRef(null);
  const ctrlRef = useRef(ctrl);
  ctrlRef.current = ctrl;
  const recognizerRef = useRef(null);
  const spaceHoldRef = useRef(null);
  const linkRef = useRef(link);
  linkRef.current = link;

  const commitSeekOnMedia = useCallback((targetTime, seq) => {
    activeSeekSeqRef.current = seq;
    const v = videoRef.current;
    if (!v) return;
    const dur = Number.isFinite(v.duration) && v.duration > 0 ? v.duration : durationRef.current;
    const clamped = Math.max(0, dur > 0 ? Math.min(dur, targetTime) : targetTime);
    setCurrentTime(clamped);
    if (onBeforeSeekRef.current?.(clamped, v) === true) {
      return;
    }
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

  // Reset per-video state synchronously on file change so carry-over play intent is ready before
  // `<video>` teardown and before any keyboard input can arrive on the next file.
  useLayoutEffect(() => {
    const prevId = prevIdentityRef.current;
    if (prevId && (prevId.machine !== machine || prevId.path !== path)) {
      if (durationRef.current > 0) {
        const sameFileLocalRename =
          prevId.path === path &&
          (prevId.machine === 'local' || prevId.machine === 'This computer') &&
          machine &&
          machine !== 'local' &&
          machine !== 'This computer';
        resumeStore.save(
          sameFileLocalRename ? { ...prevId, machine } : prevId,
          currentTimeRef.current,
          durationRef.current,
        );
      }
      const v = videoRef.current;
      const elPlaying = Boolean(v && !v.paused && !v.ended);
      const carryPlay = Boolean(
        pendingPlayIntentRef.current != null
          ? pendingPlayIntentRef.current
          : ctrlPlayingRef.current || elPlaying || wasPlayingBeforeTeardownRef.current,
      );
      wasPlayingBeforeTeardownRef.current = false;
      wantPlayAfterSwapRef.current = carryPlay;
      pendingPlayIntentRef.current = carryPlay ? true : null;
      setPendingPlayIntent(carryPlay ? true : null);
      prevIdentityRef.current = identityRef.current;
    } else {
      prevIdentityRef.current = identityRef.current;
      wantPlayAfterSwapRef.current = false;
      pendingPlayIntentRef.current = null;
      setPendingPlayIntent(null);
    }
    resumedAppliedRef.current = false;
    zoomAutoAppliedRef.current = false;
    recognizerRef.current?.reset?.();
    spaceHoldRef.current?.cancel?.();
    setVideoDims({ width: 0, height: 0 });
    setZoomMode('fit');
    setPinchScale(null);
    setZoomSettling(false);
    setZoomChip(null);
    setScrubHud(null);
    setCurrentTime(0);
    setDuration(entry?.duration_s > 0 ? entry.duration_s : 0);
    setBufferedEnd(0);
    setSpeedOpen(false);
    controller.setDragging(false);
    controller.setError(false);
    controller.setLoading(true);
    controller.dismissResumeChip();
  }, [machine, path, controller, resumeStore]);

  // Sync `<video>` src cleanly via layout effect so Preact never writes `src=""` while the next
  // file's URL is being minted.
  useLayoutEffect(() => {
    const v = videoRef.current;
    if (!v) return;
    if (mediaUrl) {
      wasPlayingBeforeTeardownRef.current = false;
      if (v.getAttribute('src') !== mediaUrl) {
        v.setAttribute('src', mediaUrl);
      }
    } else if (v.hasAttribute('src')) {
      wasPlayingBeforeTeardownRef.current = Boolean(!v.paused && !v.ended);
      try {
        v.pause();
        v.removeAttribute('src');
        v.load();
      } catch {
        // Ignore teardown errors.
      }
    }
  }, [mediaUrl]);

  // When a newly minted mediaUrl arrives for the current file, clear error and enter loading until metadata arrives.
  useEffect(() => {
    if (!mediaUrl) {
      controller.setLoading(true);
    } else if (controller.getState().error) {
      controller.setError(false);
      controller.setLoading(true);
    }
  }, [mediaUrl, controller]);

  // Re-evaluate full-screen and picture-in-picture availability once `<video>` and `.viewer` have mounted.
  useEffect(() => {
    if (typeof link?.canImmersive === 'function') {
      setCanFs(Boolean(link.canImmersive({ element: viewerRef?.current, video: videoRef.current })));
    } else {
      setCanFs(false);
    }
    const updatePipSupport = () => {
      setPipMode(detectVideoPipSupport({ link, video: videoRef.current }));
    };
    updatePipSupport();
    const unsub = link?.nativePip?.onSupportChange?.(() => updatePipSupport());
    return () => {
      if (typeof unsub === 'function') unsub();
    };
  }, [link, viewerRef, mediaUrl]);

  useLayoutEffect(() => {
    onPipActiveChangeRef.current?.(pipActive);
  }, [pipActive]);

  // Sync `inert` property on overlay container so hidden controls can never be focused or pressed.
  useEffect(() => {
    const el = overlayRef.current;
    if (!el) return;
    el.inert = !ctrl.visible || pipActive;
    if ((!ctrl.visible || pipActive) && document.activeElement && el.contains(document.activeElement)) {
      try {
        document.activeElement.blur();
      } catch {
        // Ignore blur errors.
      }
    }
  }, [ctrl.visible, pipActive]);

  // Keep overlay visible while More sheet or speed popover is open.
  useEffect(() => {
    controller.setMenuOpen(Boolean(moreOpen || speedOpen));
  }, [moreOpen, speedOpen, controller]);

  // Screen stays on while video plays (including across a file swap while playing); released on pause/end/close.
  const effectivePlaying = pendingPlayIntent != null ? pendingPlayIntent : ctrl.playing;
  const effectivePlayingRef = useRef(effectivePlaying);
  effectivePlayingRef.current = effectivePlaying;
  const holdScreenOn = Boolean(effectivePlaying);
  useEffect(() => {
    link?.keepScreenOn?.(holdScreenOn);
  }, [link, holdScreenOn]);

  // On mobile native apps, hiding the overlay in portrait hides system bars too ("one state")
  // through the viewer's single full-screen controller.
  useEffect(() => {
    if (!mobileNative || isFullscreen || pipActive || !fullscreenController) {
      return;
    }
    if (!ctrl.visible) {
      fullscreenController.enter('bars', { orientation: 'none' });
    } else if (fullscreenController.getState().source === 'bars') {
      fullscreenController.leave();
    }
  }, [mobileNative, isFullscreen, pipActive, ctrl.visible, fullscreenController]);

  const enterFullscreen = useCallback(
    async (source = 'button') => {
      if (!fullscreenController) return;
      const v = videoRef.current;
      const orientation =
        source === 'button' ? preferredVideoOrientation(v?.videoWidth, v?.videoHeight) : 'none';
      await fullscreenController.enter(source, { orientation });
    },
    [fullscreenController],
  );

  const leaveFullscreen = useCallback(async () => {
    turnWatcherRef.current?.noteLeave();
    controller.notifyActivity({ show: true });
    if (fullscreenController) {
      await fullscreenController.leave();
    }
  }, [controller, fullscreenController]);

  // Engage the post-leave turn settle guard even when full screen ends externally (Back, Esc, sheets).
  useEffect(() => {
    if (prevIsFullscreenRef.current && !isFullscreen) {
      turnWatcherRef.current?.noteLeave();
    }
    prevIsFullscreenRef.current = isFullscreen;
  }, [isFullscreen]);

  // Re-evaluate pending orientation readings as soon as an in-flight full-screen transition finishes.
  useEffect(() => {
    const inFlight = Boolean(fsState.inFlight);
    if (prevFsInFlightRef.current && !inFlight) {
      turnWatcherRef.current?.notifySettled();
    }
    prevFsInFlightRef.current = inFlight;
  }, [fsState.inFlight]);

  // Report iOS Safari `<video>` webkitendfullscreen back to the single controller.
  useEffect(() => {
    const v = videoRef.current;
    if (!v || !fullscreenController) return undefined;
    const onWebkitEnd = () => fullscreenController.syncFromPlatform(false);
    v.addEventListener?.('webkitendfullscreen', onWebkitEnd);
    return () => v.removeEventListener?.('webkitendfullscreen', onWebkitEnd);
  }, [fullscreenController, mediaUrl]);

  // Turning a phone or tablet to landscape enters full screen; turning back leaves it if turning entered it.
  useEffect(() => {
    if (!touchOrientationDevice || !fullscreenController) return undefined;

    const hasScreenOrientation = typeof window.screen?.orientation?.addEventListener === 'function';
    const hasOrientationChange = 'onorientationchange' in window;
    const hasOrientationEvent = hasScreenOrientation || hasOrientationChange;

    const readLandscape = () => {
      const orientationType = window.screen?.orientation?.type;
      if (typeof orientationType === 'string' && orientationType) {
        return /^landscape/i.test(orientationType);
      }
      return window.innerWidth > window.innerHeight;
    };

    const watcher = createTurnWatcher({
      clock: getPlayerClock(),
      hasOrientationEvent,
      isLandscape: readLandscape,
      isFullscreen: () => isFullscreenRef.current,
      getFullscreenSource: () => fullscreenSourceRef.current,
      isInFlight: () => Boolean(fullscreenController.getState().inFlight),
      onEnterRotation: () => enterFullscreen('rotation'),
      onLeaveRotation: () => leaveFullscreen(),
    });
    turnWatcherRef.current = watcher;
    watcher.start();

    const onOrientation = () => watcher.onOrientationEvent();
    const onResize = () => watcher.onResize();

    if (hasScreenOrientation) {
      window.screen.orientation.addEventListener('change', onOrientation);
    }
    if (hasOrientationChange) {
      window.addEventListener('orientationchange', onOrientation);
    }
    if (!hasOrientationEvent) {
      window.addEventListener('resize', onResize);
    }
    return () => {
      turnWatcherRef.current = null;
      watcher.dispose();
      if (hasScreenOrientation) {
        window.screen.orientation.removeEventListener('change', onOrientation);
      }
      if (hasOrientationChange) {
        window.removeEventListener('orientationchange', onOrientation);
      }
      if (!hasOrientationEvent) {
        window.removeEventListener('resize', onResize);
      }
    };
  }, [touchOrientationDevice, fullscreenController, enterFullscreen, leaveFullscreen]);

  // Pause in place and clear pending autoplay if the desktop window is hidden/closed.
  useEffect(() => {
    if (typeof link?.onWindowHidden !== 'function') return undefined;
    return link.onWindowHidden(() => {
      recognizerRef.current?.reset?.();
      spaceHoldRef.current?.cancel?.();
      wantPlayAfterSwapRef.current = false;
      pendingPlayIntentRef.current = null;
      setPendingPlayIntent(null);
      const v = videoRef.current;
      if (v && !v.paused) {
        try {
          v.pause();
        } catch {
          // Ignore pause errors.
        }
      }
      controller.setPlaying(false);
      controller.notifyActivity({ show: true });
    });
  }, [link, controller]);

  // Measure the player viewport continuously for zoom-to-fill geometry.
  useEffect(() => {
    const el = wrapRef.current;
    if (!el) return undefined;
    const measure = () => {
      const rect = el.getBoundingClientRect();
      if (rect.width > 0 && rect.height > 0) {
        setViewportSize((prev) =>
          Math.abs(prev.width - rect.width) < 0.5 && Math.abs(prev.height - rect.height) < 0.5
            ? prev
            : { width: rect.width, height: rect.height },
        );
      }
    };
    measure();
    if (typeof ResizeObserver === 'function') {
      const ro = new ResizeObserver(measure);
      ro.observe(el);
      return () => ro.disconnect();
    }
    window.addEventListener('resize', measure);
    return () => window.removeEventListener('resize', measure);
  }, []);

  const scheduleClockTimeout = useCallback((fn, ms) => {
    const c = getPlayerClock();
    if (c?.setTimeout) return { clock: c, id: c.setTimeout(fn, ms) };
    return { clock: null, id: window.setTimeout(fn, ms) };
  }, []);

  const clearClockTimeout = useCallback((handle) => {
    if (!handle) return;
    if (handle.clock?.clearTimeout) handle.clock.clearTimeout(handle.id);
    else window.clearTimeout(handle.id);
  }, []);

  const showZoomChipFor = useCallback(
    (label) => {
      clearClockTimeout(zoomChipTimerRef.current);
      setZoomChip(label);
      zoomChipTimerRef.current = scheduleClockTimeout(() => {
        zoomChipTimerRef.current = null;
        setZoomChip(null);
      }, ZOOM_CHIP_DURATION_MS);
    },
    [clearClockTimeout, scheduleClockTimeout],
  );

  const runZoomSettleAnimation = useCallback(() => {
    clearClockTimeout(zoomSettleTimerRef.current);
    setZoomSettling(true);
    zoomSettleTimerRef.current = scheduleClockTimeout(() => {
      zoomSettleTimerRef.current = null;
      setZoomSettling(false);
    }, ZOOM_SETTLE_MS);
  }, [clearClockTimeout, scheduleClockTimeout]);

  const commitZoomMode = useCallback(
    (nextMode, { animate = true, announce = true, haptic = true } = {}) => {
      const target = nextMode === 'fill' && isFullscreenRef.current && zoomMetricsRef.current.available ? 'fill' : 'fit';
      setPinchScale(null);
      setZoomMode(target);
      zoomModeRef.current = target;
      writeZoomPreference(safeStorage(), target);
      if (animate) runZoomSettleAnimation();
      if (announce) {
        showZoomChipFor(target === 'fill' ? 'Zoomed to fill' : 'Original');
      }
      if (haptic) {
        linkRef.current?.haptic?.('tick');
      }
    },
    [runZoomSettleAnimation, showZoomChipFor],
  );

  const handleToggleZoom = useCallback(() => {
    if (!isFullscreenRef.current || !zoomMetricsRef.current.available) return;
    const next = zoomModeRef.current === 'fill' ? 'fit' : 'fill';
    commitZoomMode(next, { animate: true, announce: true, haptic: true });
  }, [commitZoomMode]);

  // Reset zoom outside full screen; auto-apply remembered fill on entering full screen iff <= 15% crop.
  useLayoutEffect(() => {
    if (!isFullscreen) {
      zoomAutoAppliedRef.current = false;
      setPinchScale(null);
      setZoomSettling(false);
      setZoomMode('fit');
      zoomModeRef.current = 'fit';
      return;
    }
    if (videoDims.width <= 0 || videoDims.height <= 0 || viewportSize.width <= 0 || viewportSize.height <= 0) {
      return;
    }
    if (!zoomMetrics.available) {
      setZoomMode('fit');
      zoomModeRef.current = 'fit';
      return;
    }
    if (!zoomAutoAppliedRef.current) {
      zoomAutoAppliedRef.current = true;
      const remembered = readZoomPreference(safeStorage());
      const next = remembered === 'fill' && zoomMetrics.canAutoApplyFill ? 'fill' : 'fit';
      setZoomMode(next);
      zoomModeRef.current = next;
    }
  }, [isFullscreen, videoDims.width, videoDims.height, viewportSize.width, viewportSize.height, zoomMetrics.available, zoomMetrics.canAutoApplyFill]);

  // Expose zoom action to FilesViewer's More sheet under the exact same conditions as Z.
  useEffect(() => {
    if (typeof onZoomActionChangeRef.current !== 'function') return;
    if (isFullscreen && zoomMetrics.available) {
      onZoomActionChangeRef.current({
        available: true,
        mode: zoomMode,
        toggle: handleToggleZoom,
      });
    } else {
      onZoomActionChangeRef.current(null);
    }
  }, [isFullscreen, zoomMetrics.available, zoomMode, handleToggleZoom]);

  const startHold2x = useCallback(() => {
    if (holdActiveRef.current) return;
    const v = videoRef.current;
    const st = ctrlRef.current;
    if (!v || !mediaUrlRef.current || v.paused || v.ended || st.loading || st.error) return;
    if (playbackRateRef.current >= HOLD_PLAYBACK_RATE) return;
    clearClockTimeout(holdFadeTimerRef.current);
    holdFadeTimerRef.current = null;
    holdActiveRef.current = true;
    setHoldActive(true);
    setHoldPillMounted(true);
    v.playbackRate = HOLD_PLAYBACK_RATE;
    linkRef.current?.haptic?.('tick');
  }, [clearClockTimeout]);

  const endHold2x = useCallback(() => {
    if (!holdActiveRef.current) return;
    holdActiveRef.current = false;
    setHoldActive(false);
    const v = videoRef.current;
    if (v) {
      v.playbackRate = playbackRateRef.current;
    }
    clearClockTimeout(holdFadeTimerRef.current);
    holdFadeTimerRef.current = scheduleClockTimeout(() => {
      holdFadeTimerRef.current = null;
      setHoldPillMounted(false);
    }, HOLD_PILL_FADE_MS);
  }, [clearClockTimeout, scheduleClockTimeout]);

  // Always release keep-screen-on, PiP state, timers, gestures, resume position, and `<video>` on unmount.
  useEffect(() => {
    return () => {
      onZoomActionChangeRef.current?.(null);
      onPipActiveChangeRef.current?.(false);
      link?.nativePip?.sync?.({ active: false, playing: false, width: 0, height: 0, rect: null });
      recognizerRef.current?.dispose?.();
      spaceHoldRef.current?.dispose?.();
      clearClockTimeout(zoomSettleTimerRef.current);
      clearClockTimeout(zoomChipTimerRef.current);
      clearClockTimeout(holdFadeTimerRef.current);
      const v = videoRef.current;
      if (
        typeof document !== 'undefined' &&
        v &&
        document.pictureInPictureElement === v &&
        typeof document.exitPictureInPicture === 'function'
      ) {
        document.exitPictureInPicture().catch(() => {});
      }
      if (v && v.webkitPresentationMode === 'picture-in-picture' && typeof v.webkitSetPresentationMode === 'function') {
        try {
          v.webkitSetPresentationMode('inline');
        } catch {
          // Ignore WebKit PiP exit errors on unmount.
        }
      }
      if (v && Number.isFinite(v.duration) && v.duration > 0) {
        resumeStore.save(identityRef.current, v.currentTime, v.duration);
      }
      link?.keepScreenOn?.(false);
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
  }, [link, resumeStore, clearClockTimeout]);

  const togglePlay = useCallback(
    ({ showOverlay = true } = {}) => {
      const v = videoRef.current;
      if (showOverlay) {
        controller.notifyActivity({ show: true });
      }
      if (
        v &&
        mediaUrlRef.current &&
        (v.paused || v.ended || v.error || v.readyState < 1) &&
        onBeforePlayRef.current?.(v) === true
      ) {
        wantPlayAfterSwapRef.current = true;
        pendingPlayIntentRef.current = true;
        setPendingPlayIntent(true);
        controller.setError(false);
        controller.setLoading(true);
        return;
      }
      const loadingNext = !v || !mediaUrlRef.current || v.readyState < 1;
      if (loadingNext) {
        const nextWant = !wantPlayAfterSwapRef.current;
        wantPlayAfterSwapRef.current = nextWant;
        pendingPlayIntentRef.current = nextWant;
        setPendingPlayIntent(nextWant);
        return;
      }
      wantPlayAfterSwapRef.current = false;
      pendingPlayIntentRef.current = null;
      setPendingPlayIntent(null);
      if (v.paused || v.ended) {
        if (v.ended) {
          try {
            v.currentTime = 0;
          } catch {
            // Ignore seek errors.
          }
        }
        linkRef.current?.iosAudio?.activateSession?.();
        v.play().catch((err) => {
          if (err?.name !== 'NotAllowedError' && (v.error || v.networkState === 3)) {
            onMediaErrorRef.current?.(v);
          }
        });
      } else {
        v.pause();
        controller.setPlaying(false);
      }
    },
    [controller],
  );

  const seekBy = useCallback(
    (delta, { showOverlay = true } = {}) => {
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
      if (showOverlay) {
        controller.notifyActivity({ show: true });
      }
      controller.beginSeek(next);
    },
    [controller],
  );

  const syncAndroidPipConfig = useCallback(() => {
    if (!link?.nativePip) return;
    const v = videoRef.current;
    const st = ctrlRef.current;
    const active = Boolean(mediaUrlRef.current && !st.error && !st.loading);
    const playing = Boolean(active && effectivePlayingRef.current && !st.ended);
    const width = (v?.videoWidth > 0 ? v.videoWidth : videoDims.width) || 0;
    const height = (v?.videoHeight > 0 ? v.videoHeight : videoDims.height) || 0;
    const rect =
      active && v && !pipActiveRef.current
        ? computeRenderedVideoRect(v.getBoundingClientRect(), width, height)
        : null;
    link.nativePip.sync({ active, playing, width, height, rect });
  }, [link, videoDims.width, videoDims.height]);

  useEffect(() => {
    if (pipMode !== 'android' || !link?.nativePip) return;
    syncAndroidPipConfig();
  }, [
    pipMode,
    link,
    mediaUrl,
    effectivePlaying,
    ctrl.loading,
    ctrl.ended,
    ctrl.error,
    videoDims.width,
    videoDims.height,
    viewportSize.width,
    viewportSize.height,
    isFullscreen,
    pipActive,
    syncAndroidPipConfig,
  ]);

  useEffect(() => {
    if (!link?.nativePip) return undefined;
    const unsubChange = link.nativePip.onChange(({ active, dismissed }) => {
      if (active) {
        setPipActive(true);
        setSpeedOpen(false);
        endHold2x();
        recognizerRef.current?.reset?.();
        spaceHoldRef.current?.cancel?.();
      } else {
        setPipActive(false);
        if (dismissed) {
          endHold2x();
          recognizerRef.current?.reset?.();
          spaceHoldRef.current?.cancel?.();
          wantPlayAfterSwapRef.current = false;
          pendingPlayIntentRef.current = null;
          setPendingPlayIntent(null);
          const v = videoRef.current;
          if (v && !v.paused) {
            try {
              v.pause();
            } catch {
              // Ignore pause errors on dismissed PiP.
            }
          }
          controller.setPlaying(false);
          link.keepScreenOn?.(false);
          const revokePromise = onMediaPipDismissRef.current?.(v);
          Promise.resolve(revokePromise).finally(() => {
            link.nativePip?.ackDismiss?.();
          });
        } else {
          controller.notifyActivity({ show: true });
        }
      }
    });
    const unsubAction = link.nativePip.onAction(({ action }) => {
      if (action === 'seek_back_10') {
        seekBy(-SEEK_STEP_S, { showOverlay: false });
      } else if (action === 'toggle_play') {
        togglePlay({ showOverlay: false });
      } else if (action === 'seek_fwd_10') {
        seekBy(SEEK_STEP_S, { showOverlay: false });
      }
    });
    return () => {
      unsubChange?.();
      unsubAction?.();
    };
  }, [link, controller, endHold2x, seekBy, togglePlay]);

  // Standard Web Picture-in-Picture and Safari/iOS WebKit presentation-mode events.
  useEffect(() => {
    const v = videoRef.current;
    if (!v) return undefined;
    const onEnterWebPip = () => {
      setPipActive(true);
      setSpeedOpen(false);
      endHold2x();
    };
    const onLeaveWebPip = () => {
      setPipActive(false);
      controller.notifyActivity({ show: true });
    };
    const onWebkitPresentation = () => {
      const inPip = v.webkitPresentationMode === 'picture-in-picture';
      setPipActive(inPip);
      if (inPip) {
        setSpeedOpen(false);
        endHold2x();
      } else {
        controller.notifyActivity({ show: true });
      }
    };
    v.addEventListener('enterpictureinpicture', onEnterWebPip);
    v.addEventListener('leavepictureinpicture', onLeaveWebPip);
    v.addEventListener('webkitpresentationmodechanged', onWebkitPresentation);
    return () => {
      v.removeEventListener('enterpictureinpicture', onEnterWebPip);
      v.removeEventListener('leavepictureinpicture', onLeaveWebPip);
      v.removeEventListener('webkitpresentationmodechanged', onWebkitPresentation);
    };
  }, [controller, endHold2x, mediaUrl]);

  const handleTogglePip = useCallback(async () => {
    const mode = pipModeRef.current;
    const v = videoRef.current;
    if (!mode || !v) return;
    setSpeedOpen(false);
    if (mode === 'android') {
      syncAndroidPipConfig();
      await link?.nativePip?.enter?.();
      return;
    }
    if (mode === 'web') {
      try {
        if (typeof document !== 'undefined' && document.pictureInPictureElement === v) {
          await document.exitPictureInPicture();
        } else {
          await v.requestPictureInPicture();
        }
      } catch {
        // Ignore platform rejections (e.g. metadata still loading).
      }
      return;
    }
    if (mode === 'webkit') {
      try {
        const nextMode = v.webkitPresentationMode === 'picture-in-picture' ? 'inline' : 'picture-in-picture';
        v.webkitSetPresentationMode(nextMode);
      } catch {
        // Ignore WebKit presentation mode rejections.
      }
    }
  }, [link, syncAndroidPipConfig]);

  // Desktop & Browser MediaSession metadata and actions while video plays.
  useEffect(() => {
    if (mobileNative || typeof navigator === 'undefined' || !navigator.mediaSession) return undefined;
    const ms = navigator.mediaSession;
    const setHandler = (action, fn) => {
      try {
        ms.setActionHandler(action, fn);
      } catch {
        // Ignore unsupported actions in some browsers.
      }
    };
    if (!mediaUrl) return undefined;
    if (typeof MediaMetadata === 'function') {
      try {
        ms.metadata = new MediaMetadata({ title: name || '' });
      } catch {
        // Ignore MediaMetadata errors.
      }
    }
    try {
      ms.playbackState = effectivePlaying ? 'playing' : 'paused';
    } catch {
      // Ignore read-only playbackState errors.
    }
    setHandler('play', () => {
      const v = videoRef.current;
      if (v && (v.paused || v.ended)) togglePlay();
    });
    setHandler('pause', () => {
      const v = videoRef.current;
      if (v && !v.paused) togglePlay();
    });
    setHandler('seekbackward', (details) => {
      const offset = Number.isFinite(details?.seekOffset) && details.seekOffset > 0 ? details.seekOffset : SEEK_STEP_S;
      seekBy(-offset);
    });
    setHandler('seekforward', (details) => {
      const offset = Number.isFinite(details?.seekOffset) && details.seekOffset > 0 ? details.seekOffset : SEEK_STEP_S;
      seekBy(offset);
    });
    setHandler('seekto', (details) => {
      if (typeof details?.seekTime === 'number' && Number.isFinite(details.seekTime)) {
        controller.beginSeek(details.seekTime);
      }
    });
    setHandler('previoustrack', queue.prev && onNavigateSibling ? () => onNavigateSibling(queue.prev) : null);
    setHandler('nexttrack', queue.next && onNavigateSibling ? () => onNavigateSibling(queue.next) : null);
    return () => {
      for (const action of ['play', 'pause', 'seekbackward', 'seekforward', 'seekto', 'previoustrack', 'nexttrack']) {
        setHandler(action, null);
      }
      try {
        ms.playbackState = 'none';
      } catch {
        // Ignore cleanup errors.
      }
    };
  }, [mobileNative, mediaUrl, name, effectivePlaying, queue.prev, queue.next, togglePlay, seekBy, controller, onNavigateSibling]);

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

  const applyRate = useCallback(
    (rate) => {
      setPlaybackRate(rate);
      playbackRateRef.current = rate;
      setSpeedOpen(false);
      if (rate >= HOLD_PLAYBACK_RATE && holdActiveRef.current) {
        endHold2x();
      }
      const v = videoRef.current;
      if (v && !holdActiveRef.current) v.playbackRate = rate;
    },
    [endHold2x],
  );

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

  // Desktop Space hold-for-2x controller (short press toggles play/pause; > 350 ms holds 2x).
  if (!spaceHoldRef.current) {
    spaceHoldRef.current = createSpaceHoldController({
      clock: getPlayerClock(),
      canHold: () => {
        const v = videoRef.current;
        const st = ctrlRef.current;
        return Boolean(
          v &&
            mediaUrlRef.current &&
            v.readyState >= 1 &&
            !v.paused &&
            !v.ended &&
            !st.loading &&
            !st.error &&
            playbackRateRef.current < HOLD_PLAYBACK_RATE,
        );
      },
      onTogglePlay: () => {
        controller.notifyActivity({ show: true });
        togglePlay();
      },
      onHoldStart: () => startHold2x(),
      onHoldEnd: () => endHold2x(),
    });
  }

  // Keyboard shortcuts (desktop & browser).
  useEffect(() => {
    function onKeyDown(e) {
      if (moreOpenRef.current) return;
      const hasScrim = Boolean(document.querySelector('.scrim'));
      if (e.key === 'Tab' && !hasScrim) {
        const overlay = overlayRef.current;
        if (overlay && overlay.inert) {
          e.preventDefault();
          e.stopImmediatePropagation?.();
          overlay.inert = false;
          controller.notifyActivity({ show: true });
          controller.setFocusInside(true);
          const first = overlay.querySelector(
            'button:not([disabled]), [role="slider"], [tabindex="0"], a[href]',
          );
          first?.focus?.();
          return;
        }
      }
      if (speedOpenRef.current && e.key === 'Escape') {
        e.preventDefault();
        setSpeedOpen(false);
        return;
      }
      const action = classifyPlayerKey(e, {
        mode: 'video',
        isFullscreen: isFullscreenRef.current,
        hasOpenOverlay: hasScrim,
      });
      if (!action) return;
      e.preventDefault();
      e.stopImmediatePropagation?.();

      if (action === 'toggle_play' && (e.key === ' ' || e.key === 'Spacebar')) {
        spaceHoldRef.current?.onKeyDown({ repeat: Boolean(e.repeat) });
        return;
      }
      if (action === 'toggle_zoom') {
        if (isFullscreenRef.current && zoomMetricsRef.current.available) {
          handleToggleZoom();
        }
        return;
      }

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
          if (e.key === 'Escape' && fullscreenController?.recentlyExitedFromPlatform?.()) break;
          if (onClose) onClose();
          break;
        default:
          break;
      }
    }

    function onKeyUp(e) {
      if (e.key === ' ' || e.key === 'Spacebar') {
        spaceHoldRef.current?.onKeyUp();
      }
    }

    function onWindowBlur() {
      spaceHoldRef.current?.cancel();
      recognizerRef.current?.reset();
    }

    window.addEventListener('keydown', onKeyDown, true);
    window.addEventListener('keyup', onKeyUp, true);
    window.addEventListener('blur', onWindowBlur);
    return () => {
      window.removeEventListener('keydown', onKeyDown, true);
      window.removeEventListener('keyup', onKeyUp, true);
      window.removeEventListener('blur', onWindowBlur);
    };
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
    handleToggleZoom,
    onNavigateSibling,
    leaveFullscreen,
    onClose,
    fullscreenController,
  ]);

  // Single unified pointer recogniser for the video picture surface:
  // tap / double-tap, pinch zoom-to-fill, press-and-hold 2x, and horizontal scrub anywhere.
  if (!recognizerRef.current) {
    recognizerRef.current = createPictureGestureRecognizer({
      clock: getPlayerClock(),
      edgeMarginPx: GESTURE_EDGE_MARGIN_PX,
      getBounds: () => {
        const rect = wrapRef.current?.getBoundingClientRect?.() || { left: 0, top: 0, width: window.innerWidth, height: window.innerHeight };
        let safeBottom = 0;
        try {
          safeBottom = parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--safe-area-inset-bottom')) || 0;
        } catch {
          safeBottom = 0;
        }
        return {
          left: rect.left,
          top: rect.top,
          width: rect.width,
          height: rect.height,
          bottomMarginPx: Math.max(GESTURE_EDGE_MARGIN_PX, safeBottom),
        };
      },
      isLayerVisible: () => Boolean(ctrlRef.current.visible),
      canHold: () => {
        const v = videoRef.current;
        const st = ctrlRef.current;
        return Boolean(
          v &&
            mediaUrlRef.current &&
            !v.paused &&
            !v.ended &&
            !st.loading &&
            !st.error &&
            playbackRateRef.current < HOLD_PLAYBACK_RATE,
        );
      },
      canScrub: () => {
        const v = videoRef.current;
        const dur = v && Number.isFinite(v.duration) && v.duration > 0 ? v.duration : durationRef.current;
        return Boolean(mediaUrlRef.current && Number.isFinite(dur) && dur > 0);
      },
      canPinch: () => Boolean(isFullscreenRef.current && zoomMetricsRef.current.available),
      getZoomFillScale: () => zoomMetricsRef.current.fillScale,
      getZoomMode: () => zoomModeRef.current,
      getScrubBarWidth: () => {
        const track = scrubBarWrapRef.current?.querySelector?.('.player-scrub-track');
        const trackRect = track?.getBoundingClientRect?.();
        if (trackRect && trackRect.width > 0) return trackRect.width;
        const wrapRect = wrapRef.current?.getBoundingClientRect?.();
        return wrapRect && wrapRect.width > 0 ? wrapRect.width : 1;
      },
      getCurrentTime: () => {
        const st = controllerRef.current?.getState?.();
        if (st?.pendingSeekTime != null) return st.pendingSeekTime;
        const v = videoRef.current;
        return v && Number.isFinite(v.currentTime) ? v.currentTime : currentTimeRef.current;
      },
      getDuration: () => {
        const v = videoRef.current;
        return v && Number.isFinite(v.duration) && v.duration > 0 ? v.duration : durationRef.current;
      },
      onTap: (zone) => {
        controller.handlePictureTap(zone);
      },
      onHoldStart: () => startHold2x(),
      onHoldEnd: () => endHold2x(),
      onScrubStart: (p) => {
        controller.setDragging(true);
        setScrubHud({ time: p.time, delta: p.delta, rate: p.rate, rateLabel: p.rateLabel });
      },
      onScrubMove: (p) => {
        setScrubHud({ time: p.time, delta: p.delta, rate: p.rate, rateLabel: p.rateLabel });
        if (p.rateChanged) {
          linkRef.current?.haptic?.('tick');
        }
      },
      onScrubEnd: (p) => {
        setScrubHud(null);
        controller.setDragging(false);
        if (p.committed) {
          controller.beginSeek(p.time);
        }
      },
      onScrubCancel: () => {
        setScrubHud(null);
        controller.setDragging(false);
      },
      onPinchStart: ({ startScale }) => {
        clearClockTimeout(zoomSettleTimerRef.current);
        setZoomSettling(false);
        setPinchScale(startScale);
      },
      onPinchMove: ({ scale }) => {
        setPinchScale(scale);
      },
      onPinchEnd: ({ targetMode, changed }) => {
        if (changed) {
          commitZoomMode(targetMode, { animate: true, announce: true, haptic: true });
        } else {
          setPinchScale(null);
          runZoomSettleAnimation();
        }
      },
      onPinchCancel: () => {
        setPinchScale(null);
        runZoomSettleAnimation();
      },
    });
  }

  function onStagePointerDown(e) {
    if (e.button > 0) return;
    if (speedOpen) {
      setSpeedOpen(false);
      return;
    }
    const accepted = recognizerRef.current?.onPointerDown(e);
    if (accepted) {
      try {
        e.currentTarget.setPointerCapture?.(e.pointerId);
      } catch {
        // Ignore synthetic pointer events without an active capture id.
      }
    }
  }

  function onStagePointerMove(e) {
    const recState = recognizerRef.current?.getState?.();
    if (e.pointerType === 'mouse' && e.buttons === 0 && recState?.state === 'idle') {
      controller.notifyActivity({ show: true });
      return;
    }
    recognizerRef.current?.onPointerMove(e);
  }

  function onStagePointerUp(e) {
    recognizerRef.current?.onPointerUp(e);
    try {
      e.currentTarget.releasePointerCapture?.(e.pointerId);
    } catch {
      // Ignore synthetic pointer events.
    }
  }

  function onStagePointerCancel(e) {
    recognizerRef.current?.onPointerCancel(e);
  }

  const displayTime = scrubHud
    ? scrubHud.time
    : ctrl.pendingSeekTime != null
      ? ctrl.pendingSeekTime
      : currentTime;
  const activeVideoScale =
    !isFullscreen || pipActive || !zoomMetrics.available
      ? 1
      : pinchScale != null
        ? pinchScale
        : zoomMode === 'fill'
          ? zoomMetrics.fillScale
          : 1;
  const effectiveZoomMode = isFullscreen && !pipActive && zoomMetrics.available ? zoomMode : 'fit';

  return html`
    <div
      ref=${wrapRef}
      class=${[
        'player-video-wrap',
        ctrl.cursorHidden ? 'cursor-hidden' : '',
        ctrl.visible ? 'overlay-visible' : '',
        isFullscreen ? 'is-fullscreen' : '',
        effectiveZoomMode === 'fill' ? 'is-zoom-fill' : '',
        pipActive ? 'pip-active' : '',
      ]
        .filter(Boolean)
        .join(' ')}
      onPointerMove=${(e) => {
        if (e.pointerType === 'mouse' && e.buttons === 0 && recognizerRef.current?.getState?.().state === 'idle') {
          controller.notifyActivity({ show: true });
        }
      }}
      onContextMenu=${(e) => e.preventDefault()}
    >
      <video
        ref=${bindVideoRef}
        class=${zoomSettling ? 'viewer-video zoom-settling' : 'viewer-video'}
        style=${activeVideoScale !== 1 ? { transform: `scale(${activeVideoScale})` } : undefined}
        data-zoom-mode=${effectiveZoomMode}
        data-zoom-scale=${activeVideoScale.toFixed(4)}
        playsinline
        preload="metadata"
        aria-label=${name}
        onResize=${(e) => {
          const v = e.currentTarget;
          if (v.videoWidth > 0 && v.videoHeight > 0) {
            setVideoDims({ width: v.videoWidth, height: v.videoHeight });
          }
        }}
        onLoadedMetadata=${(e) => {
          const v = e.currentTarget;
          if (!mediaUrlRef.current || !v.getAttribute('src')) return;
          if (v.videoWidth > 0 && v.videoHeight > 0) {
            setVideoDims({ width: v.videoWidth, height: v.videoHeight });
          }
          const dur = Number.isFinite(v.duration) && v.duration > 0 ? v.duration : entry?.duration_s || 0;
          if (dur > 0) setDuration(dur);
          setBufferedEnd(readBufferedEnd(v));
          v.playbackRate = holdActiveRef.current ? HOLD_PLAYBACK_RATE : playbackRateRef.current;
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
          const pendingIntent = pendingPlayIntentRef.current;
          const wantPlay = wantPlayAfterSwapRef.current;
          wantPlayAfterSwapRef.current = false;
          pendingPlayIntentRef.current = null;
          setPendingPlayIntent(null);
          if (!recovered) {
            if (wantPlay) {
              controller.setPlaying(true);
              v.play().catch(() => {
                controller.setPlaying(false);
              });
            } else if (pendingIntent === false) {
              if (!v.paused) {
                try {
                  v.pause();
                } catch {
                  // Ignore pause errors.
                }
              }
              controller.setPlaying(false);
            } else {
              // Preserve playback that already started before the queued loadedmetadata event fired.
              controller.setPlaying(!v.paused && !v.ended);
            }
          }
        }}
        onLoadedData=${(e) => {
          const v = e.currentTarget;
          if (!mediaUrlRef.current || !v.getAttribute('src')) return;
          if (v.videoWidth > 0 && v.videoHeight > 0) {
            setVideoDims((prev) =>
              prev.width === v.videoWidth && prev.height === v.videoHeight
                ? prev
                : { width: v.videoWidth, height: v.videoHeight },
            );
          }
          setBufferedEnd(readBufferedEnd(v));
          controller.onCanPlay();
        }}
        onCanPlay=${(e) => {
          if (!mediaUrlRef.current || !e.currentTarget.getAttribute('src')) return;
          setBufferedEnd(readBufferedEnd(e.currentTarget));
          controller.onCanPlay();
        }}
        onPlay=${(e) => {
          if (!mediaUrlRef.current || !e.currentTarget.getAttribute('src')) return;
          linkRef.current?.iosAudio?.activateSession?.();
          onMediaPlay?.(e.currentTarget);
          controller.setPlaying(true);
        }}
        onPlaying=${(e) => {
          if (!mediaUrlRef.current || !e.currentTarget.getAttribute('src')) return;
          linkRef.current?.iosAudio?.activateSession?.();
          onMediaPlay?.(e.currentTarget);
          controller.setPlaying(true);
          controller.onCanPlay();
        }}
        onPause=${(e) => {
          const v = e.currentTarget;
          if (!mediaUrlRef.current || !v.getAttribute('src')) return;
          endHold2x();
          spaceHoldRef.current?.cancel?.();
          onMediaPause?.(v);
          if (Number.isFinite(v.duration) && v.duration > 0) {
            resumeStore.save(identityRef.current, v.currentTime, v.duration);
          }
          if (!v.ended) controller.setPlaying(false);
        }}
        onWaiting=${() => {
          if (mediaUrlRef.current) controller.onWaiting();
        }}
        onStalled=${() => {
          if (mediaUrlRef.current) controller.onWaiting();
        }}
        onSeeked=${(e) => {
          const v = e.currentTarget;
          if (!mediaUrlRef.current || !v.getAttribute('src')) return;
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
          if (!mediaUrlRef.current || !v.getAttribute('src')) return;
          onMediaTime?.(v);
          if (controller.getState().pendingSeekTime != null) return;
          const t = Number.isFinite(v.currentTime) ? v.currentTime : 0;
          setCurrentTime(t);
          setBufferedEnd(readBufferedEnd(v));
          if (Number.isFinite(v.duration) && v.duration > 0) {
            const now = getPlayerClock()?.now?.() ?? Date.now();
            resumeStore.savePeriodic(identityRef.current, t, v.duration, now);
          }
        }}
        onProgress=${(e) => {
          if (mediaUrlRef.current && e.currentTarget.getAttribute('src')) {
            setBufferedEnd(readBufferedEnd(e.currentTarget));
          }
        }}
        onEnded=${(e) => {
          if (!mediaUrlRef.current || !e.currentTarget.getAttribute('src')) return;
          endHold2x();
          spaceHoldRef.current?.cancel?.();
          wantPlayAfterSwapRef.current = false;
          onMediaPause?.(e.currentTarget);
          onMediaEndedRef.current?.(e.currentTarget);
          resumeStore.clear(identityRef.current);
          controller.setEnded(true);
        }}
        onError=${(e) => {
          if (!mediaUrlRef.current || !e.currentTarget.getAttribute('src')) return;
          endHold2x();
          spaceHoldRef.current?.cancel?.();
          wantPlayAfterSwapRef.current = false;
          if (onMediaError && onMediaError(e.currentTarget) === true) return;
          controller.setError(true);
        }}
      />

      <div
        class="player-stage-hit"
        onPointerDown=${onStagePointerDown}
        onPointerMove=${onStagePointerMove}
        onPointerUp=${onStagePointerUp}
        onPointerCancel=${onStagePointerCancel}
      />

      ${holdPillMounted
        ? html`
            <div
              class=${holdActive ? 'player-hold-pill' : 'player-hold-pill fading'}
              role="status"
              aria-label="Playing at 2× speed"
            >
              <span>2×</span>
              <${Icon} name="fast-forward" />
            </div>
          `
        : null}

      ${zoomChip
        ? html`
            <div class="player-zoom-chip" role="status">
              ${zoomChip}
            </div>
          `
        : null}

      ${scrubHud
        ? html`
            <div class="player-scrub-hud" role="status" aria-live="off">
              <div class="player-scrub-hud-time">${formatPlayerTime(scrubHud.time)}</div>
              <div class="player-scrub-hud-delta">${formatScrubDelta(scrubHud.delta)}</div>
              ${scrubHud.rateLabel
                ? html`<div class="player-scrub-hud-rate">${scrubHud.rateLabel}</div>`
                : null}
            </div>
          `
        : null}

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

      ${ctrl.stalled || mediaLoading
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
          <span class="name" role="heading" aria-level="2" aria-label=${name} title=${name}>
            <${FileNameSingle} name=${name} lines=${1} titleOnHover=${false} />
          </span>
          ${pipMode
            ? html`
                <button
                  type="button"
                  class="btn icon player-pip-btn"
                  aria-label=${pipActive ? 'Exit picture in picture' : 'Picture in picture'}
                  aria-pressed=${pipActive ? 'true' : 'false'}
                  onClick=${handleTogglePip}
                >
                  <${Icon} name="pip" />
                </button>
              `
            : null}
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

        <div
          class=${scrubHud ? 'player-center scrubbing-hidden' : 'player-center'}
          aria-label="Playback controls"
          aria-hidden=${scrubHud ? 'true' : undefined}
        >
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
            aria-label=${effectivePlaying ? 'Pause' : 'Play'}
            onClick=${togglePlay}
          >
            <${Icon} name=${effectivePlaying ? 'pause' : 'play'} />
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

        <div class="player-bottom" ref=${scrubBarWrapRef}>
          <${PositionSlider}
            currentTime=${displayTime}
            duration=${duration}
            bufferedEnd=${bufferedEnd}
            externalDragging=${Boolean(scrubHud)}
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
                  class="btn player-btn player-speed-btn"
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

function prefersReducedMotion() {
  if (typeof window === 'undefined' || typeof window.matchMedia !== 'function') return false;
  try {
    return window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  } catch {
    return false;
  }
}

export function MusicPlayer({
  musicController,
  musicState,
  name,
  moreOpen = false,
  transitionFromRect = null,
  onConsumeTransitionRect = null,
  onOpenMore,
  onClose,
}) {
  const showVolume = useMemo(() => !isMobileTouchDevice(), []);
  const screenRef = useRef(null);
  const coverBoxRef = useRef(null);
  const moreOpenRef = useRef(moreOpen);
  moreOpenRef.current = moreOpen;

  const stateRef = useRef(musicState);
  stateRef.current = musicState;

  const display = useMemo(
    () => formatMusicDisplay(musicState?.tags, musicState?.name || name),
    [musicState?.tags, musicState?.name, name],
  );

  // Shared-element feel when opening from the small player: the cover grows into place over
  // 220 ms, while reduced motion cuts immediately with no animation.
  useLayoutEffect(() => {
    if (!transitionFromRect) return;
    onConsumeTransitionRect?.();
    if (prefersReducedMotion()) return;
    const coverBox = coverBoxRef.current;
    const screen = screenRef.current;
    if (!coverBox || typeof coverBox.animate !== 'function') return;
    const toRect = coverBox.getBoundingClientRect();
    if (!toRect || toRect.width <= 0 || toRect.height <= 0 || transitionFromRect.width <= 0) return;

    const dx = transitionFromRect.left - toRect.left;
    const dy = transitionFromRect.top - toRect.top;
    const sx = Math.max(0.05, transitionFromRect.width / toRect.width);
    const sy = Math.max(0.05, transitionFromRect.height / toRect.height);
    const startRadius = `${Math.round((MINI_PLAYER_COVER_RADIUS_PX / sx) * 10) / 10}px`;

    try {
      coverBox.animate(
        [
          {
            transformOrigin: 'top left',
            transform: `translate3d(${dx}px, ${dy}px, 0) scale(${sx}, ${sy})`,
            borderRadius: startRadius,
          },
          {
            transformOrigin: 'top left',
            transform: 'translate3d(0, 0, 0) scale(1, 1)',
            borderRadius: 'var(--radius)',
          },
        ],
        {
          duration: MINI_PLAYER_TRANSITION_MS,
          easing: 'cubic-bezier(0.2, 0.8, 0.2, 1)',
        },
      );
      if (screen && typeof screen.animate === 'function') {
        screen.animate([{ opacity: 0.92 }, { opacity: 1 }], {
          duration: MINI_PLAYER_TRANSITION_MS,
          easing: 'ease-out',
        });
      }
    } catch {
      // Ignore WAAPI errors in minimal test environments.
    }
  }, [transitionFromRect, onConsumeTransitionRect]);

  const togglePlay = useCallback(() => {
    musicController?.togglePlay?.();
  }, [musicController]);

  const commitSeek = useCallback(
    (targetTime) => {
      musicController?.seek?.(targetTime);
    },
    [musicController],
  );

  const handlePrev = useCallback(() => {
    musicController?.prev?.();
  }, [musicController]);

  const handleNext = useCallback(() => {
    musicController?.next?.();
  }, [musicController]);

  const handleToggleShuffle = useCallback(() => {
    musicController?.toggleShuffle?.();
  }, [musicController]);

  const handleCycleRepeat = useCallback(() => {
    musicController?.cycleRepeat?.();
  }, [musicController]);

  const handleStartOver = useCallback(() => {
    musicController?.startOver?.();
  }, [musicController]);

  const applyVolume = useCallback(
    (nextVol) => {
      musicController?.setVolume?.(nextVol);
    },
    [musicController],
  );

  const toggleMute = useCallback(() => {
    musicController?.toggleMute?.();
  }, [musicController]);

  // Keyboard shortcuts (desktop & browser).
  useEffect(() => {
    function onKeyDown(e) {
      if (moreOpenRef.current) return;
      const hasScrim = Boolean(document.querySelector('.scrim'));
      const action = classifyPlayerKey(e, { mode: 'audio', isFullscreen: false, hasOpenOverlay: hasScrim });
      if (!action) return;
      e.preventDefault();
      e.stopImmediatePropagation?.();
      const st = stateRef.current || {};
      switch (action) {
        case 'toggle_play':
          togglePlay();
          break;
        case 'seek_back':
          commitSeek(Math.max(0, (st.currentTime || 0) - SEEK_STEP_S));
          break;
        case 'seek_forward':
          commitSeek((st.currentTime || 0) + SEEK_STEP_S);
          break;
        case 'volume_up':
          if (showVolume) applyVolume((st.muted ? 0 : st.volume ?? 1) + VOLUME_STEP);
          break;
        case 'volume_down':
          if (showVolume) applyVolume((st.muted ? 0 : st.volume ?? 1) - VOLUME_STEP);
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
  }, [togglePlay, commitSeek, showVolume, applyVolume, toggleMute, handleNext, handlePrev, onClose]);

  const queue = musicState?.queue || { items: [], index: -1, prev: null, next: null };
  const coverUrl = musicState?.coverUrl || null;
  const playing = Boolean(musicState?.playing);
  const duration = Number.isFinite(musicState?.duration) ? musicState.duration : 0;
  const currentTime = Number.isFinite(musicState?.currentTime) ? musicState.currentTime : 0;
  const bufferedEnd = Number.isFinite(musicState?.bufferedEnd) ? musicState.bufferedEnd : 0;
  const displayTime = musicState?.pendingSeekTime != null ? musicState.pendingSeekTime : currentTime;
  const busy = Boolean(musicState?.stalled || musicState?.mediaLoading);
  const resumeChip = musicState?.resumeChip || null;
  const shuffle = Boolean(musicState?.shuffle);
  const repeat = musicState?.repeat || 'off';
  const volume = musicState?.volume ?? 1;
  const muted = Boolean(musicState?.muted);

  const prevIsRestart = !queue.prev || shouldMusicPrevRestart(displayTime);
  const showNext = Boolean(queue.next);
  const repeatLabel =
    repeat === 'folder'
      ? 'Repeat folder (tap for repeat one)'
      : repeat === 'one'
        ? 'Repeat one song (tap to turn off)'
        : 'Repeat off (tap to repeat folder)';

  return html`
    <div ref=${screenRef} class=${coverUrl ? 'music-screen has-cover' : 'music-screen'}>
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
          <div ref=${coverBoxRef} class="music-cover-box">
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
            <h1 class="music-title" aria-label=${display.title} title=${display.title}>
              <${FileNameSingle} name=${display.title} lines=${2} titleOnHover=${false} />
            </h1>
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
            <div class="music-transport-slot">
              <button
                type="button"
                class=${shuffle ? 'btn icon music-mode-btn active' : 'btn icon music-mode-btn'}
                aria-label=${shuffle ? 'Shuffle on' : 'Shuffle off'}
                aria-pressed=${shuffle ? 'true' : 'false'}
                onClick=${handleToggleShuffle}
              >
                <${Icon} name="shuffle" />
              </button>
            </div>
            <div class="music-transport-slot">
              <button
                type="button"
                class="btn icon music-ctl-btn"
                aria-label=${prevIsRestart ? 'Restart track' : 'Previous track'}
                onClick=${handlePrev}
              >
                <${Icon} name="prev" />
              </button>
            </div>
            <div class="music-transport-slot">
              <button
                type="button"
                class="btn icon primary music-play-btn"
                aria-label=${playing ? 'Pause' : 'Play'}
                onClick=${togglePlay}
              >
                ${busy
                  ? html`<span class="music-spinner" aria-hidden="true"></span>`
                  : html`<${Icon} name=${playing ? 'pause' : 'play'} />`}
              </button>
            </div>
            <div class="music-transport-slot">
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
            <div class="music-transport-slot">
              <button
                type="button"
                class=${repeat !== 'off' ? 'btn icon music-mode-btn active' : 'btn icon music-mode-btn'}
                aria-label=${repeatLabel}
                aria-pressed=${repeat !== 'off' ? 'true' : 'false'}
                data-repeat=${repeat}
                onClick=${handleCycleRepeat}
              >
                <${Icon} name=${repeat === 'one' ? 'repeat-1' : 'repeat'} />
              </button>
            </div>
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
    </div>
  `;
}
