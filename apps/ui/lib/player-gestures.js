// Pure video picture touch recogniser, zoom-to-fill geometry/persistence, hold-for-2x,
// scrub-anywhere position/rate accumulator, and desktop Space-hold controller.
//
// All time-dependent behaviour takes an injectable clock so unit tests can step exact
// millisecond boundaries without real timers.

import { formatPlayerTime, tapZoneForPosition } from './player.js';

export const GESTURE_MOVE_SLOP_PX = 12;
export const GESTURE_HORIZONTAL_MAX_DEG = 30;
export const GESTURE_HORIZONTAL_MAX_SLOPE = Math.tan((GESTURE_HORIZONTAL_MAX_DEG * Math.PI) / 180);
export const GESTURE_HOLD_DELAY_MS = 350;
export const GESTURE_EDGE_MARGIN_PX = 24;

export const ZOOM_STORAGE_KEY = 'legion.player.zoom.v1';
export const ZOOM_MIN_DIFF = 0.03;
export const ZOOM_AUTO_APPLY_MAX_CROP = 0.15;
export const ZOOM_SETTLE_MS = 180;
export const ZOOM_CHIP_DURATION_MS = 1200;
export const PINCH_VELOCITY_THRESHOLD = 0.5;

export const HOLD_PLAYBACK_RATE = 2;
export const HOLD_PILL_FADE_MS = 120;

export const SCRUB_HALF_OFFSET_PX = 80;
export const SCRUB_QUARTER_OFFSET_PX = 160;
export const SCRUB_FINE_OFFSET_PX = 240;

function defaultClock() {
  return {
    now: () => Date.now(),
    setTimeout: (fn, ms) => (typeof window !== 'undefined' ? window.setTimeout(fn, ms) : setTimeout(fn, ms)),
    clearTimeout: (id) => (typeof window !== 'undefined' ? window.clearTimeout(id) : clearTimeout(id)),
  };
}

// ── Zoom to fill (fit vs fill geometry, rubber-band resistance, snap & persistence) ──────────

/**
 * Computes fit-vs-fill zoom metrics for a video inside a viewport.
 *
 * `fillScale` is the uniform CSS scale on an `object-fit: contain` video needed to cover the
 * viewport cleanly on two sides. Zoom is offered only when `fillScale` differs from `1` by more
 * than 3 percent (`fillScale - 1 > 0.03`). A remembered fill choice auto-applies on entering
 * full screen only when fill hides at most 15 percent of the picture (`1 - 1 / fillScale <= 0.15`).
 */
export function computeVideoZoomMetrics(videoWidth, videoHeight, viewportWidth, viewportHeight) {
  if (
    !Number.isFinite(videoWidth) ||
    !Number.isFinite(videoHeight) ||
    !Number.isFinite(viewportWidth) ||
    !Number.isFinite(viewportHeight) ||
    videoWidth <= 0 ||
    videoHeight <= 0 ||
    viewportWidth <= 0 ||
    viewportHeight <= 0
  ) {
    return {
      available: false,
      fillScale: 1,
      hiddenFraction: 0,
      canAutoApplyFill: false,
    };
  }
  const videoAspect = videoWidth / videoHeight;
  const viewportAspect = viewportWidth / viewportHeight;
  const fillScale = Math.max(videoAspect / viewportAspect, viewportAspect / videoAspect);
  const diff = fillScale - 1;
  const available = diff > ZOOM_MIN_DIFF + 1e-9;
  const hiddenFraction = fillScale > 1 ? 1 - 1 / fillScale : 0;
  const canAutoApplyFill = available && hiddenFraction <= ZOOM_AUTO_APPLY_MAX_CROP + 1e-9;
  return {
    available,
    fillScale: available ? fillScale : 1,
    hiddenFraction,
    canAutoApplyFill,
  };
}

/**
 * Follows the pinch continuously between `1` (fit) and `fillScale` (fill), with smooth
 * rubber-band resistance beyond either end.
 */
export function applyPinchResistance(rawScale, fillScale) {
  if (!Number.isFinite(rawScale) || !Number.isFinite(fillScale) || fillScale <= 1) {
    return 1;
  }
  if (rawScale >= 1 && rawScale <= fillScale) {
    return rawScale;
  }
  if (rawScale < 1) {
    const over = 1 - rawScale;
    return 1 - (over * 0.25) / (1 + over * 1.5);
  }
  const over = rawScale - fillScale;
  return fillScale + (over * 0.25) / (1 + over * 1.5);
}

/**
 * Decides whether a released pinch settles on `'fit'` or `'fill'`.
 * Fast radial movement decides the direction; otherwise the nearer state wins.
 */
