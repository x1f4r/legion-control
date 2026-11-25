// Foreground resume controller shared by the shell lifecycle hook and DOM visibility fallback.

export function createForegroundResumeController({
  getVisibilityState = () => (typeof document !== 'undefined' ? document.visibilityState : 'visible'),
  invalidateSessions = null,
  onBackground = null,
  onInvalidate = null,
  onRefresh = null,
  onCheckUpdates = null,
  dedupeMs = 1500,
  now = () => Date.now(),
} = {}) {
  let wasHidden = getVisibilityState() === 'hidden';
  let resuming = false;
  let lastInvalidateAt = -Infinity;

  async function runForeground({ forceInvalidate }) {
    if (resuming) return false;
    const currentNow = now();
    const recentlyInvalidated = currentNow - lastInvalidateAt < dedupeMs;

    if (forceInvalidate) {
      wasHidden = false;
      if (recentlyInvalidated) return false;
      if (typeof invalidateSessions === 'function') {
        lastInvalidateAt = currentNow;
        resuming = true;
        try {
          await invalidateSessions();
          if (typeof onInvalidate === 'function') onInvalidate();
        } finally {
          resuming = false;
        }
      } else {
        lastInvalidateAt = currentNow;
      }
      if (typeof onRefresh === 'function') onRefresh();
      if (typeof onCheckUpdates === 'function') onCheckUpdates();
      return true;
    }

    if (wasHidden && typeof invalidateSessions === 'function') {
      wasHidden = false;
      if (recentlyInvalidated) return false;
      lastInvalidateAt = currentNow;
      resuming = true;
      try {
        await invalidateSessions();
        if (typeof onInvalidate === 'function') onInvalidate();
      } finally {
        resuming = false;
      }
      if (typeof onRefresh === 'function') onRefresh();
      if (typeof onCheckUpdates === 'function') onCheckUpdates();
      return true;
    }

    wasHidden = false;
    if (recentlyInvalidated) return false;
    if (typeof onRefresh === 'function') onRefresh();
    if (typeof onCheckUpdates === 'function') onCheckUpdates();
    return true;
  }

  async function handleVisibilityChange() {
    const state = getVisibilityState();
    if (state === 'hidden') {
      wasHidden = true;
      if (typeof onBackground === 'function') onBackground();
      return false;
    }
    if (state !== 'visible') return false;
    return runForeground({ forceInvalidate: false });
  }

  async function handleNativeResume() {
    return runForeground({ forceInvalidate: true });
  }

  return {
    handleVisibilityChange,
    handleNativeResume,
    isResuming: () => resuming,
    isHidden: () => wasHidden,
  };
}
