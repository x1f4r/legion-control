// Browser door implementation of immersive(on, { orientation, element, video }) and keepScreenOn(on).

/**
 * Reports whether this browser supports entering full screen either on the viewer element
 * (Fullscreen API) or on the video element itself (iPhone Safari `webkitEnterFullscreen`).
 */
export function canBrowserImmersive({ element = null, video = null, doc = null } = {}) {
  const d = doc || (typeof document !== 'undefined' ? document : null);
  const el = element || d?.documentElement || null;
  const fsAllowed = d ? d.fullscreenEnabled !== false && d.webkitFullscreenEnabled !== false : true;
  const hasElementFs = Boolean(
    fsAllowed &&
      el &&
      (typeof el.requestFullscreen === 'function' || typeof el.webkitRequestFullscreen === 'function'),
  );
  if (hasElementFs) return true;
  if (video && typeof video.webkitEnterFullscreen === 'function') return true;
  return false;
}

/**
 * Creates a browser immersive + wake-lock controller. Kept self-contained so it can be unit-tested
 * with fake document/screen/navigator objects and shared by the HTTP door and browser-fs mock mode.
 */
export function createBrowserImmersive({
  doc = typeof document !== 'undefined' ? document : null,
  win = typeof window !== 'undefined' ? window : null,
  nav = typeof navigator !== 'undefined' ? navigator : null,
  scr = typeof screen !== 'undefined' ? screen : null,
} = {}) {
  let wakeWanted = false;
  let wakeSentinel = null;
  let wakeInFlight = null;
  let wakeVisListenerInstalled = false;
  let lastVideo = null;

  function isDocFullscreen() {
    if (!doc) return false;
    return Boolean(doc.fullscreenElement || doc.webkitFullscreenElement);
  }

  function canImmersive({ element = null, video = null } = {}) {
    return canBrowserImmersive({ element, video, doc });
  }

  async function immersive(on, options = {}) {
    const { element = null, video = null, orientation = 'none' } = options || {};
    if (video) lastVideo = video;
    const targetVideo = video || lastVideo;

    if (on) {
      const targetEl = element || doc?.documentElement || null;
      const fsAllowed = doc ? doc.fullscreenEnabled !== false && doc.webkitFullscreenEnabled !== false : true;
      const req =
        fsAllowed && targetEl
          ? targetEl.requestFullscreen || targetEl.webkitRequestFullscreen || null
          : null;

      if (typeof req === 'function') {
        try {
          await req.call(targetEl);
        } catch {
          // Fall through to webkitEnterFullscreen if element fullscreen failed.
          if (targetVideo && typeof targetVideo.webkitEnterFullscreen === 'function') {
            try {
              targetVideo.webkitEnterFullscreen();
              return { active: true, mode: 'webkit-video' };
            } catch {
              return { active: false, mode: 'none' };
            }
          }
          return { active: false, mode: 'none' };
        }
        if (orientation === 'landscape' || orientation === 'portrait') {
          try {
            if (scr?.orientation && typeof scr.orientation.lock === 'function') {
              await scr.orientation.lock(orientation);
            }
          } catch {
            // Desktop browsers and some mobile environments disallow orientation.lock; ignore.
          }
        }
        return { active: true, mode: 'element' };
      }

      if (targetVideo && typeof targetVideo.webkitEnterFullscreen === 'function') {
        try {
          targetVideo.webkitEnterFullscreen();
          return { active: true, mode: 'webkit-video' };
        } catch {
          return { active: false, mode: 'none' };
        }
      }
      return { active: false, mode: 'none' };
    }

    // Leaving immersive mode.
    try {
      if (scr?.orientation && typeof scr.orientation.unlock === 'function') {
        scr.orientation.unlock();
      }
    } catch {
      // Ignore unlock failures on browsers without orientation lock.
    }

    if (isDocFullscreen()) {
      const exit = doc?.exitFullscreen || doc?.webkitExitFullscreen || null;
      if (typeof exit === 'function') {
        try {
          await exit.call(doc);
        } catch {
          // Ignore exit failures when fullscreen already ended.
        }
      }
    } else if (targetVideo?.webkitDisplayingFullscreen && typeof targetVideo.webkitExitFullscreen === 'function') {
      try {
        targetVideo.webkitExitFullscreen();
      } catch {
        // Ignore webkit exit failures.
      }
    }
    return { active: false, mode: 'none' };
  }

  function onImmersiveChange(listener) {
    if (!doc || typeof listener !== 'function') return () => {};
    const handle = () => {
      try {
        listener(isDocFullscreen());
      } catch {
        // Keep other listeners running.
      }
    };
    doc.addEventListener?.('fullscreenchange', handle);
    doc.addEventListener?.('webkitfullscreenchange', handle);
    return () => {
      doc.removeEventListener?.('fullscreenchange', handle);
      doc.removeEventListener?.('webkitfullscreenchange', handle);
    };
  }

  function acquireWakeLock() {
    if (!wakeWanted || wakeSentinel) return Promise.resolve();
    if (wakeInFlight) return wakeInFlight;
    if (!nav?.wakeLock || typeof nav.wakeLock.request !== 'function') return Promise.resolve();
    if (doc && doc.visibilityState === 'hidden') return Promise.resolve();
    const req = (async () => {
      try {
        const sentinel = await nav.wakeLock.request('screen');
        if (!wakeWanted || wakeSentinel) {
          try {
            await sentinel.release();
          } catch {
            // Ignore release failure.
          }
          return;
        }
        wakeSentinel = sentinel;
        sentinel.addEventListener?.(
          'release',
          () => {
            if (wakeSentinel === sentinel) wakeSentinel = null;
          },
          { once: true },
        );
      } catch {
        // Wake Lock can be refused (battery saver, background tab); fail silently.
      } finally {
        if (wakeInFlight === req) {
          wakeInFlight = null;
        }
      }
    })();
    wakeInFlight = req;
    return req;
  }

  function handleVisibilityChange() {
    if (wakeWanted && doc?.visibilityState === 'visible' && !wakeSentinel) {
      acquireWakeLock();
    }
  }

  async function keepScreenOn(on) {
    wakeWanted = Boolean(on);
    if (wakeWanted) {
      if (doc && !wakeVisListenerInstalled) {
        wakeVisListenerInstalled = true;
        doc.addEventListener?.('visibilitychange', handleVisibilityChange);
      }
      await acquireWakeLock();
      return Boolean(wakeSentinel);
    }
    if (doc && wakeVisListenerInstalled) {
      wakeVisListenerInstalled = false;
      doc.removeEventListener?.('visibilitychange', handleVisibilityChange);
    }
    const sentinel = wakeSentinel;
    wakeSentinel = null;
    if (sentinel && typeof sentinel.release === 'function') {
      try {
        await sentinel.release();
      } catch {
        // Ignore release failure.
      }
    }
    return false;
  }

  return {
    canImmersive,
    immersive,
    onImmersiveChange,
    keepScreenOn,
    isFullscreen: isDocFullscreen,
    // Exposed for cleanup if needed.
    _win: win,
  };
}