export function resolvePinchZoomTarget({
  scale,
  fillScale,
  velocity = 0,
  velocityThreshold = PINCH_VELOCITY_THRESHOLD,
} = {}) {
  if (!Number.isFinite(fillScale) || fillScale <= 1) return 'fit';
  if (Number.isFinite(velocity)) {
    if (velocity >= velocityThreshold) return 'fill';
    if (velocity <= -velocityThreshold) return 'fit';
  }
  const current = Number.isFinite(scale) ? scale : 1;
  const midpoint = (1 + fillScale) / 2;
  return current >= midpoint ? 'fill' : 'fit';
}

export function readZoomPreference(storage) {
  if (!storage) return 'fit';
  try {
    const raw = storage.getItem(ZOOM_STORAGE_KEY);
    return raw === 'fill' ? 'fill' : 'fit';
  } catch {
    return 'fit';
  }
}

export function writeZoomPreference(storage, mode) {
  if (!storage) return;
  try {
    storage.setItem(ZOOM_STORAGE_KEY, mode === 'fill' ? 'fill' : 'fit');
  } catch {
    // Ignore storage quota or private-mode failures.
  }
}

// ── Scrub-anywhere precision rates & signed delta formatting ─────────────────────────────────

/**
 * Returns the scrub precision multiplier and user-facing label for a vertical pull distance
 * away from the horizontal drag origin.
 */
export function scrubRateForOffset(absDy) {
  const dy = Number.isFinite(absDy) ? Math.abs(absDy) : 0;
  if (dy > SCRUB_FINE_OFFSET_PX) return { rate: 0.1, label: 'Fine' };
  if (dy > SCRUB_QUARTER_OFFSET_PX) return { rate: 0.25, label: 'Quarter speed' };
  if (dy > SCRUB_HALF_OFFSET_PX) return { rate: 0.5, label: 'Half speed' };
  return { rate: 1, label: null };
}

/**
 * Formats a scrub delta such as `"+0:35"` or `"−0:12"`.
 */
export function formatScrubDelta(deltaSeconds) {
  const rounded = Number.isFinite(deltaSeconds) ? Math.round(deltaSeconds) : 0;
  if (rounded < 0) {
    return `\u2212${formatPlayerTime(Math.abs(rounded))}`;
  }
  return `+${formatPlayerTime(rounded)}`;
}

// ── Single unified picture touch / pointer recogniser ────────────────────────────────────────

/**
 * Single recogniser owning all pointer gestures on the video picture surface (tap, double-tap
 * delegation, pinch zoom, horizontal scrub, and press-and-hold 2x).
 *
 * Guarantees:
 * - Touches beginning within `edgeMarginPx` (24 px) of left/right edges or inside the system
 *   bottom gesture area start nothing.
 * - A second pointer immediately switches to pinch and drops any pending tap, scrub (without
 *   seeking), or hold (restoring speed).
 * - Movement > 12 px within 30 degrees of horizontal with the layer visible becomes a scrub;
 *   vertical drags or sideways drags with the layer hidden become nothing and never fire a tap.
 * - Staying within 12 px for 350 ms engages hold when `canHold()` is true; lifting before
 *   350 ms within 12 px fires a tap.
 * - Once a touch becomes pinch, scrub, or hold, its release never turns into a tap.
 */
