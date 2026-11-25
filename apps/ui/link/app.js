// Native app link: calls the Tauri 2 shell through window.__TAURI__.core.invoke and Channel.

import { isFanOut } from '../lib/result.js';

const CAPABILITY_TOOLS = new Set(['meta.version', 'meta.whoami', 'meta.tools']);

function tauriCore() {
  const tauri = typeof window !== 'undefined' ? window.__TAURI__ : null;
  if (!tauri || !tauri.core || typeof tauri.core.invoke !== 'function') {
    return null;
  }
  return tauri.core;
}

function tauriEvent() {
  const tauri = typeof window !== 'undefined' ? window.__TAURI__ : null;
  if (!tauri || !tauri.event || typeof tauri.event.listen !== 'function') {
    return null;
  }
  return tauri.event;
}

function invoke(command, payload) {
  const core = tauriCore();
  if (!core) {
    return Promise.reject({
      code: 'unsupported',
      message: 'The native Legion shell is not available in this window.',
      hint: null,
      machine: null,
      details: null,
    });
  }
  return core.invoke(command, payload);
}

function normalizeError(err) {
  if (err && typeof err === 'object' && typeof err.message === 'string') {
    return {
      code: err.code || 'failed',
      message: err.message,
      hint: err.hint || null,
      machine: err.machine || null,
      details: err.details || null,
    };
  }
  return {
    code: 'failed',
    message: typeof err === 'string' ? err : 'The native command failed.',
    hint: null,
    machine: null,
    details: null,
  };
}

function randomCallPrefix() {
  if (typeof crypto !== 'undefined' && typeof crypto.getRandomValues === 'function') {
    const bytes = new Uint8Array(8);
    crypto.getRandomValues(bytes);
    return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
  }
  return Math.random().toString(16).slice(2, 10) + Date.now().toString(16);
}

function isPairingError(err) {
  const reason = err?.details?.reason;
  return (
    err?.code === 'denied' &&
    (reason === 'device_removed_or_unknown' ||
      reason === 'auth_failed' ||
      reason === 'host_key_mismatch')
  );
}

function readCachedNodeIdentity(nodeName) {
  if (!nodeName || typeof window === 'undefined') return null;
  try {
    const raw = window.localStorage?.getItem(`legion.nodeIdentity.${nodeName}`);
    if (!raw) return null;
    const parsed = JSON.parse(raw);
    if (!parsed || typeof parsed !== 'object') return null;
    return parsed;
  } catch {
    return null;
  }
}

function writeCachedNodeIdentity(nodeName, entry) {
  if (!nodeName || typeof window === 'undefined') return;
  try {
    window.localStorage?.setItem(`legion.nodeIdentity.${nodeName}`, JSON.stringify(entry));
  } catch {
    // Ignore storage quota or private-mode failures.
  }
}

function isIosPlatform() {
  if (typeof navigator === 'undefined') return false;
  const ua = String(navigator.userAgent || '');
  if (/iPad|iPhone|iPod/i.test(ua)) return true;
  return navigator.platform === 'MacIntel' && Number(navigator.maxTouchPoints || 0) > 1;
}

function isAndroidPlatform() {
  if (typeof navigator === 'undefined') return false;
  return /Android/i.test(String(navigator.userAgent || ''));
}

/**
 * Reports the real immersive implementation available on the current app platform.
 * Returns `{ supported: false, mode: 'none' }` on mobile platforms whenever the native bridge is
 * absent rather than falling through to the desktop Tauri command.
 */
export function detectAppImmersiveCapability({
  hasAndroidBridge = false,
  isAndroid = false,
  hasIosHandler = false,
  isIos = false,
  hasTauriCore = false,
} = {}) {
  if (hasAndroidBridge) return { supported: true, mode: 'android' };
  if (isAndroid) return { supported: false, mode: 'none' };
  if (hasIosHandler) return { supported: true, mode: 'ios' };
  if (isIos) return { supported: false, mode: 'none' };
  if (hasTauriCore) return { supported: true, mode: 'desktop' };
  return { supported: false, mode: 'none' };
}

