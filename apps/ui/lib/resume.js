// Foreground visibility and resume helpers shared across the app shell and view pollers.

export function isForegroundVisible(link) {
  const docVisible = typeof document === 'undefined' ? true : document.visibilityState === 'visible';
  return docVisible && !link?.isAppSuspended?.();
}

export function subscribeForegroundVisibility(link, listener) {
  if (typeof listener !== 'function') return () => {};
  let lastVisible = isForegroundVisible(link);
  const notify = (source) => {
    const visible = isForegroundVisible(link);
    if (visible === lastVisible) return;
    lastVisible = visible;
    listener({ visible, source });
  };
  const onVis = () => notify('visibilitychange');
  if (typeof document !== 'undefined' && typeof document.addEventListener === 'function') {
    document.addEventListener('visibilitychange', onVis);
  }
  const unsubSuspend =
    typeof link?.onAppSuspend === 'function' ? link.onAppSuspend(() => notify('suspend')) : () => {};
  const unsubResume =
    typeof link?.onAppResume === 'function' ? link.onAppResume(() => notify('resume')) : () => {};
  return () => {
    if (typeof document !== 'undefined' && typeof document.removeEventListener === 'function') {
      document.removeEventListener('visibilitychange', onVis);
    }
    unsubSuspend();
    unsubResume();
  };
}

export function createForegroundResumeController({
  getVisibilityState = () => (typeof document !== 'undefined' ? document.visibilityState : 'visible'),
  invalidateSessions = null,
  onBackground = null,
  onInvalidate = null,
  onRefresh = null,
  onCheckUpdates = null,
  onResumingChange = null,
  dedupeMs = 1500,
  now = () => Date.now(),
} = {}) {
  let wasHidden = getVisibilityState() === 'hidden';
  let resuming = false;
  let lastInvalidateAt = -Infinity;
  let lastRefreshAt = -Infinity;

  function setResuming(next) {
    resuming = next;
    if (typeof onResumingChange === 'function') onResumingChange(next);
  }

  function triggerRefresh(currentNow) {
    lastRefreshAt = currentNow;
    if (typeof onRefresh === 'function') onRefresh();
  }

  async function runForeground({ forceInvalidate }) {
    if (resuming) return false;
    const currentNow = now();
    const recentlyInvalidated = currentNow - lastInvalidateAt < dedupeMs;
    const recentlyRefreshed = currentNow - lastRefreshAt < dedupeMs;

    if (forceInvalidate) {
      wasHidden = false;
      if (recentlyInvalidated || recentlyRefreshed) return false;
      if (typeof invalidateSessions === 'function') {
        lastInvalidateAt = currentNow;
        lastRefreshAt = currentNow;
        setResuming(true);
        try {
          await invalidateSessions();
          if (typeof onInvalidate === 'function') onInvalidate();
        } finally {
          setResuming(false);
        }
      } else {
        lastInvalidateAt = currentNow;
      }
      triggerRefresh(now());
      if (typeof onCheckUpdates === 'function') onCheckUpdates();
      return true;
    }

    if (wasHidden && typeof invalidateSessions === 'function') {
      wasHidden = false;
      if (recentlyInvalidated || recentlyRefreshed) return false;
      lastInvalidateAt = currentNow;
      lastRefreshAt = currentNow;
      setResuming(true);
      try {
        await invalidateSessions();
        if (typeof onInvalidate === 'function') onInvalidate();
      } finally {
        setResuming(false);
      }
      triggerRefresh(now());
      if (typeof onCheckUpdates === 'function') onCheckUpdates();
      return true;
    }

    wasHidden = false;
    if (recentlyInvalidated || recentlyRefreshed) return false;
    triggerRefresh(currentNow);
    if (typeof onCheckUpdates === 'function') onCheckUpdates();
    return true;
  }

  async function handleVisibilityChange() {
    const state = getVisibilityState();
    if (state === 'hidden') {
      wasHidden = true;
      // Never trigger background auto-install while resume recovery is re-establishing sessions.
      if (!resuming && typeof onBackground === 'function') onBackground();
      return false;
    }
    if (state !== 'visible') return false;
    return runForeground({ forceInvalidate: false });
  }

  async function handleNativeResume() {
    return runForeground({ forceInvalidate: true });
  }

  function handleFocus() {
    if (resuming || wasHidden || getVisibilityState() !== 'visible') return false;
    const currentNow = now();
    if (currentNow - lastRefreshAt < dedupeMs) return false;
    triggerRefresh(currentNow);
    return true;
  }

  function recordRefresh() {
    lastRefreshAt = now();
  }

  return {
    handleVisibilityChange,
    handleNativeResume,
    handleFocus,
    recordRefresh,
    isResuming: () => resuming,
    isHidden: () => wasHidden,
  };
}
