// Shared scroll container with touch-only pull-to-refresh used by every scrollable screen.

import { html, useCallback, useEffect, useRef, useState } from '../vendor/preact-htm.js';
import { EDGE_BACK_ZONE_PX, PULL_REFRESH_PX } from '../lib/gestures.js';

const MIN_REFRESH_MS = 400;
const LOCK_DEADZONE_PX = 8;
const MAX_PULL_VISUAL_PX = 76;
const REFRESHING_HEIGHT_PX = 44;

function prefersReducedMotion() {
  try {
    return Boolean(window.matchMedia?.('(prefers-reduced-motion: reduce)')?.matches);
  } catch {
    return false;
  }
}

function visualPullHeight(dy) {
  if (dy <= 0) return 0;
  if (dy <= PULL_REFRESH_PX) {
    return Math.round(dy * 0.68);
  }
  const extra = dy - PULL_REFRESH_PX;
  return Math.min(MAX_PULL_VISUAL_PX, Math.round(PULL_REFRESH_PX * 0.68 + extra * 0.28));
}

function shouldIgnorePullTarget(target, root) {
  if (!target || typeof target.closest !== 'function') return false;
  if (
    target.closest(
      'input, textarea, select, option, pre, [contenteditable="true"], [data-no-pull="true"], .sheet, .sheet-backdrop, .viewer',
    )
  ) {
    return true;
  }
  let el = target;
  while (el && el !== root) {
    if (el.nodeType === 1) {
      const style = window.getComputedStyle ? window.getComputedStyle(el) : null;
      if (style) {
        const oy = style.overflowY;
        if ((oy === 'auto' || oy === 'scroll') && el.scrollTop > 1) {
          return true;
        }
        const ox = style.overflowX;
        if (
          (ox === 'auto' || ox === 'scroll') &&
          el.scrollWidth > el.clientWidth + 2
        ) {
          return true;
        }
      }
      if (
        el.classList?.contains('where') ||
        el.classList?.contains('filters') ||
        el.classList?.contains('tabs') ||
        el.classList?.contains('strip')
      ) {
        if (el.scrollWidth > el.clientWidth + 2) {
          return true;
        }
      }
    }
    el = el.parentElement;
  }
  return false;
}