export function createPictureGestureRecognizer({
  clock = defaultClock(),
  edgeMarginPx = GESTURE_EDGE_MARGIN_PX,
  getBounds = () => ({ left: 0, top: 0, width: 1000, height: 600, bottomMarginPx: GESTURE_EDGE_MARGIN_PX }),
  isLayerVisible = () => true,
  canHold = () => false,
  canScrub = () => true,
  canPinch = () => false,
  getZoomFillScale = () => 1,
  getZoomMode = () => 'fit',
  getScrubBarWidth = () => 1000,
  getCurrentTime = () => 0,
  getDuration = () => 0,
  onTap = () => {},
  onHoldStart = () => {},
  onHoldEnd = () => {},
  onScrubStart = () => {},
  onScrubMove = () => {},
  onScrubEnd = () => {},
  onScrubCancel = () => {},
  onPinchStart = () => {},
  onPinchMove = () => {},
  onPinchEnd = () => {},
  onPinchCancel = () => {},
} = {}) {
  let state = 'idle'; // 'idle' | 'pending' | 'scrub' | 'hold' | 'pinch' | 'ignored'
  let holdTimer = null;
  const pointers = new Map(); // pointerId -> { id, pointerType, x, y, startX, startY, downAt }
  let primaryId = null;

  // Scrub tracking
  let scrub = null; // { startX, startY, lastX, startTime, time, duration, barWidth, rate, rateLabel }
  // Pinch tracking
  let pinch = null; // { active, startDist, fillScale, startMode, startScale, scale, samples }

  function clearHoldTimer() {
    if (holdTimer != null) {
      clock.clearTimeout(holdTimer);
      holdTimer = null;
    }
  }

  function isInSystemEdge(x, y, pointerType) {
    if (pointerType === 'mouse') return false;
    const bounds = getBounds();
    if (!bounds || !Number.isFinite(bounds.width) || !Number.isFinite(bounds.height)) return false;
    const bottomMargin = Math.max(0, Number(bounds.bottomMarginPx ?? edgeMarginPx));
    const fromLeft = x - bounds.left;
    const fromRight = bounds.left + bounds.width - x;
    const fromBottom = bounds.top + bounds.height - y;
    return fromLeft <= edgeMarginPx || fromRight <= edgeMarginPx || fromBottom <= bottomMargin;
  }

  function pairDistance() {
    const pts = [...pointers.values()];
    if (pts.length < 2) return 0;
    return Math.hypot(pts[0].x - pts[1].x, pts[0].y - pts[1].y);
  }

  function clampTime(t, duration) {
    if (!Number.isFinite(duration) || duration <= 0) return 0;
    return Math.max(0, Math.min(duration, t));
  }

  function updateScrubAt(x, y, notifyStart = false) {
    if (!scrub) return;
    const prevRate = scrub.rate;
    const { rate, label } = scrubRateForOffset(Math.abs(y - scrub.startY));
    const stepDx = x - scrub.lastX;
    scrub.lastX = x;
    scrub.rate = rate;
    scrub.rateLabel = label;
    if (stepDx !== 0 && scrub.duration > 0 && scrub.barWidth > 0) {
      const nextTime = scrub.time + (stepDx / scrub.barWidth) * scrub.duration * rate;
      scrub.time = clampTime(nextTime, scrub.duration);
    }
    const payload = {
      startTime: scrub.startTime,
      time: scrub.time,
      delta: scrub.time - scrub.startTime,
      rate: scrub.rate,
      rateLabel: scrub.rateLabel,
      rateChanged: !notifyStart && rate !== prevRate,
    };
    if (notifyStart) {
      onScrubStart(payload);
    }
    onScrubMove(payload);
  }

  function computePinchVelocity(samples) {
    if (!samples || samples.length < 2) return 0;
    const last = samples[samples.length - 1];
    let first = samples[0];
    for (let i = samples.length - 2; i >= 0; i -= 1) {
      first = samples[i];
      if (last.t - first.t >= 40) break;
    }
    const dtSec = (last.t - first.t) / 1000;
    if (dtSec <= 0.005) return 0;
    return (last.scale - first.scale) / dtSec;
  }

  function transitionToPinch() {
    clearHoldTimer();
    if (state === 'scrub') {
      scrub = null;
      onScrubCancel();
    } else if (state === 'hold') {
      onHoldEnd();
    }
    state = 'pinch';
    const active = Boolean(canPinch());
    const fillScale = active ? getZoomFillScale() : 1;
    const startMode = active ? getZoomMode() : 'fit';
    const startScale = startMode === 'fill' ? fillScale : 1;
    const startDist = Math.max(1, pairDistance());
    pinch = {
      active,
      startDist,
      fillScale,
      startMode,
      startScale,
      scale: startScale,
      samples: [{ t: clock.now(), scale: startScale }],
    };
    if (active) {
      onPinchStart({ startScale, fillScale, mode: startMode });
    }
  }

  function finishPinch(cancelled = false) {
    if (!pinch) return;
    const current = pinch;
    pinch = null;
    if (!current.active) return;
    if (cancelled) {
      onPinchCancel();
      return;
    }
    const velocity = computePinchVelocity(current.samples);
    const targetMode = resolvePinchZoomTarget({
      scale: current.scale,
      fillScale: current.fillScale,
      velocity,
    });
    onPinchEnd({
      scale: current.scale,
      velocity,
      targetMode,
      changed: targetMode !== current.startMode,
    });
  }

  function onPointerDown(e) {
    if (!e) return false;
    if (typeof e.button === 'number' && e.button > 0) return false;
    const id = e.pointerId ?? 0;
    const pointerType = String(e.pointerType || 'touch');
    const x = Number(e.clientX);
    const y = Number(e.clientY);
    if (!Number.isFinite(x) || !Number.isFinite(y)) return false;

    if (pointers.size === 0 && isInSystemEdge(x, y, pointerType)) {
      return false;
    }

    const pt = {
      id,
      pointerType,
      x,
      y,
      startX: x,
      startY: y,
      downAt: clock.now(),
    };
    pointers.set(id, pt);

    if (pointers.size === 1) {
      primaryId = id;
      state = 'pending';
      clearHoldTimer();
      holdTimer = clock.setTimeout(() => {
        holdTimer = null;
        if (state !== 'pending' || primaryId !== id || pointers.size !== 1) return;
        if (canHold()) {
          state = 'hold';
          onHoldStart();
        } else {
          // Stayed still for 350 ms when hold cannot engage: release must not become a tap.
          state = 'ignored';
        }
      }, GESTURE_HOLD_DELAY_MS);
      return true;
    }

    if (pointers.size === 2) {
      transitionToPinch();
      return true;
    }

    return true;
  }

  function onPointerMove(e) {
    if (!e) return false;
    const id = e.pointerId ?? 0;
    const pt = pointers.get(id);
    if (!pt) return false;
    const x = Number(e.clientX);
    const y = Number(e.clientY);
    if (!Number.isFinite(x) || !Number.isFinite(y)) return false;
    pt.x = x;
    pt.y = y;

    if (state === 'pinch') {
      if (pointers.size >= 2 && pinch) {
        const dist = pairDistance();
        const rawScale = pinch.startScale * (dist / pinch.startDist);
        const scale = applyPinchResistance(rawScale, pinch.fillScale);
        pinch.scale = scale;
        const now = clock.now();
        pinch.samples.push({ t: now, scale });
        while (pinch.samples.length > 2 && now - pinch.samples[0].t > 120) {
          pinch.samples.shift();
        }
        if (pinch.active) {
          onPinchMove({ scale, rawScale, fillScale: pinch.fillScale });
        }
      }
      return true;
    }

    if (id !== primaryId) return true;

    if (state === 'pending') {
      const dx = x - pt.startX;
      const dy = y - pt.startY;
      const dist = Math.hypot(dx, dy);
      if (dist <= GESTURE_MOVE_SLOP_PX) {
        return true;
      }
      clearHoldTimer();
      const absDx = Math.abs(dx);
      const absDy = Math.abs(dy);
      const isHorizontal = absDx > 0 && absDy <= absDx * GESTURE_HORIZONTAL_MAX_SLOPE + 1e-9;
      if (isHorizontal && isLayerVisible() && canScrub()) {
        const duration = getDuration();
        if (Number.isFinite(duration) && duration > 0) {
          const startTime = clampTime(getCurrentTime(), duration);
          const barWidth = Math.max(1, Number(getScrubBarWidth()) || 1);
          state = 'scrub';
          scrub = {
            startX: pt.startX,
            startY: pt.startY,
            lastX: pt.startX,
            startTime,
            time: startTime,
            duration,
            barWidth,
            rate: 1,
            rateLabel: null,
          };
          updateScrubAt(x, y, true);
          return true;
        }
      }
      state = 'ignored';
      return true;
    }

    if (state === 'scrub') {
      updateScrubAt(x, y, false);
      return true;
    }

    // In 'hold' or 'ignored', ignore further movement without breaking the gesture.
    return true;
  }

  function onPointerUp(e) {
    if (!e) return false;
    const id = e.pointerId ?? 0;
    const pt = pointers.get(id);
    if (!pt) return false;
    if (Number.isFinite(Number(e.clientX))) pt.x = Number(e.clientX);
    if (Number.isFinite(Number(e.clientY))) pt.y = Number(e.clientY);
    pointers.delete(id);

    if (state === 'pinch') {
      if (pointers.size < 2) {
        finishPinch(false);
        state = pointers.size > 0 ? 'ignored' : 'idle';
        if (pointers.size === 0) primaryId = null;
      }
      return true;
    }

    if (id === primaryId) {
      clearHoldTimer();
      if (state === 'hold') {
        onHoldEnd();
      } else if (state === 'scrub' && scrub) {
        const distFromStart = Math.hypot(pt.x - scrub.startX, pt.y - scrub.startY);
        const committed = distFromStart > GESTURE_MOVE_SLOP_PX;
        const done = scrub;
        scrub = null;
        onScrubEnd({
          startTime: done.startTime,
          time: committed ? done.time : done.startTime,
          delta: committed ? done.time - done.startTime : 0,
          committed,
        });
      } else if (state === 'pending') {
        const distFromStart = Math.hypot(pt.x - pt.startX, pt.y - pt.startY);
        const elapsed = clock.now() - pt.downAt;
        if (distFromStart <= GESTURE_MOVE_SLOP_PX && elapsed < GESTURE_HOLD_DELAY_MS) {
          const bounds = getBounds();
          const zone = tapZoneForPosition(pt.x, bounds.left, bounds.width);
          onTap(zone);
        }
      }
    }

    if (pointers.size === 0) {
      state = 'idle';
      primaryId = null;
      scrub = null;
      pinch = null;
    } else if (id === primaryId) {
      state = 'ignored';
    }
    return true;
  }

  function onPointerCancel(e) {
    const id = e?.pointerId;
    if (id != null && !pointers.has(id)) return false;
    clearHoldTimer();
    if (state === 'hold') {
      onHoldEnd();
    } else if (state === 'scrub') {
      scrub = null;
      onScrubCancel();
    } else if (state === 'pinch') {
      finishPinch(true);
    }
    if (id != null) {
      pointers.delete(id);
    } else {
      pointers.clear();
    }
    if (pointers.size === 0) {
      state = 'idle';
      primaryId = null;
      scrub = null;
      pinch = null;
    } else {
      state = 'ignored';
    }
    return true;
  }

  function reset() {
    clearHoldTimer();
    if (state === 'hold') {
      onHoldEnd();
    } else if (state === 'scrub') {
      scrub = null;
      onScrubCancel();
    } else if (state === 'pinch') {
      finishPinch(true);
    }
    pointers.clear();
    primaryId = null;
    scrub = null;
    pinch = null;
    state = 'idle';
  }

  function getState() {
    return {
      state,
      pointerCount: pointers.size,
      scrub: scrub ? { ...scrub } : null,
      pinch: pinch ? { active: pinch.active, scale: pinch.scale, fillScale: pinch.fillScale } : null,
    };
  }

  function dispose() {
    clearHoldTimer();
    pointers.clear();
    primaryId = null;
    scrub = null;
    pinch = null;
    state = 'idle';
  }

  return {
    onPointerDown,
    onPointerMove,
    onPointerUp,
    onPointerCancel,
    reset,
    getState,
    dispose,
  };
}