export function createAppLink() {
  const callPrefix = randomCallPrefix();
  let nextCallId = 1;
  let readySent = false;
  let selectedNode = '';
  const iosPlatform = isIosPlatform();
  const androidPlatform = isAndroidPlatform();
  const unsupportedContentMachines = new Set();
  const contentSupportListeners = new Set();
  const appResumeListeners = new Set();
  let nativeResumeUnlistens = null;
  let nativeResumeReady = null;
  let nativeResumeToken = null;
  let sawNativeSuspend = false;

  function notifyAppResume(source) {
    for (const listener of appResumeListeners) {
      try {
        listener({ source });
      } catch {
        // Keep notifying remaining listeners.
      }
    }
  }

  function ensureNativeResumeSubscription() {
    if (nativeResumeReady) return nativeResumeReady;
    const ev = tauriEvent();
    if (!ev) return Promise.resolve(false);
    const token = {};
    nativeResumeToken = token;
    const onResumed = () => {
      sawNativeSuspend = false;
      notifyAppResume('tauri://resumed');
    };
    const onSuspended = () => {
      sawNativeSuspend = true;
    };
    const onFocus = () => {
      if (!sawNativeSuspend) return;
      sawNativeSuspend = false;
      notifyAppResume('tauri://focus');
    };
    nativeResumeReady = Promise.all([
      Promise.resolve(ev.listen('tauri://resumed', onResumed)).catch(() => null),
      Promise.resolve(ev.listen('tauri://suspended', onSuspended)).catch(() => null),
      Promise.resolve(ev.listen('tauri://focus', onFocus)).catch(() => null),
    ]).then((unlistens) => {
      const active = unlistens.filter((fn) => typeof fn === 'function');
      if (nativeResumeToken !== token || appResumeListeners.size === 0) {
        for (const fn of active) {
          try {
            const res = fn();
            if (res && typeof res.catch === 'function') res.catch(() => {});
          } catch {
            // Best-effort cleanup.
          }
        }
        return false;
      }
      nativeResumeUnlistens = active;
      return active.length > 0;
    });
    return nativeResumeReady;
  }

  function teardownNativeResumeSubscription() {
    nativeResumeToken = null;
    nativeResumeReady = null;
    sawNativeSuspend = false;
    const active = nativeResumeUnlistens;
    nativeResumeUnlistens = null;
    if (Array.isArray(active)) {
      for (const fn of active) {
        try {
          const res = fn();
          if (res && typeof res.catch === 'function') res.catch(() => {});
        } catch {
          // Best-effort cleanup.
        }
      }
    }
  }

  function onAppResume(listener) {
    if (typeof listener !== 'function') return () => {};
    appResumeListeners.add(listener);
    ensureNativeResumeSubscription().catch(() => {});
    return () => {
      appResumeListeners.delete(listener);
      if (appResumeListeners.size === 0) {
        teardownNativeResumeSubscription();
      }
    };
  }

  function machineKey(machine) {
    return typeof machine === 'string' ? machine.trim() : '';
  }

  function notifyContentSupport() {
    for (const listener of contentSupportListeners) {
      try {
        listener();
      } catch {
        // Keep notifying remaining listeners.
      }
    }
  }

  function markContentUnsupported(machine) {
    const key = machineKey(machine);
    if (!key) return;
    if (!unsupportedContentMachines.has(key)) {
      unsupportedContentMachines.add(key);
      notifyContentSupport();
    }
  }

  function clearContentUnsupported(machines) {
    let changed = false;
    for (const machine of machines) {
      const key = machineKey(machine);
      if (key && unsupportedContentMachines.delete(key)) {
        changed = true;
      }
    }
    if (changed) notifyContentSupport();
  }

  function noteCapabilityContact(tool, targetOn, result) {
    if (!CAPABILITY_TOOLS.has(tool) || !result || typeof result !== 'object') return;
    if (isFanOut(result)) {
      const refreshed = [];
      for (const [name, entry] of Object.entries(result.results || {})) {
        if (entry && typeof entry === 'object' && entry.ok && !entry.error) {
          refreshed.push(name);
          if (typeof entry.ok.node === 'string') refreshed.push(entry.ok.node);
        }
      }
      if (refreshed.length > 0) clearContentUnsupported(refreshed);
      return;
    }

    const onKey = machineKey(targetOn);
    const nodeKey = machineKey(result.node);
    const refreshed = [];
    if (onKey && onKey !== 'all') refreshed.push(onKey);
    if (nodeKey) refreshed.push(nodeKey);
    if (!onKey && selectedNode) refreshed.push(selectedNode);
    if (refreshed.length > 0) clearContentUnsupported(refreshed);
  }

  function clearUnsupportedContentMachines() {
    if (unsupportedContentMachines.size > 0) {
      unsupportedContentMachines.clear();
      notifyContentSupport();
    }
  }

  function supportsContentFor(machine) {
    const key = machineKey(machine);
    return !key || !unsupportedContentMachines.has(key);
  }

  function onContentSupportChange(listener) {
    if (typeof listener !== 'function') return () => {};
    contentSupportListeners.add(listener);
    return () => {
      contentSupportListeners.delete(listener);
    };
  }

  async function runConfiguredMediaCheck() {
    if (typeof document === 'undefined' || !document.body) return;
    let spec;
    try {
      spec = await invoke('media_check_spec');
    } catch {
      return;
    }
    if (!spec || typeof spec !== 'object' || !spec.machine || !spec.image_path || !spec.video_path) {
      return;
    }

    const originOf = (rawUrl) => {
      try {
        return new URL(rawUrl).origin;
      } catch {
        return '';
      }
    };

    const loadImage = (url) =>
      new Promise((resolve, reject) => {
        const img = new Image();
        const timer = window.setTimeout(() => reject(new Error('Image load timed out')), 20000);
        const done = (ok, err) => {
          window.clearTimeout(timer);
          if (ok) {
            resolve({ width: img.naturalWidth, height: img.naturalHeight });
          } else {
            reject(err || new Error('Image failed to load'));
          }
        };
        img.onload = () => {
          if (typeof img.decode === 'function') {
            img.decode().then(
              () => done(true),
              () => done(img.naturalWidth > 0 && img.naturalHeight > 0),
            );
          } else {
            done(true);
          }
        };
        img.onerror = () => done(false, new Error('Image element errored'));
        img.src = url;
      });

    const loadVideo = (url, seekTo) =>
      new Promise((resolve, reject) => {
        const video = document.createElement('video');
        const wantSeek = typeof seekTo === 'number' && Number.isFinite(seekTo) && seekTo > 0;
        video.muted = true;
        video.playsInline = true;
        video.setAttribute('playsinline', '');
        video.preload = wantSeek ? 'metadata' : 'auto';
        video.style.cssText =
          'position:fixed;left:-9999px;top:-9999px;width:64px;height:64px;opacity:0;pointer-events:none;';
        document.body.appendChild(video);
        let seekStarted = false;
        let settled = false;
        let timer = null;
        let pollTimer = null;
        const cleanup = () => {
          if (timer !== null) {
            window.clearTimeout(timer);
            timer = null;
          }
          if (pollTimer !== null) {
            window.clearInterval(pollTimer);
            pollTimer = null;
          }
          video.removeEventListener('loadedmetadata', onMeta);
          video.removeEventListener('loadeddata', onLoaded);
          video.removeEventListener('canplay', onLoaded);
          video.removeEventListener('resize', onLoaded);
          video.removeEventListener('playing', onLoaded);
          video.removeEventListener('timeupdate', onLoaded);
          video.removeEventListener('seeked', onSeeked);
          video.removeEventListener('error', onError);
          try {
            video.pause();
            video.removeAttribute('src');
            video.load();
          } catch {
            // Ignore cleanup errors.
          }
          video.remove();
        };
        const onSeeked = () => {
          if (settled) return;
          settled = true;
          const width = video.videoWidth || 0;
          const height = video.videoHeight || 0;
          const duration = Number.isFinite(video.duration) ? video.duration : 0;
          const seekedTime = Number.isFinite(video.currentTime) ? video.currentTime : 0;
          cleanup();
          resolve({ width, height, duration, seekedTime });
        };
        const onMeta = () => {
          if (wantSeek && !seekStarted) {
            seekStarted = true;
            video.currentTime = seekTo;
          }
        };
        const onLoaded = () => {
          if (settled) return;
          const width = video.videoWidth || 0;
          const height = video.videoHeight || 0;
          const duration = Number.isFinite(video.duration) ? video.duration : 0;
          if (width <= 0 || height <= 0) return;
          if (wantSeek) {
            if (!seekStarted) {
              seekStarted = true;
              video.currentTime = seekTo;
            }
            return;
          }
          settled = true;
          cleanup();
          resolve({ width, height, duration, seekedTime: null });
        };
        const onError = () => {
          if (settled) return;
          settled = true;
          const code = video.error ? video.error.code : 'unknown';
          const msg = video.error?.message || '';
          cleanup();
          reject(new Error(`Video error code=${code} ${msg}`.trim()));
        };
        timer = window.setTimeout(() => {
          if (settled) return;
          settled = true;
          const st = `readyState=${video.readyState} net=${video.networkState} t=${video.currentTime} w=${video.videoWidth || 0} h=${video.videoHeight || 0}`;
          cleanup();
          reject(new Error(`Video load timed out (${st})`));
        }, 30000);
        pollTimer = window.setInterval(onLoaded, 250);
        video.addEventListener('loadedmetadata', onMeta);
        video.addEventListener('loadeddata', onLoaded);
        video.addEventListener('canplay', onLoaded);
        video.addEventListener('resize', onLoaded);
        video.addEventListener('playing', onLoaded);
        video.addEventListener('timeupdate', onLoaded);
        video.addEventListener('seeked', onSeeked);
        video.addEventListener('error', onError);
        video.src = url;
        video.load();
      });

    const verifyContentIpcIsolation = async (machine, htmlPath, svgPath) => {
      const checks = [];
      if (htmlPath) checks.push({ kind: 'html', path: htmlPath });
      if (svgPath) checks.push({ kind: 'svg', path: svgPath });
      if (checks.length === 0) return null;

      const appOrigin = window.location.origin;
      let scriptProbe = null;
      const onMsg = (evt) => {
        if (evt?.data && typeof evt.data === 'object' && evt.data.legionProbe) {
          scriptProbe = evt.data;
        }
      };
      window.addEventListener('message', onMsg);
      try {
        for (const item of checks) {
          const ticket = await contentUrl({ machine, path: item.path });
          // 1) App CSP restricts connect-src to Tauri IPC only, so fetch() to the content origin
          // must either fail under CSP or (if ever permitted) return sandbox + frame-ancestors 'none'.
          if (typeof fetch === 'function') {
            try {
              const resp = await fetch(ticket.url);
              const csp = String(resp.headers.get('content-security-policy') || '').toLowerCase();
              const nosniff = String(resp.headers.get('x-content-type-options') || '').toLowerCase();
              const ctype = String(resp.headers.get('content-type') || '').toLowerCase();
              if (!csp.includes('sandbox') || !csp.includes("frame-ancestors 'none'")) {
                throw new Error(`Missing sandbox/frame-ancestors CSP on ${item.kind}: ${csp}`);
              }
              if (nosniff !== 'nosniff') {
                throw new Error(`Missing nosniff on ${item.kind}: ${nosniff}`);
              }
              if (item.kind === 'html' && ctype.includes('text/html')) {
                throw new Error(`HTML content must not be served as text/html: ${ctype}`);
              }
            } catch (fetchErr) {
              const msg = String(fetchErr?.message || fetchErr || '');
              if (msg.startsWith('Missing ') || msg.startsWith('HTML content ')) {
                throw fetchErr;
              }
              // Expected: strict app CSP `connect-src` blocks fetch() to the content origin.
            }
          }

          // 2) For SVG, load through <img> (allowed by img-src) and confirm it renders without
          // executing its embedded <script>.
          if (item.kind === 'svg') {
            const svgImg = await loadImage(ticket.url);
            if (svgImg.width <= 0 || svgImg.height <= 0) {
              throw new Error('SVG content image did not decode');
            }
          }

          // 3) Attempt to frame the content URL and ensure neither script nor IPC is reachable.
          // End the wait as soon as the frame is conclusively cross-origin (SecurityError on
          // location.href) or fires load/error, capping silent CSP cancellations at 400 ms. When
          // CSP `default-src 'none'` or WebView2's `on_navigation` blocks navigation before
          // commit, the iframe can remain on its initial untouched empty `about:blank` document
          // (which inherits the app origin and receives Tauri's `withGlobalTauri` init script on
          // WebView2). Only fail if content actually framed into a same-origin document (navigated
          // away from `about:blank` or populated DOM) or ran script.
          const iframe = document.createElement('iframe');
          iframe.style.cssText = 'position:fixed;left:-9999px;top:-9999px;width:10px;height:10px;';
          const isIframeCrossOrigin = () => {
            try {
              void iframe.contentWindow?.location?.href;
              return false;
            } catch {
              return true;
            }
          };
          await new Promise((resolve) => {
            let done = false;
            const finish = (graceMs = 0) => {
              if (done) return;
              done = true;
              window.clearTimeout(capTimer);
              window.clearInterval(pollTimer);
              if (graceMs > 0 && !isIframeCrossOrigin()) {
                window.setTimeout(resolve, graceMs);
              } else {
                resolve();
              }
            };
            const capTimer = window.setTimeout(() => finish(0), 400);
            const pollTimer = window.setInterval(() => {
              if (scriptProbe || isIframeCrossOrigin()) finish(0);
            }, 40);
            iframe.addEventListener('load', () => finish(50), { once: true });
            iframe.addEventListener('error', () => finish(0), { once: true });
            iframe.src = ticket.url;
            document.body.appendChild(iframe);
          });

          let iframeLeak = null;
          try {
            const win = iframe.contentWindow;
            if (win) {
              const href = String(win.location?.href || '');
              const doc = win.document;
              const rootTag = String(doc?.documentElement?.tagName || '').toLowerCase();
              const hasDom = Boolean(
                doc &&
                  ((doc.head?.childNodes?.length || 0) > 0 ||
                    (doc.body?.childNodes?.length || 0) > 0 ||
                    (rootTag && rootTag !== 'html')),
              );
              const isUntouchedBlank = href === 'about:blank' && !hasDom;
              if (!isUntouchedBlank) {
                iframeLeak = `href=${href || '<empty>'} tauri=${Boolean(win.__TAURI__)} dom=${hasDom}`;
              }
            }
          } catch {
            // Cross-origin SecurityError (opaque sandbox or error page) proves isolation.
          }
          iframe.remove();
          if (iframeLeak) {
            throw new Error(`Content ${item.kind} framed into same-origin document (${iframeLeak})`);
          }

          // 4) Attempt top-level navigation to the content URL; on_navigation must refuse it so
          // the WebView stays on the bundled app origin.
          try {
            window.location.assign(ticket.url);
          } catch {
            // Blocked synchronously.
          }
          await new Promise((r) => window.setTimeout(r, 300));
          if (window.location.origin !== appOrigin) {
            throw new Error(`WebView navigated to content origin for ${item.kind}`);
          }
          if (scriptProbe) {
            throw new Error(
              `Content ${item.kind} executed script (ipc=${Boolean(scriptProbe.ipc)})`,
            );
          }
        }
        return true;
      } finally {
        window.removeEventListener('message', onMsg);
      }
    };

    const createAppResumeWaiter = () => {
      let settled = false;
      let sawBackground = false;
      let resolvePromise = null;
      let rejectPromise = null;
      let unsubNative = null;
      let timer = null;

      const cleanup = () => {
        if (timer !== null) {
          window.clearTimeout(timer);
          timer = null;
        }
        if (typeof unsubNative === 'function') {
          unsubNative();
          unsubNative = null;
        }
        document.removeEventListener('visibilitychange', onVis);
        window.removeEventListener('pagehide', onBg);
        window.removeEventListener('blur', onBg);
        window.removeEventListener('focus', onFg);
        window.removeEventListener('pageshow', onFg);
      };

      const trigger = (source) => {
        if (settled) return;
        settled = true;
        cleanup();
        resolvePromise(source);
      };

      const onBg = () => {
        sawBackground = true;
      };
      const onFg = () => {
        if (sawBackground && document.visibilityState !== 'hidden') {
          trigger('dom-foreground');
        }
      };
      const onVis = () => {
        if (document.visibilityState === 'hidden') {
          sawBackground = true;
        } else if (sawBackground) {
          trigger('dom-visibility');
        }
      };

      const promise = new Promise((resolve, reject) => {
        resolvePromise = resolve;
        rejectPromise = reject;
      });

      // Native mobile lifecycle resume (`tauri://resumed`, or `tauri://suspended` -> `tauri://focus`)
      // resolves directly even if the web view never saw `visibilityState === 'hidden'`.
      unsubNative = onAppResume((ev) => {
        trigger(ev?.source || 'tauri://resumed');
      });
      const ready = ensureNativeResumeSubscription().catch(() => false);

      document.addEventListener('visibilitychange', onVis);
      window.addEventListener('pagehide', onBg);
      window.addEventListener('blur', onBg);
      window.addEventListener('focus', onFg);
      window.addEventListener('pageshow', onFg);

      // Leave room for up to three simulator retry cycles before timing out inside JS.
      timer = window.setTimeout(() => {
        if (settled) return;
        settled = true;
        cleanup();
        rejectPromise(new Error('Timed out waiting for app suspend/resume cycle'));
      }, 180000);

      return { ready, promise };
    };

    const report = {
      ok: false,
      image_width: 0,
      image_height: 0,
      video_width: 0,
      video_height: 0,
      video_duration: 0,
      image_origin: '',
      video_origin: '',
      ipc_isolation_ok: null,
      resume_ok: spec.verify_resume ? false : null,
      immersive_check: null,
      resume_trigger: null,
      error: null,
    };

    const verifyDesktopImmersive = async () => {
      const waitTick = (ms = 150) => new Promise((r) => window.setTimeout(r, ms));
      const waitForFullscreen = async (expected, timeoutMs = 1500) => {
        const deadline = Date.now() + timeoutMs;
        let last = await invoke('window_is_fullscreen');
        while (last !== expected && Date.now() < deadline) {
          await waitTick(40);
          last = await invoke('window_is_fullscreen');
        }
        return last;
      };

      const initial = await invoke('window_is_fullscreen');
      const entered = await immersive(true, { orientation: 'landscape' });
      const during = await waitForFullscreen(true);
      const left = await immersive(false);
      const afterLeave = await waitForFullscreen(false);

      // Sequence A: WM makes window full screen -> player enters -> player exits -> window stays full screen.
      await invoke('debug_set_window_fullscreen', { fullscreen: true });
      const preFullscreen = await waitForFullscreen(true);
      const enteredPre = await immersive(true, { orientation: 'landscape' });
      const duringPreFullscreen = await waitForFullscreen(true);
      const leftPre = await immersive(false);
      await waitTick(150);
      const afterPreFullscreen = await invoke('window_is_fullscreen');
      await invoke('debug_set_window_fullscreen', { fullscreen: false });
      const cleanedUp = await waitForFullscreen(false);

      // Sequence B: Player enters full screen -> WM leaves full screen -> player syncs off.
      const observedChanges = [];
      const unsubImmersive = onImmersiveChange((active) => {
        observedChanges.push(Boolean(active));
      });
      await ensureNativeImmersiveSubscription().catch(() => false);
      const enteredSeqB = await immersive(true, { orientation: 'landscape' });
      const duringSeqB = await waitForFullscreen(true);
      await invoke('debug_set_window_fullscreen', { fullscreen: false });
      const afterWmExit = await waitForFullscreen(false);
      const notifyDeadline = Date.now() + 1500;
      while (!observedChanges.includes(false) && Date.now() < notifyDeadline) {
        await waitTick(40);
      }
      unsubImmersive();
      const wmExitNotified = observedChanges.includes(false);

      const ok =
        initial === false &&
        entered?.active === true &&
        during === true &&
        left?.active === false &&
        afterLeave === false &&
        preFullscreen === true &&
        enteredPre?.active === true &&
        duringPreFullscreen === true &&
        leftPre?.active === false &&
        afterPreFullscreen === true &&
        cleanedUp === false &&
        enteredSeqB?.active === true &&
        duringSeqB === true &&
        afterWmExit === false &&
        wmExitNotified === true;
      return {
        ok,
        initial,
        during,
        afterLeave,
        preFullscreen,
        duringPreFullscreen,
        afterPreFullscreen,
        cleanedUp,
        duringSeqB,
        afterWmExit,
        wmExitNotified,
      };
    };

    try {
      if (spec.invite) {
        const current = await rawState().catch(() => null);
        const alreadyPaired =
          Array.isArray(current?.nodes) && current.nodes.some((n) => n.node === spec.machine);
        if (!alreadyPaired) {
          try {
            await pair(spec.invite, 'ci-media');
          } catch (pairErr) {
            if (pairErr?.code !== 'timeout') throw pairErr;
            // On a cold CI simulator runner, the initial SSH handshake can exceed 10s without
            // consuming the invite; retry pairing once.
            await new Promise((r) => window.setTimeout(r, 500));
            await pair(spec.invite, 'ci-media');
          }
        }
      }

      const [imgTicket, vidTicket] = await Promise.all([
        contentUrl({ machine: spec.machine, path: spec.image_path }),
        contentUrl({ machine: spec.machine, path: spec.video_path }),
      ]);
      report.image_origin = originOf(imgTicket?.url || '');
      report.video_origin = originOf(vidTicket?.url || '');

      const imgResult = await loadImage(imgTicket.url);
      report.image_width = imgResult.width;
      report.image_height = imgResult.height;

      const vidResult = await loadVideo(vidTicket.url, spec.seek_to);
      report.video_width = vidResult.width;
      report.video_height = vidResult.height;
      report.video_duration = vidResult.duration;
      if (typeof vidResult.seekedTime === 'number') {
        report.seeked_time = vidResult.seekedTime;
      }

      if (spec.html_path || spec.svg_path) {
        report.ipc_isolation_ok = await verifyContentIpcIsolation(
          spec.machine,
          spec.html_path,
          spec.svg_path,
        );
      }

      if (spec.verify_immersive) {
        report.immersive_check = await verifyDesktopImmersive();
      }

      const mediaPass =
        report.image_width > 0 &&
        report.image_height > 0 &&
        report.video_width > 0 &&
        report.video_height > 0 &&
        (typeof spec.seek_to !== 'number' ||
          (typeof vidResult.seekedTime === 'number' && Math.abs(vidResult.seekedTime - spec.seek_to) < 5)) &&
        (report.ipc_isolation_ok === null || report.ipc_isolation_ok === true) &&
        (report.immersive_check === null || report.immersive_check.ok === true);
      report.ok = mediaPass;

      if (spec.verify_resume && mediaPass) {
        // Register both native (`tauri://resumed`) and DOM resume listeners and await Tauri
        // listener registration before signaling the simulator harness.
        const resumeWaiter = createAppResumeWaiter();
        await resumeWaiter.ready;
        // Emit intermediate report so the simulator harness knows initial playback succeeded and
        // can now background and foreground the app.
        await invoke('media_check_report', { report: { ...report, resume_ok: false } }).catch(() => {});
        report.resume_trigger = await resumeWaiter.promise;
        // Mint immediately on resume: content_url probes and recovers the loopback listener
        // synchronously under its lock without relying on an arbitrary sleep.
        const [imgAfter, vidAfter] = await Promise.all([
          contentUrl({ machine: spec.machine, path: spec.image_path }),
          contentUrl({ machine: spec.machine, path: spec.video_path }),
        ]);
        const imgRes2 = await loadImage(imgAfter.url);
        const vidRes2 = await loadVideo(vidAfter.url, spec.seek_to);
        report.resume_ok =
          imgRes2.width > 0 && imgRes2.height > 0 && vidRes2.width > 0 && vidRes2.height > 0;
        report.ok = mediaPass && report.resume_ok;
      }
    } catch (err) {
      report.ok = false;
      report.error = err?.message ? String(err.message) : String(err);
    }

    try {
      await invoke('media_check_report', { report });
    } catch {
      // Ignore reporting failures during shutdown.
    }
  }

  function signalReady() {
    if (readySent) return;
    readySent = true;
    let fired = false;
    const fire = () => {
      if (fired) return;
      fired = true;
      invoke('ready')
        .catch(() => {})
        .finally(() => {
          runConfiguredMediaCheck().catch(() => {});
        });
    };
    if (typeof window !== 'undefined' && typeof window.requestAnimationFrame === 'function') {
      window.requestAnimationFrame(() => window.requestAnimationFrame(fire));
      window.setTimeout(fire, 250);
    } else {
      fire();
    }
  }
  function ensureLocalNetworkPermission() {
    if (typeof window === 'undefined') return Promise.resolve(true);
    const android = window.__LEGION_ANDROID__;
    if (!android || typeof android.requestLocalNetwork !== 'function') {
      return Promise.resolve(true);
    }
    try {
      if (typeof android.hasLocalNetwork === 'function' && android.hasLocalNetwork() === true) {
        return Promise.resolve(true);
      }
      return Promise.resolve(android.requestLocalNetwork()).then(Boolean, () => true);
    } catch {
      return Promise.resolve(true);
    }
  }

  async function rawState() {
    try {
      return await invoke('state');
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function tools(options = {}) {
    try {
      const result = await invoke('tools', { on: options?.on ?? null });
      const target = machineKey(options?.on) || selectedNode;
      if (target && target !== 'all') {
        clearContentUnsupported([target]);
      }
      return result;
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function call(tool, args = {}, options = {}) {
    const { on, onEvent, signal } = options || {};
    const id = `${callPrefix}-${nextCallId++}`;
    const core = tauriCore();

    const channel = core && typeof core.Channel === 'function' ? new core.Channel() : null;
    if (channel && typeof onEvent === 'function') {
      channel.onmessage = (evt) => {
        try {
          onEvent(evt);
        } catch {
          // Keep receiving events if a view callback throws.
        }
      };
    }

    const onAbort = () => {
      invoke('cancel', { id }).catch(() => {});
    };
    if (signal) {
      if (signal.aborted) {
        throw {
          code: 'cancelled',
          message: 'The call was cancelled.',
          hint: null,
          machine: null,
          details: null,
        };
      }
      signal.addEventListener('abort', onAbort, { once: true });
    }

    try {
      const payload = {
        id,
        tool,
        args: args && typeof args === 'object' ? args : {},
        on: on ?? null,
      };
      if (channel) {
        payload.onEvent = channel;
      }
      const result = await invoke('call', payload);
      noteCapabilityContact(tool, on, result);
      return result;
    } catch (err) {
      throw normalizeError(err);
    } finally {
      if (signal) signal.removeEventListener('abort', onAbort);
    }
  }

  async function state() {
    try {
      const base = await rawState();
      if (!base?.ready || !Array.isArray(base.nodes) || base.nodes.length === 0) {
        selectedNode = '';
        return base;
      }

      await ensureLocalNetworkPermission();

      const primary = base.nodes[0];
      const targetNode = primary.node;
      selectedNode = machineKey(targetNode);

      try {
        const [whoami, version] = await Promise.all([
          call('meta.whoami', {}, { on: targetNode }),
          call('meta.version', {}, { on: targetNode }).catch(() => null),
        ]);
        if (!(primary.via === 'local' && targetNode === 'local') && whoami?.node && whoami.node !== targetNode) {
          throw {
            code: 'denied',
            message: `Expected identity from ${targetNode}, but ${whoami.node} answered.`,
            hint: 'Check the connection settings for this machine.',
            machine: targetNode,
            details: { reason: 'node_mismatch', expected: targetNode, actual: whoami.node },
          };
        }
        const checkedAt = new Date().toISOString();
        const latest = await rawState().catch(() => base);
        const nodes =
          Array.isArray(latest?.nodes) && latest.nodes.length > 0 ? latest.nodes : base.nodes;
        const currentPrimary = nodes[0]?.node === targetNode ? nodes[0] : primary;
        const resolvedPrincipal = whoami.principal || currentPrimary.principal;
        const resolvedRole = whoami.role || currentPrimary.role;
        writeCachedNodeIdentity(targetNode, {
          principal: resolvedPrincipal,
          role: resolvedRole,
          tools: Array.isArray(whoami.tools) ? whoami.tools : null,
          checkedAt,
        });
        const enriched = {
          ...currentPrimary,
          node: targetNode,
          principal: resolvedPrincipal,
          role: resolvedRole,
          reachable: true,
          whoamiStale: false,
          roleCheckedAt: checkedAt,
          unreachableError: null,
          version: version?.version || '',
          build: version?.build || null,
          protocol: version?.protocol ?? 1,
          whoami,
        };
        return {
          ...latest,
          nodes: [enriched, ...nodes.filter((n) => n.node !== targetNode)],
        };
      } catch (err) {
        const normalized = normalizeError(err);
        if (isPairingError(normalized)) {
          return {
            ...base,
            error: normalized,
          };
        }
        // Selected machine did not answer: keep it selected and mark it unreachable with its
        // last known role ("as of <when>"), never substituting another node's identity.
        const cached = readCachedNodeIdentity(targetNode);
        const lastRole = cached?.role || (primary.role && primary.role !== 'unknown' ? primary.role : null);
        const lastPrincipal = cached?.principal || primary.principal || '';
        const checkedAt = cached?.checkedAt || primary.role_checked_at || null;
        const offlinePrimary = {
          ...primary,
          node: targetNode,
          principal: lastPrincipal,
          role: lastRole || primary.role || 'unknown',
          reachable: false,
          whoamiStale: true,
          roleCheckedAt: checkedAt,
          unreachableError: normalized,
          whoami: lastRole
            ? {
                node: targetNode,
                principal: lastPrincipal,
                role: lastRole,
                tools: Array.isArray(cached?.tools) ? cached.tools : null,
                stale: true,
              }
            : null,
        };
        return {
          ...base,
          nodes: [offlinePrimary, ...base.nodes.slice(1)],
        };
      }
    } finally {
      signalReady();
    }
  }

  async function deviceName() {
    if (
      typeof window !== 'undefined' &&
      window.__LEGION_ANDROID__ &&
      typeof window.__LEGION_ANDROID__.deviceModel === 'function'
    ) {
      const model = String(window.__LEGION_ANDROID__.deviceModel() || '').trim();
      if (model) return model;
    }
    try {
      const name = await invoke('device_name');
      return typeof name === 'string' ? name : '';
    } catch {
      return '';
    }
  }

  async function pair(inviteText, name) {
    const granted = await ensureLocalNetworkPermission();
    if (!granted) {
      throw {
        code: 'denied',
        message: 'Local network permission was denied.',
        hint: 'Allow local network access for Legion in Android Settings to connect to machines on your LAN or Tailscale.',
        machine: null,
        details: null,
      };
    }
    try {
      const paired = await invoke('pair', { invite: inviteText, name: name || null });
      clearUnsupportedContentMachines();
      const next = await state();
      return {
        ...next,
        last_paired: paired?.last_paired || next?.last_paired || null,
      };
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function learn() {
    try {
      return await invoke('learn');
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function acceptIdentityChange(node, fingerprints) {
    try {
      await invoke('accept_identity_change', {
        node,
        fingerprints: Array.isArray(fingerprints) ? fingerprints : [],
      });
      clearUnsupportedContentMachines();
      return await state();
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function dismissIdentityChange(node) {
    try {
      await invoke('dismiss_identity_change', { node });
      return await state();
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function unpair(node) {
    try {
      const res = await invoke('unpair', { node });
      clearUnsupportedContentMachines();
      return res;
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function selectNode(node) {
    try {
      await invoke('select_node', { node });
      selectedNode = machineKey(node);
      return await state();
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function repairStore() {
    try {
      const result = await invoke('repair_store');
      clearUnsupportedContentMachines();
      const nextState = await state();
      return {
        archived_to: result?.archived_to || '',
        message: result?.message || '',
        state: nextState,
      };
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function invalidateSessions() {
    try {
      await invoke('invalidate_sessions');
      clearUnsupportedContentMachines();
    } catch {
      // Best-effort teardown on resume; subsequent calls still reconnect on error.
    }
  }

  async function scanInvite() {
    try {
      const reply = await invoke('scan_invite');
      if (typeof reply === 'string') return reply;
      if (reply && (reply.use_plugin || reply.usePlugin || reply.mode === 'use_plugin')) {
        const scanned = await invoke('plugin:barcode-scanner|scan', {
          windowed: false,
          formats: ['QR_CODE'],
        });
        if (typeof scanned === 'string') return scanned;
        return scanned?.content || '';
      }
      return reply?.invite || reply?.content || '';
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function updateState() {
    try {
      return await invoke('update_state');
    } catch (err) {
      throw normalizeError(err);
    }
  }

  function watchUpdates(onState) {
    const core = tauriCore();
    if (!core || typeof core.Channel !== 'function' || typeof onState !== 'function') {
      return () => {};
    }
    let active = true;
    const channel = new core.Channel();
    channel.onmessage = (st) => {
      if (!active) return;
      try {
        onState(st);
      } catch {
        // Ignore view callback errors.
      }
    };
    invoke('update_watch', { onState: channel }).catch(() => {});
    return () => {
      active = false;
    };
  }

  async function checkUpdates() {
    try {
      return await invoke('update_check');
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function installUpdate() {
    try {
      return await invoke('update_install');
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function setAutoUpdate(enabled) {
    try {
      return await invoke('update_set_auto', { enabled: Boolean(enabled) });
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function dismissUpdateNotice() {
    try {
      return await invoke('update_dismiss_notice');
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function maybeAutoInstall(trigger = 'startup') {
    try {
      return await invoke('update_maybe_auto_install', { trigger });
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function contentUrl(req = {}) {
    const { machine, path, download = false, signal } = req || {};
    if (signal?.aborted) {
      throw {
        code: 'cancelled',
        message: 'The content request was cancelled.',
        hint: null,
        machine: machine || null,
        details: null,
      };
    }
    try {
      const result = await invoke('content_url', {
        machine: machine || '',
        path: path || '',
        download: Boolean(download),
      });
      clearContentUnsupported([machine || selectedNode]);
      if (signal?.aborted) {
        if (result?.url) {
          invoke('content_revoke', { url: String(result.url) }).catch(() => {});
        }
        throw {
          code: 'cancelled',
          message: 'The content request was cancelled.',
          hint: null,
          machine: machine || null,
          details: null,
        };
      }
      return result;
    } catch (err) {
      const normalized = normalizeError(err);
      if (
        normalized.code === 'unsupported' &&
        normalized.details?.reason !== 'too_large_for_inline'
      ) {
        const target = machineKey(machine) || machineKey(normalized.machine);
        markContentUnsupported(target);
        if (normalized.machine && machineKey(normalized.machine) !== target) {
          markContentUnsupported(normalized.machine);
        }
      }
      throw normalized;
    }
  }

  async function contentStatus(url) {
    try {
      return await invoke('content_status', { url: String(url || '') });
    } catch {
      return null;
    }
  }

  async function revokeContentUrl(url) {
    const raw = String(url || '').trim();
    if (!raw) return false;
    try {
      return Boolean(await invoke('content_revoke', { url: raw }));
    } catch {
      return false;
    }
  }

  async function download(req = {}) {
    const { machine, path, name, size, mime } = req || {};
    try {
      return await invoke('transfer_download', {
        machine: machine || '',
        path: path || '',
        name: name ?? null,
        size: typeof size === 'number' && Number.isFinite(size) ? size : null,
        mime: mime ?? null,
      });
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function pickUploads() {
    try {
      const picked = await invoke('transfer_pick_uploads');
      return Array.isArray(picked) ? picked : [];
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function upload(req = {}) {
    const { machine, dir, handles } = req || {};
    try {
      return await invoke('transfer_upload', {
        machine: machine || '',
        dir: dir || '',
        handles: Array.isArray(handles) ? handles : null,
      });
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function openExternally(req = {}) {
    const { machine, path, name, size, mime } = req || {};
    try {
      return await invoke('transfer_open_externally', {
        machine: machine || '',
        path: path || '',
        name: name ?? null,
        size: typeof size === 'number' && Number.isFinite(size) ? size : null,
        mime: mime ?? null,
      });
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function transfers() {
    try {
      const list = await invoke('transfers');
      return Array.isArray(list) ? list : [];
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function transferControl(id, action) {
    try {
      return await invoke('transfer_control', { id, action });
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function openTransfer(id) {
    try {
      return await invoke('open_transfer', { id });
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function showTransferInFolder(id) {
    try {
      return await invoke('show_transfer_in_folder', { id });
    } catch (err) {
      throw normalizeError(err);
    }
  }

  function onTransferEvent(listener) {
    const core = tauriCore();
    if (!core || typeof core.Channel !== 'function' || typeof listener !== 'function') {
      return () => {};
    }
    let active = true;
    let watcherId = null;
    let unwatchSent = false;
    const channel = new core.Channel();
    channel.onmessage = (list) => {
      if (!active) return;
      try {
        listener(Array.isArray(list) ? list : []);
      } catch {
        // Keep receiving queue updates if a view callback throws.
      }
    };
    const releaseWatcher = () => {
      active = false;
      if (unwatchSent || watcherId == null) return;
      unwatchSent = true;
      const id = watcherId;
      invoke('transfer_unwatch', { watcherId: id }).catch(() => {});
    };
    const onUnload = () => {
      releaseWatcher();
    };
    if (typeof window !== 'undefined' && typeof window.addEventListener === 'function') {
      window.addEventListener('pagehide', onUnload);
      window.addEventListener('beforeunload', onUnload);
    }
    invoke('transfer_watch', { onEvent: channel })
      .then((id) => {
        if (typeof id === 'number') {
          watcherId = id;
          if (!active) releaseWatcher();
        }
      })
      .catch(() => {});
    return () => {
      if (typeof window !== 'undefined' && typeof window.removeEventListener === 'function') {
        window.removeEventListener('pagehide', onUnload);
        window.removeEventListener('beforeunload', onUnload);
      }
      releaseWatcher();
    };
  }

  async function transferSettings() {
    try {
      return await invoke('transfer_settings');
    } catch (err) {
      throw normalizeError(err);
    }
  }

  async function setTransferSettings(req = {}) {
    try {
      return await invoke('transfer_set_settings', {
        wifiOnlyLarge: Boolean(req?.wifi_only_large ?? req?.wifiOnlyLarge),
      });
    } catch (err) {
      throw normalizeError(err);
    }
  }

  let desktopImmersiveActive = false;
  const immersiveListeners = new Set();
  const windowHiddenListeners = new Set();
  let nativeImmersiveUnlisten = null;
  let nativeImmersiveReady = null;
  let nativeImmersiveToken = null;
  let nativeWindowHiddenUnlisten = null;
  let nativeWindowHiddenReady = null;
  let nativeWindowHiddenToken = null;

  function notifyImmersiveListeners(active) {
    for (const listener of immersiveListeners) {
      try {
        listener(Boolean(active));
      } catch {
        // Keep notifying remaining listeners.
      }
    }
  }

  function notifyWindowHiddenListeners() {
    if (desktopImmersiveActive) {
      desktopImmersiveActive = false;
      notifyImmersiveListeners(false);
    }
    for (const listener of windowHiddenListeners) {
      try {
        listener();
      } catch {
        // Keep notifying remaining listeners.
      }
    }
  }

  function ensureNativeWindowHiddenSubscription() {
    if (nativeWindowHiddenReady) return nativeWindowHiddenReady;
    const ev = tauriEvent();
    if (!ev) return Promise.resolve(false);
    const token = {};
    nativeWindowHiddenToken = token;
    const onHidden = () => {
      notifyWindowHiddenListeners();
    };
    nativeWindowHiddenReady = Promise.resolve(ev.listen('legion://window-hidden', onHidden))
      .catch(() => null)
      .then((unlisten) => {
        if (typeof unlisten !== 'function') return false;
        if (nativeWindowHiddenToken !== token || windowHiddenListeners.size === 0) {
          try {
            const res = unlisten();
            if (res && typeof res.catch === 'function') res.catch(() => {});
          } catch {
            // Best-effort cleanup.
          }
          return false;
        }
        nativeWindowHiddenUnlisten = unlisten;
        return true;
      });
    return nativeWindowHiddenReady;
  }

  function teardownNativeWindowHiddenSubscription() {
    nativeWindowHiddenToken = null;
    nativeWindowHiddenReady = null;
    const unlisten = nativeWindowHiddenUnlisten;
    nativeWindowHiddenUnlisten = null;
    if (typeof unlisten === 'function') {
      try {
        const res = unlisten();
        if (res && typeof res.catch === 'function') res.catch(() => {});
      } catch {
        // Best-effort cleanup.
      }
    }
  }

  function onWindowHidden(listener) {
    if (typeof listener !== 'function') return () => {};
    windowHiddenListeners.add(listener);
    if (currentImmersiveCap().mode === 'desktop') {
      ensureNativeWindowHiddenSubscription().catch(() => {});
    }
    return () => {
      windowHiddenListeners.delete(listener);
      if (windowHiddenListeners.size === 0) {
        teardownNativeWindowHiddenSubscription();
      }
    };
  }

  function ensureNativeImmersiveSubscription() {
    if (nativeImmersiveReady) return nativeImmersiveReady;
    const ev = tauriEvent();
    if (!ev) return Promise.resolve(false);
    const token = {};
    nativeImmersiveToken = token;
    const onChanged = (event) => {
      const next = Boolean(event?.payload);
      if (!next && desktopImmersiveActive) {
        desktopImmersiveActive = false;
        notifyImmersiveListeners(false);
      }
    };
    nativeImmersiveReady = Promise.resolve(ev.listen('legion://immersive-changed', onChanged))
      .catch(() => null)
      .then((unlisten) => {
        if (typeof unlisten !== 'function') return false;
        if (nativeImmersiveToken !== token || immersiveListeners.size === 0) {
          try {
            const res = unlisten();
            if (res && typeof res.catch === 'function') res.catch(() => {});
          } catch {
            // Best-effort cleanup.
          }
          return false;
        }
        nativeImmersiveUnlisten = unlisten;
        return true;
      });
    return nativeImmersiveReady;
  }

  function teardownNativeImmersiveSubscription() {
    nativeImmersiveToken = null;
    nativeImmersiveReady = null;
    const unlisten = nativeImmersiveUnlisten;
    nativeImmersiveUnlisten = null;
    if (typeof unlisten === 'function') {
      try {
        const res = unlisten();
        if (res && typeof res.catch === 'function') res.catch(() => {});
      } catch {
        // Best-effort cleanup.
      }
    }
  }

  function androidBridge() {
    if (typeof window === 'undefined') return null;
    const bridge = window.__LEGION_ANDROID__;
    return bridge && typeof bridge.setImmersive === 'function' ? bridge : null;
  }

  function iosImmersiveHandler() {
    if (typeof window === 'undefined') return null;
    const handler = window.webkit?.messageHandlers?.legionImmersive;
    return handler && typeof handler.postMessage === 'function' ? handler : null;
  }

  function currentImmersiveCap() {
    return detectAppImmersiveCapability({
      hasAndroidBridge: Boolean(androidBridge()),
      isAndroid: androidPlatform,
      hasIosHandler: Boolean(iosImmersiveHandler()),
      isIos: iosPlatform,
      hasTauriCore: Boolean(tauriCore()),
    });
  }

  function canImmersive() {
    return currentImmersiveCap().supported;
  }

  async function immersive(on, options = {}) {
    const active = Boolean(on);
    const orientation = typeof options?.orientation === 'string' ? options.orientation : 'none';
    const cap = currentImmersiveCap();

    if (cap.mode === 'android') {
      const android = androidBridge();
      const ok = await Promise.resolve(android.setImmersive(active, orientation)).catch(() => false);
      return { active: Boolean(ok), mode: 'android' };
    }

    if (cap.mode === 'ios') {
      const ios = iosImmersiveHandler();
      try {
        ios.postMessage({ type: 'immersive', on: active, orientation });
        return { active, mode: 'ios' };
      } catch {
        return { active: false, mode: 'ios' };
      }
    }

    if (cap.mode !== 'desktop') {
      return { active: false, mode: 'none' };
    }

    try {
      const res = await invoke('window_set_immersive', { on: active });
      desktopImmersiveActive = Boolean(res?.active);
      return {
        active: desktopImmersiveActive,
        fullscreen: Boolean(res?.fullscreen),
        mode: 'desktop',
      };
    } catch {
      desktopImmersiveActive = false;
      return { active: false, fullscreen: false, mode: 'desktop' };
    }
  }

  function onImmersiveChange(listener) {
    if (typeof listener !== 'function') return () => {};
    immersiveListeners.add(listener);
    const isDesktop = currentImmersiveCap().mode === 'desktop';
    let onResize = null;
    if (isDesktop) {
      if (tauriEvent()) {
        ensureNativeImmersiveSubscription().catch(() => {});
      } else if (typeof window !== 'undefined' && typeof window.addEventListener === 'function') {
        onResize = () => {
          if (!desktopImmersiveActive) return;
          invoke('window_is_fullscreen')
            .then((fs) => {
              if (desktopImmersiveActive && fs === false) {
                desktopImmersiveActive = false;
                invoke('window_set_immersive', { on: false }).catch(() => {});
                notifyImmersiveListeners(false);
              }
            })
            .catch(() => {});
        };
        window.addEventListener('resize', onResize);
      }
    }
    return () => {
      immersiveListeners.delete(listener);
      if (immersiveListeners.size === 0) {
        teardownNativeImmersiveSubscription();
      }
      if (onResize && typeof window !== 'undefined' && typeof window.removeEventListener === 'function') {
        window.removeEventListener('resize', onResize);
      }
    };
  }

  async function keepScreenOn(on) {
    const active = Boolean(on);
    const cap = currentImmersiveCap();
    if (cap.mode === 'android') {
      const android = androidBridge();
      if (typeof android?.setKeepScreenOn === 'function') {
        try {
          android.setKeepScreenOn(active);
        } catch {
          // Ignore bridge errors.
        }
        return active;
      }
      return false;
    }
    if (cap.mode === 'ios') {
      const ios = iosImmersiveHandler();
      try {
        ios.postMessage({ type: 'keep_screen_on', on: active });
        return active;
      } catch {
        return false;
      }
    }
    if (cap.mode === 'desktop') {
      await invoke('set_keep_screen_on', { on: active }).catch(() => {});
      return active;
    }
    return false;
  }

  const link = {
    kind: 'app',
    supportsContent: true,
    supportsContentFor,
    onContentSupportChange,
    state,
    tools,
    call,
    deviceName,
    pair,
    unpair,
    selectNode,
    learn,
    acceptIdentityChange,
    dismissIdentityChange,
    repairStore,
    invalidateSessions,
    onAppResume,
    scanInvite,
    updateState,
    watchUpdates,
    checkUpdates,
    installUpdate,
    setAutoUpdate,
    dismissUpdateNotice,
    maybeAutoInstall,
    contentUrl,
    contentStatus,
    revokeContentUrl,
    download,
    transfers,
    transferControl,
    openTransfer,
    showTransferInFolder,
    onTransferEvent,
    transferSettings,
    setTransferSettings,
    immersive,
    canImmersive,
    onImmersiveChange,
    onWindowHidden,
    keepScreenOn,
  };

  // On iOS/iPadOS, native file-picker uploads and open-in-another-app are not yet wired; omit
  // those methods so the interface hides unsupported actions instead of showing dead controls.
  if (!iosPlatform) {
    link.pickUploads = pickUploads;
    link.upload = upload;
    link.openExternally = openExternally;
  }

  return link;
}