export function ScrollContainer({
  as = 'div',
  className = '',
  class: classProp = '',
  containerRef = null,
  onRefresh = null,
  disabled = false,
  children,
  ...rest
}) {
  const internalRef = useRef(null);
  const indicatorRef = useRef(null);
  const gestureRef = useRef(null);
  const refreshingRef = useRef(false);
  const mountedRef = useRef(true);
  const onRefreshRef = useRef(onRefresh);
  onRefreshRef.current = onRefresh;
  const disabledRef = useRef(disabled);
  disabledRef.current = disabled;

  const [phase, setPhase] = useState('idle'); // 'idle' | 'pulling' | 'armed' | 'refreshing'
  const phaseRef = useRef('idle');

  const updatePhase = useCallback((next) => {
    if (phaseRef.current !== next) {
      phaseRef.current = next;
      setPhase(next);
    }
  }, []);

  const setVisual = useCallback((heightPx, progress, animate = false) => {
    const ind = indicatorRef.current;
    if (!ind) return;
    const reduced = prefersReducedMotion();
    ind.style.transition = animate && !reduced
      ? 'height 180ms cubic-bezier(0.22, 1, 0.36, 1), opacity 160ms ease'
      : 'none';
    ind.style.height = `${Math.max(0, heightPx)}px`;
    ind.style.opacity = heightPx > 0 ? String(Math.min(1, Math.max(0.2, progress))) : '0';
    ind.style.setProperty('--pull-progress', String(Math.max(0, Math.min(1, progress))));
  }, []);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  // Cancel any in-progress pull gesture when pull becomes disabled (e.g. sheet/viewer opens).
  useEffect(() => {
    if (disabled && gestureRef.current) {
      gestureRef.current = null;
      if (!refreshingRef.current) {
        setVisual(0, 0, false);
        updatePhase('idle');
      }
    }
  }, [disabled, setVisual, updatePhase]);

  useEffect(() => {
    const el = internalRef.current;
    if (!el) return undefined;

    function onTouchStart(e) {
      if (disabledRef.current || refreshingRef.current) return;
      if (typeof onRefreshRef.current !== 'function') return;
      if (!e.touches || e.touches.length !== 1) {
        gestureRef.current = null;
        return;
      }
      if (el.scrollTop > 1) return;
      if (shouldIgnorePullTarget(e.target, el)) return;

      const touch = e.touches[0];
      gestureRef.current = {
        id: touch.identifier,
        startX: touch.clientX,
        startY: touch.clientY,
        mode: 'undecided', // 'undecided' | 'pulling' | 'rejected'
        dy: 0,
      };
    }

    function onTouchMove(e) {
      const g = gestureRef.current;
      if (!g) return;
      if (disabledRef.current || refreshingRef.current) {
        gestureRef.current = null;
        setVisual(0, 0, false);
        updatePhase('idle');
        return;
      }
      if (!e.touches || e.touches.length !== 1) {
        gestureRef.current = null;
        if (phaseRef.current !== 'idle') {
          setVisual(0, 0, true);
          updatePhase('idle');
        }
        return;
      }

      const touch = e.touches[0];
      const dx = touch.clientX - g.startX;
      const dy = touch.clientY - g.startY;
      g.dy = dy;

      if (g.mode === 'rejected') return;

      if (g.mode === 'undecided') {
        if (Math.hypot(dx, dy) < LOCK_DEADZONE_PX) return;
        // Let normal scroll, horizontal swipes, and left-edge back gesture run uninhibited.
        if (
          dy <= 0 ||
          el.scrollTop > 1 ||
          Math.abs(dx) * 1.35 >= dy ||
          (g.startX <= EDGE_BACK_ZONE_PX && dx > 4)
        ) {
          g.mode = 'rejected';
          return;
        }
        g.mode = 'pulling';
      }

      if (g.mode === 'pulling') {
        if (dy <= 0) {
          setVisual(0, 0, false);
          updatePhase('idle');
          return;
        }
        if (e.cancelable) e.preventDefault();
        const progress = Math.min(1, dy / PULL_REFRESH_PX);
        setVisual(visualPullHeight(dy), progress, false);
        updatePhase(dy >= PULL_REFRESH_PX ? 'armed' : 'pulling');
      }
    }

    function finishGesture(cancelled) {
      const g = gestureRef.current;
      gestureRef.current = null;
      if (!g || g.mode !== 'pulling') return;

      const armed = !cancelled && g.dy >= PULL_REFRESH_PX && !disabledRef.current;
      if (!armed || typeof onRefreshRef.current !== 'function') {
        setVisual(0, 0, !cancelled);
        updatePhase('idle');
        return;
      }

      refreshingRef.current = true;
      updatePhase('refreshing');
      setVisual(REFRESHING_HEIGHT_PX, 1, true);

      const startedAt = Date.now();
      let req;
      try {
        req = Promise.resolve(onRefreshRef.current());
      } catch {
        req = Promise.resolve();
      }
      req
        .catch(() => {})
        .finally(() => {
          const elapsed = Date.now() - startedAt;
          const remaining = Math.max(0, MIN_REFRESH_MS - elapsed);
          window.setTimeout(() => {
            refreshingRef.current = false;
            if (!mountedRef.current) return;
            setVisual(0, 0, true);
            updatePhase('idle');
          }, remaining);
        });
    }

    function onTouchEnd() {
      finishGesture(false);
    }

    function onTouchCancel() {
      finishGesture(true);
    }

    el.addEventListener('touchstart', onTouchStart, { passive: true });
    el.addEventListener('touchmove', onTouchMove, { passive: false });
    el.addEventListener('touchend', onTouchEnd, { passive: true });
    el.addEventListener('touchcancel', onTouchCancel, { passive: true });

    return () => {
      el.removeEventListener('touchstart', onTouchStart);
      el.removeEventListener('touchmove', onTouchMove);
      el.removeEventListener('touchend', onTouchEnd);
      el.removeEventListener('touchcancel', onTouchCancel);
    };
  }, [setVisual, updatePhase]);

  const setRef = useCallback(
    (node) => {
      internalRef.current = node;
      if (typeof containerRef === 'function') {
        containerRef(node);
      } else if (containerRef && typeof containerRef === 'object') {
        containerRef.current = node;
      }
    },
    [containerRef],
  );

  const Tag = as;
  const mergedClass = ['scroll-container', className, classProp].filter(Boolean).join(' ');
  const indicatorClass = [
    'pull-indicator',
    phase !== 'idle' ? `is-${phase}` : '',
  ]
    .filter(Boolean)
    .join(' ');
  const statusLabel =
    phase === 'refreshing'
      ? 'Refreshing…'
      : phase === 'armed'
        ? 'Release to refresh'
        : phase === 'pulling'
          ? 'Pull to refresh'
          : '';

  return html`
    <${Tag}
      ref=${setRef}
      class=${mergedClass}
      data-pull-state=${phase}
      ...${rest}
    >
      <div
        ref=${indicatorRef}
        class=${indicatorClass}
        role="status"
        aria-live="polite"
        aria-hidden=${phase === 'idle' ? 'true' : undefined}
      >
        <span class="pull-glyph" aria-hidden="true">
          ${phase === 'refreshing'
            ? html`<span class="pull-spinner"></span>`
            : html`
                <svg viewBox="0 0 16 16" width="14" height="14">
                  <path
                    d="M8 3v10M4.5 9.5 8 13l3.5-3.5"
                    fill="none"
                    stroke="currentColor"
                    stroke-width="1.6"
                    stroke-linecap="round"
                    stroke-linejoin="round"
                  />
                </svg>
              `}
        </span>
        <span class="pull-text">${statusLabel}</span>
      </div>
      ${children}
    <//>
  `;
}
