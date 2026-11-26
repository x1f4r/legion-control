// Pure gesture maths and pointer-event helpers (no external library).

export const SWIPE_MIN_PX = 56;
export const SWIPE_FAST_PX = 38;
export const SWIPE_FAST_VELOCITY = 0.35; // px/ms
export const SWIPE_AXIS_RATIO = 1.5;
export const EDGE_BACK_ZONE_PX = 24;
export const PULL_REFRESH_PX = 64;
export const ROW_REVEAL_PX = 48;
export const ROW_CLOSE_PX = 32;

/**
 * Classifies a completed pointer drag as a horizontal navigation swipe or left-edge back gesture.
 * Returns 'edge-back', 'prev' (swipe right), 'next' (swipe left), or null.
 */
export function classifyHorizontalSwipe({
  dx,
  dy,
  elapsedMs = 200,
  startX = 100,
  allowEdgeBack = false,
}) {
  const absX = Math.abs(dx);
  const absY = Math.abs(dy);
  if (absX < SWIPE_FAST_PX) return null;
  if (absX <= absY * SWIPE_AXIS_RATIO) return null;

  const dt = Math.max(1, Number(elapsedMs) || 1);
  const velocity = absX / dt;
  if (absX < SWIPE_MIN_PX && velocity < SWIPE_FAST_VELOCITY) return null;

  if (dx > 0) {
    if (allowEdgeBack && startX <= EDGE_BACK_ZONE_PX) return 'edge-back';
    return 'prev';
  }
  return 'next';
}

/**
 * Decides whether a vertical drag starting at scrollTop <= 0 should trigger pull-to-refresh.
 */
export function classifyPullRefresh({ dx, dy, scrollTop = 0 }) {
  if (scrollTop > 2) return false;
  if (dy < PULL_REFRESH_PX) return false;
  return dy > Math.abs(dx) * SWIPE_AXIS_RATIO;
}

/**
 * Classifies a horizontal swipe on a single list row: 'reveal' (swipe left), 'close' (swipe right), or null.
 */
export function classifyRowSwipe({ dx, dy, currentlyOpen = false }) {
  const absX = Math.abs(dx);
  const absY = Math.abs(dy);
  if (absX <= absY * SWIPE_AXIS_RATIO) return null;
  if (!currentlyOpen && dx <= -ROW_REVEAL_PX) return 'reveal';
  if (currentlyOpen && dx >= ROW_CLOSE_PX) return 'close';
  return null;
}

/**
 * Returns true when an interactive or horizontally-scrollable ancestor should own the pointer drag.
 */
export function shouldIgnoreSwipeTarget(target, boundaryEl = null) {
  let el = target;
  while (el && el !== boundaryEl) {
    if (el.nodeType === 1) {
      const tag = el.tagName ? el.tagName.toUpperCase() : '';
      if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || tag === 'PRE') {
        return true;
      }
      if (el.dataset && el.dataset.noSwipe === 'true') {
        return true;
      }
      if (typeof window !== 'undefined' && typeof window.getComputedStyle === 'function') {
        const style = window.getComputedStyle(el);
        const ox = style.overflowX;
        if ((ox === 'auto' || ox === 'scroll') && el.scrollWidth > el.clientWidth + 2) {
          return true;
        }
      }
    }
    el = el.parentElement;
  }
  return false;
}

/**
 * Android WebView already provides the system edge-back gesture via MainActivity.kt.
 */
export function hasNativeEdgeBack(win = typeof window !== 'undefined' ? window : null) {
  return Boolean(win && win.__LEGION_ANDROID__);
}

/**
 * Steps to the previous or next item in an ordered list (clamped, no wrap-around).
 */
export function stepTab(tabs, current, direction) {
  const idx = tabs.indexOf(current);
  if (idx === -1) return null;
  const nextIdx = direction === 'next' ? idx + 1 : idx - 1;
  if (nextIdx < 0 || nextIdx >= tabs.length) return null;
  return tabs[nextIdx];
}