// ── Desktop Space hold-for-2x controller ─────────────────────────────────────────────────────

/**
 * Splits Space on desktop into a short press (< 350 ms -> play/pause) and a press-and-hold
 * (>= 350 ms while playing and chosen speed < 2x -> play at 2x until release without toggling).
 */
export function createSpaceHoldController({
  clock = defaultClock(),
  holdDelayMs = GESTURE_HOLD_DELAY_MS,
  canHold = () => false,
  onTogglePlay = () => {},
  onHoldStart = () => {},
  onHoldEnd = () => {},
} = {}) {
  let isDown = false;
  let downAt = 0;
  let holding = false;
  let toggledOnDown = false;
  let timer = null;

  function clearTimer() {
    if (timer != null) {
      clock.clearTimeout(timer);
      timer = null;
    }
  }

  function onKeyDown({ repeat = false } = {}) {
    if (repeat || isDown) {
      return { action: holding ? 'holding' : 'ignored' };
    }
    isDown = true;
    downAt = clock.now();
    holding = false;
    toggledOnDown = false;

    if (!canHold()) {
      toggledOnDown = true;
      onTogglePlay();
      return { action: 'toggle' };
    }

    clearTimer();
    timer = clock.setTimeout(() => {
      timer = null;
      if (!isDown) return;
      if (canHold()) {
        holding = true;
        onHoldStart();
      }
    }, holdDelayMs);
    return { action: 'pending' };
  }

  function onKeyUp() {
    if (!isDown) return { action: 'idle' };
    clearTimer();
    const wasHolding = holding;
    const wasToggledOnDown = toggledOnDown;
    const elapsed = clock.now() - downAt;
    isDown = false;
    holding = false;
    toggledOnDown = false;

    if (wasHolding) {
      onHoldEnd();
      return { action: 'hold_end' };
    }
    if (wasToggledOnDown) {
      return { action: 'already_toggled' };
    }
    if (elapsed < holdDelayMs) {
      onTogglePlay();
      return { action: 'toggle' };
    }
    return { action: 'ignored' };
  }

  function cancel() {
    clearTimer();
    const wasHolding = holding;
    isDown = false;
    holding = false;
    toggledOnDown = false;
    if (wasHolding) {
      onHoldEnd();
    }
  }

  function isHolding() {
    return holding;
  }

  function dispose() {
    clearTimer();
    isDown = false;
    holding = false;
    toggledOnDown = false;
  }

  return {
    onKeyDown,
    onKeyUp,
    cancel,
    isHolding,
    dispose,
  };
}
