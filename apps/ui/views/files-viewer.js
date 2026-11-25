// Full-screen file viewer: pictures (pinch/double-tap zoom, pan, swipe neighbours, swipe down),
// video & audio via platform players on content addresses, text (128 KiB + load more), and fallback.

import { html, useEffect, useRef, useState } from '../vendor/preact-htm.js';
import { Icon } from '../icons.js';
import {
  baseFileName,
  entryCategory,
  entryIconName,
  mimeFromName,
  plainCategoryLabel,
} from '../lib/files.js';
import { readTextWrapPreference, writeTextWrapPreference } from '../lib/files-store.js';
import { formatBytes, formatRelativeTime } from '../lib/format.js';
import { CopyButton, Notice, Sheet } from './common.js';

const TEXT_CHUNK_BYTES = 131072; // 128 KiB
const MEDIA_RECOVERY_WINDOW_MS = 10_000;

function isLocalMediaUrl(url) {
  return typeof url === 'string' && (url.startsWith('data:') || url.startsWith('blob:'));
}

function classifyMintError(err) {
  if (err?.code === 'unsupported') {
    return { code: 'unsupported', message: err.message || '' };
  }
  if (err?.code === 'denied') {
    return { code: 'denied', message: err.message || "You're not allowed to open this file." };
  }
  return { code: 'unreachable', message: err?.message || '' };
}

function safeStorage() {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

function clamp(v, min, max) {
  return Math.max(min, Math.min(max, v));
}

function pointerDist(a, b) {
  const dx = a.x - b.x;
  const dy = a.y - b.y;
  return Math.hypot(dx, dy);
}

export function FilesViewer({
  machine,
  path,
  entry,
  siblings = [],
  link,
  caps,
  deviceNoun = 'phone',
  technical = false,
  isStarred = false,
  isPinned = false,
  onClose,
  onNavigateSibling,
  onToggleStar,
  onTogglePin,
  onSendTo,
  onDownload,
  onOpenExternally,
  onRename,
  onDelete,
  onToast,
}) {
  const name = entry?.name || baseFileName(path);
  const effectiveEntry = entry || { name, path, kind: 'file' };
  const category = entryCategory(effectiveEntry);
  const mime = effectiveEntry.mime || mimeFromName(name);
  const fileKey = `${machine || ''}\0${path || ''}`;
  const hasContentUrl = Boolean(caps?.hasContentUrl);

  const [chromeHidden, setChromeHidden] = useState(false);
  const [moreOpen, setMoreOpen] = useState(false);
  const [wrapLines, setWrapLines] = useState(() => readTextWrapPreference(safeStorage()));
  const [retryNonce, setRetryNonce] = useState(0);
  const [mediaState, setMediaState] = useState(() => ({
    key: fileKey,
    url: null,
    loading: false,
    error: null,
    isFreshAddress: false,
  }));
  const [textState, setTextState] = useState(() => ({
    key: fileKey,
    content: '',
    size: 0,
    loaded: 0,
    eof: true,
    binary: false,
    loading: false,
    error: null,
  }));
  const [hashInfo, setHashInfo] = useState(() => ({ key: fileKey, value: null }));

  // Key media, text, and hash state synchronously to the active file so switching a mounted
  // viewer (for example from a video to a picture) never renders a previous file's address or
  // error for even one frame.
  const activeMedia =
    mediaState.key === fileKey
      ? mediaState
      : { key: fileKey, url: null, loading: false, error: null, isFreshAddress: false };
  const mediaUrl = activeMedia.url;
  const mediaLoading = activeMedia.loading;
  const mediaError = activeMedia.error;

  const activeText =
    textState.key === fileKey
      ? textState
      : {
          key: fileKey,
          content: '',
          size: 0,
          loaded: 0,
          eof: true,
          binary: false,
          loading: false,
          error: null,
        };
  const activeHash = hashInfo.key === fileKey ? hashInfo.value : null;

  const imgRef = useRef(null);
  const videoRef = useRef(null);
  const audioRef = useRef(null);
  const abortRef = useRef(null);
  const activeFileKeyRef = useRef(fileKey);
  activeFileKeyRef.current = fileKey;
  const activeMediaUrlRef = useRef(mediaUrl);
  activeMediaUrlRef.current = mediaUrl;
  const mediaStateRef = useRef(activeMedia);
  mediaStateRef.current = activeMedia;
  const playbackRef = useRef({ key: fileKey, time: 0, wantPlay: false });
  const pendingResumeRef = useRef(null);
  const recoveryRef = useRef({ key: fileKey, lastAttemptAt: 0, inFlight: false });
  const prevHasContentRef = useRef(hasContentUrl);

  // Gesture state for picture zoom/pan/swipe.
  const gestureRef = useRef({
    scale: 1,
    tx: 0,
    ty: 0,
    pointers: new Map(),
    startX: 0,
    startY: 0,
    startTx: 0,
    startTy: 0,
    startTime: 0,
    moved: false,
    pinchStartDist: 0,
    pinchStartScale: 1,
    lastTapAt: 0,
    tapTimer: null,
  });

  const pictureSiblings = siblings.filter((s) => s && s.kind !== 'dir' && entryCategory(s) === 'image');
  const siblingIndex = pictureSiblings.findIndex((s) => s.path === path);

  function applyImgTransform() {
    const el = imgRef.current;
    if (!el) return;
    const g = gestureRef.current;
    if (g.scale <= 1.01 && Math.abs(g.tx) < 0.5 && Math.abs(g.ty) < 0.5) {
      el.style.transform = '';
    } else {
      el.style.transform = `translate3d(${g.tx}px, ${g.ty}px, 0) scale(${g.scale})`;
    }
  }

  function resetZoom() {
    const g = gestureRef.current;
    g.scale = 1;
    g.tx = 0;
    g.ty = 0;
    applyImgTransform();
  }

  function stopMediaElements() {
    for (const ref of [videoRef, audioRef]) {
      const el = ref.current;
      if (el) {
        try {
          el.pause();
          el.removeAttribute('src');
          el.load();
        } catch {
          // Ignore media teardown errors.
        }
      }
    }
  }

  function handleClose() {
    if (abortRef.current) {
      abortRef.current.abort();
      abortRef.current = null;
    }
    stopMediaElements();
    if (onClose) onClose();
  }

  // If content support flips from unsupported to supported while this viewer is sitting on an
  // unsupported fallback (for example after capabilities refresh), reload automatically.
  useEffect(() => {
    const prev = prevHasContentRef.current;
    prevHasContentRef.current = hasContentUrl;
    if (
      !prev &&
      hasContentUrl &&
      mediaStateRef.current.key === fileKey &&
      mediaStateRef.current.error?.code === 'unsupported'
    ) {
      setRetryNonce((n) => n + 1);
    }
  }, [hasContentUrl, fileKey]);

  // Load media URL or text content whenever machine/path changes; abort immediately on close/switch.
  useEffect(() => {
    if (abortRef.current) abortRef.current.abort();
    const ctrl = new AbortController();
    abortRef.current = ctrl;
    const targetKey = fileKey;

    resetZoom();
    playbackRef.current = { key: targetKey, time: 0, wantPlay: false };
    pendingResumeRef.current = null;
    recoveryRef.current = { key: targetKey, lastAttemptAt: 0, inFlight: false };
    setMediaState({ key: targetKey, url: null, loading: false, error: null, isFreshAddress: false });
    setHashInfo({ key: targetKey, value: null });
    setTextState({
      key: targetKey,
      content: '',
      size: 0,
      loaded: 0,
      eof: true,
      binary: false,
      loading: false,
      error: null,
    });

    if (category === 'text') {
      setTextState((prev) =>
        prev.key === targetKey
          ? { ...prev, loading: true, error: null }
          : {
              key: targetKey,
              content: '',
              size: 0,
              loaded: 0,
              eof: true,
              binary: false,
              loading: true,
              error: null,
            },
      );
      link
        .call('fs.read', { path, offset: 0, length: TEXT_CHUNK_BYTES }, { on: machine, signal: ctrl.signal })
        .then((res) => {
          if (ctrl.signal.aborted || activeFileKeyRef.current !== targetKey) return;
          if (res?.encoding === 'base64') {
            setTextState({
              key: targetKey,
              content: '',
              size: res.size || 0,
              loaded: 0,
              eof: true,
              binary: true,
              loading: false,
              error: null,
            });
          } else {
            const content = String(res?.content ?? '');
            setTextState({
              key: targetKey,
              content,
              size: res?.size ?? content.length,
              loaded: (res?.offset || 0) + (res?.length ?? content.length),
              eof: Boolean(res?.eof),
              binary: false,
              loading: false,
              error: null,
            });
          }
        })
        .catch((err) => {
          if (ctrl.signal.aborted || activeFileKeyRef.current !== targetKey) return;
          setTextState((prev) => ({ ...prev, key: targetKey, loading: false, error: err }));
        });
    } else if (category === 'image' || category === 'video' || category === 'audio') {
      if (caps?.hasContentUrl && typeof link?.contentUrl === 'function') {
        setMediaState({ key: targetKey, url: null, loading: true, error: null, isFreshAddress: false });
        link
          .contentUrl({ machine, path, signal: ctrl.signal })
          .then((res) => {
            if (ctrl.signal.aborted || activeFileKeyRef.current !== targetKey) return;
            if (!res?.url) {
              setMediaState({
                key: targetKey,
                url: null,
                loading: false,
                error: { code: 'unreachable', message: 'Preview is not available.' },
                isFreshAddress: false,
              });
              return;
            }
            setMediaState({
              key: targetKey,
              url: res.url,
              loading: false,
              error: null,
              isFreshAddress: false,
            });
          })
          .catch((err) => {
            if (ctrl.signal.aborted || activeFileKeyRef.current !== targetKey) return;
            setMediaState({
              key: targetKey,
              url: null,
              loading: false,
              error: classifyMintError(err),
              isFreshAddress: false,
            });
          });
      } else if (category === 'image' && caps?.hasThumb) {
        setMediaState({ key: targetKey, url: null, loading: true, error: null, isFreshAddress: false });
        link
          .call('fs.thumb', { paths: [path], size: 512, fit: 'contain' }, { on: machine, signal: ctrl.signal })
          .then((res) => {
            if (ctrl.signal.aborted || activeFileKeyRef.current !== targetKey) return;
            const t = res?.thumbnails?.[path];
            if (t?.content && t?.mime) {
              setMediaState({
                key: targetKey,
                url: `data:${t.mime};base64,${t.content}`,
                loading: false,
                error: null,
                isFreshAddress: false,
              });
            } else {
              setMediaState({
                key: targetKey,
                url: null,
                loading: false,
                error: { code: 'unsupported', message: 'Preview is not available.' },
                isFreshAddress: false,
              });
            }
          })
          .catch((err) => {
            if (ctrl.signal.aborted || activeFileKeyRef.current !== targetKey) return;
            setMediaState({
              key: targetKey,
              url: null,
              loading: false,
              error: classifyMintError(err),
              isFreshAddress: false,
            });
          });
      } else {
        setMediaState({
          key: targetKey,
          url: null,
          loading: false,
          error: { code: 'unsupported', message: 'Preview is not available on this machine yet.' },
          isFreshAddress: false,
        });
      }
    }

    return () => {
      ctrl.abort();
      stopMediaElements();
    };
  }, [machine, path, category, retryNonce]);

  // Keyboard navigation inside viewer: Escape closes, Left/Right steps through pictures.
  useEffect(() => {
    function onKeyDown(e) {
      if (moreOpen || document.querySelector('.scrim')) return;
      if (e.key === 'Escape') {
        e.preventDefault();
        handleClose();
      } else if (category === 'image' && e.key === 'ArrowRight' && siblingIndex >= 0 && siblingIndex + 1 < pictureSiblings.length) {
        e.preventDefault();
        if (onNavigateSibling) onNavigateSibling(pictureSiblings[siblingIndex + 1]);
      } else if (category === 'image' && e.key === 'ArrowLeft' && siblingIndex > 0) {
        e.preventDefault();
        if (onNavigateSibling) onNavigateSibling(pictureSiblings[siblingIndex - 1]);
      }
    }
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [moreOpen, category, siblingIndex, pictureSiblings.length]);

  // Load BLAKE3 hash when details sheet opens in technical mode.
  useEffect(() => {
    if (!moreOpen || !technical || !caps?.hasHash || activeHash) return;
    let alive = true;
    const targetKey = fileKey;
    link
      .call('fs.hash', { path }, { on: machine })
      .then((res) => {
        if (alive && res?.hash && activeFileKeyRef.current === targetKey) {
          setHashInfo({ key: targetKey, value: res.hash });
        }
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [moreOpen, technical, caps?.hasHash, machine, path, fileKey, activeHash]);

  async function loadMoreText() {
    if (activeText.loading || activeText.eof) return;
    const targetKey = fileKey;
    setTextState((prev) => ({ ...prev, key: targetKey, loading: true, error: null }));
    try {
      const res = await link.call(
        'fs.read',
        { path, offset: activeText.loaded, length: TEXT_CHUNK_BYTES },
        { on: machine, signal: abortRef.current?.signal },
      );
      if (activeFileKeyRef.current !== targetKey) return;
      const chunk = String(res?.content ?? '');
      setTextState((prev) => ({
        ...prev,
        key: targetKey,
        content: prev.content + chunk,
        size: res?.size ?? prev.size,
        loaded: (res?.offset ?? prev.loaded) + (res?.length ?? chunk.length),
        eof: Boolean(res?.eof),
        loading: false,
      }));
    } catch (err) {
      if (err?.code !== 'cancelled' && activeFileKeyRef.current === targetKey) {
        setTextState((prev) => ({ ...prev, key: targetKey, loading: false, error: err }));
      }
    }
  }

  function handleRetryMedia() {
    recoveryRef.current = { key: fileKey, lastAttemptAt: 0, inFlight: false };
    pendingResumeRef.current = null;
    setRetryNonce((n) => n + 1);
  }

  function noteMediaTime(expectedKey, el) {
    if (!el || activeFileKeyRef.current !== expectedKey) return;
    if (pendingResumeRef.current && pendingResumeRef.current.key === expectedKey) return;
    if (typeof el.currentTime === 'number' && Number.isFinite(el.currentTime) && !el.error) {
      if (playbackRef.current.key !== expectedKey) {
        playbackRef.current = { key: expectedKey, time: el.currentTime, wantPlay: !el.paused };
      } else {
        playbackRef.current.time = el.currentTime;
      }
    }
  }

  function noteMediaPlay(expectedKey, el) {
    if (!el || activeFileKeyRef.current !== expectedKey) return;
    if (playbackRef.current.key !== expectedKey) {
      playbackRef.current = { key: expectedKey, time: el.currentTime || 0, wantPlay: true };
    } else {
      playbackRef.current.wantPlay = true;
      if (!pendingResumeRef.current && Number.isFinite(el.currentTime) && el.currentTime > 0) {
        playbackRef.current.time = el.currentTime;
      }
    }
  }

  function noteMediaPause(expectedKey, el) {
    if (!el || activeFileKeyRef.current !== expectedKey) return;
    // Ignore pause events triggered as part of an error teardown or source swap.
    if (el.error || pendingResumeRef.current?.key === expectedKey) return;
    if (playbackRef.current.key === expectedKey) {
      playbackRef.current.wantPlay = false;
      if (!el.ended && Number.isFinite(el.currentTime)) {
        playbackRef.current.time = el.currentTime;
      }
    }
  }

  function applyPendingResume(expectedKey, expectedUrl, el) {
    const resume = pendingResumeRef.current;
    if (!resume || resume.key !== expectedKey || resume.url !== expectedUrl || !el) return;
    pendingResumeRef.current = null;
    const duration = Number.isFinite(el.duration) && el.duration > 0 ? el.duration : null;
    const targetTime =
      resume.time > 0
        ? duration !== null
          ? Math.min(resume.time, Math.max(0, duration - 0.05))
          : resume.time
        : 0;
    playbackRef.current = { key: expectedKey, time: targetTime, wantPlay: Boolean(resume.play) };
    if (targetTime > 0 && Math.abs(el.currentTime - targetTime) > 0.05) {
      try {
        el.currentTime = targetTime;
      } catch {
        // Ignore seek failures on unseekable streams.
      }
    }
    if (resume.play) {
      const p = el.play();
      if (p && typeof p.catch === 'function') p.catch(() => {});
    }
  }

  async function handleMediaElementError(kind, expectedKey, expectedUrl, el) {
    if (activeFileKeyRef.current !== expectedKey || activeMediaUrlRef.current !== expectedUrl) {
      return;
    }

    if (isLocalMediaUrl(expectedUrl)) {
      setMediaState({
        key: expectedKey,
        url: null,
        loading: false,
        error: { code: 'decode' },
        isFreshAddress: false,
      });
      return;
    }

    const now = Date.now();
    const rec = recoveryRef.current;
    const sameFile = rec.key === expectedKey;
    if (sameFile && rec.inFlight) return;
    const recentlyTried =
      sameFile && rec.lastAttemptAt > 0 && now - rec.lastAttemptAt < MEDIA_RECOVERY_WINDOW_MS;
    const canRefresh = Boolean(caps?.hasContentUrl && typeof link?.contentUrl === 'function');
    const signal = abortRef.current?.signal;

    // On a first failure outside the 10-second loop guard, silently request ONE fresh content
    // address, swap `src`, seek back to where playback stopped, and resume if it was playing.
    if (!recentlyTried && canRefresh) {
      const savedTime =
        el && typeof el.currentTime === 'number' && Number.isFinite(el.currentTime) && el.currentTime > 0
          ? el.currentTime
          : playbackRef.current.key === expectedKey
            ? playbackRef.current.time
            : 0;
      const savedPlay =
        kind !== 'image' &&
        Boolean(
          (playbackRef.current.key === expectedKey && playbackRef.current.wantPlay) ||
            (el && !el.paused && !el.ended),
        );

      recoveryRef.current = { key: expectedKey, lastAttemptAt: now, inFlight: true };
      try {
        const res = await link.contentUrl({ machine, path, signal });
        if (signal?.aborted || activeFileKeyRef.current !== expectedKey) return;
        recoveryRef.current.inFlight = false;
        if (!res?.url) {
          setMediaState({
            key: expectedKey,
            url: null,
            loading: false,
            error: { code: 'unreachable' },
            isFreshAddress: false,
          });
          return;
        }
        if (kind !== 'image') {
          pendingResumeRef.current = {
            key: expectedKey,
            url: res.url,
            time: savedTime,
            play: savedPlay,
          };
        }
        setMediaState({
          key: expectedKey,
          url: res.url,
          loading: false,
          error: null,
          isFreshAddress: true,
        });
      } catch (err) {
        if (err?.code === 'cancelled' || signal?.aborted || activeFileKeyRef.current !== expectedKey) {
          return;
        }
        recoveryRef.current.inFlight = false;
        setMediaState({
          key: expectedKey,
          url: null,
          loading: false,
          error: classifyMintError(err),
          isFreshAddress: false,
        });
      }
      return;
    }

    // Second failure within 10 seconds (or no refresh capability): never loop.
    // On the native app link, connect-src restricts fetch() to Tauri IPC so a cross-origin HEAD
    // probe to the local content mount would always fail under CSP; contentUrl already probed the
    // file via open_content(0, 0) over SSH/local when minting the fresh address, so a second media
    // element error on that fresh address is a decode error rather than unreachable.
    let code = 'unreachable';
    if (link?.kind === 'app') {
      if (mediaStateRef.current.key === expectedKey && mediaStateRef.current.isFreshAddress) {
        code = 'decode';
      }
    } else {
      try {
        const probe = await fetch(expectedUrl, { method: 'HEAD', signal });
        if (signal?.aborted || activeFileKeyRef.current !== expectedKey) return;
        if (probe.ok) {
          code = 'decode';
        }
      } catch {
        if (signal?.aborted || activeFileKeyRef.current !== expectedKey) return;
        code = 'unreachable';
      }
    }
    if (activeFileKeyRef.current !== expectedKey || activeMediaUrlRef.current !== expectedUrl) {
      return;
    }
    setMediaState({
      key: expectedKey,
      url: null,
      loading: false,
      error: { code },
      isFreshAddress: false,
    });
  }

  // Pointer handlers for picture stage: pinch zoom, double-tap zoom, pan, swipe neighbours, swipe down close, tap chrome.
  function onStagePointerDown(e) {
    if (category !== 'image' || e.button > 0) return;
    const g = gestureRef.current;
    g.pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });
    if (g.pointers.size === 1) {
      g.startX = e.clientX;
      g.startY = e.clientY;
      g.startTx = g.tx;
      g.startTy = g.ty;
      g.startTime = Date.now();
      g.moved = false;
    } else if (g.pointers.size === 2) {
      const pts = Array.from(g.pointers.values());
      g.pinchStartDist = Math.max(1, pointerDist(pts[0], pts[1]));
      g.pinchStartScale = g.scale;
      g.moved = true;
    }
  }

  function onStagePointerMove(e) {
    if (category !== 'image') return;
    const g = gestureRef.current;
    if (!g.pointers.has(e.pointerId)) return;
    g.pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });

    if (g.pointers.size === 2) {
      const pts = Array.from(g.pointers.values());
      const dist = pointerDist(pts[0], pts[1]);
      g.scale = clamp(g.pinchStartScale * (dist / g.pinchStartDist), 1, 4);
      if (g.scale <= 1.02) {
        g.scale = 1;
        g.tx = 0;
        g.ty = 0;
      }
      g.moved = true;
      applyImgTransform();
      return;
    }

    if (g.pointers.size === 1) {
      const dx = e.clientX - g.startX;
      const dy = e.clientY - g.startY;
      if (Math.hypot(dx, dy) > 8) g.moved = true;
      if (g.scale > 1.02) {
        g.tx = g.startTx + dx;
        g.ty = g.startTy + dy;
        applyImgTransform();
      } else if (g.moved && Math.abs(dy) > Math.abs(dx) && dy > 0) {
        g.ty = dy * 0.6;
        applyImgTransform();
      } else if (g.moved && Math.abs(dx) > Math.abs(dy)) {
        g.tx = dx * 0.45;
        applyImgTransform();
      }
    }
  }

  function onStagePointerUp(e) {
    if (category !== 'image') return;
    const g = gestureRef.current;
    const wasSingle = g.pointers.size === 1;
    g.pointers.delete(e.pointerId);
    if (!wasSingle) return;

    const dx = e.clientX - g.startX;
    const dy = e.clientY - g.startY;
    const dt = Math.max(1, Date.now() - g.startTime);

    if (!g.moved && dt < 300) {
      const now = Date.now();
      if (now - g.lastTapAt < 280) {
        // Double tap toggles zoom.
        g.lastTapAt = 0;
        if (g.tapTimer) {
          clearTimeout(g.tapTimer);
          g.tapTimer = null;
        }
        if (g.scale > 1.05) {
          resetZoom();
        } else {
          g.scale = 2.4;
          g.tx = 0;
          g.ty = 0;
          applyImgTransform();
        }
      } else {
        g.lastTapAt = now;
        g.tapTimer = window.setTimeout(() => {
          g.tapTimer = null;
          setChromeHidden((v) => !v);
        }, 290);
      }
      return;
    }

    if (g.scale <= 1.02) {
      // Swipe down closes viewer.
      if (dy > 85 && dy > Math.abs(dx) * 1.4) {
        resetZoom();
        handleClose();
        return;
      }
      // Swipe sideways navigates to previous / next picture in folder order.
      if (Math.abs(dx) > 54 && Math.abs(dx) > Math.abs(dy) * 1.4) {
        resetZoom();
        if (dx < 0 && siblingIndex >= 0 && siblingIndex + 1 < pictureSiblings.length) {
          if (onNavigateSibling) onNavigateSibling(pictureSiblings[siblingIndex + 1]);
          return;
        }
        if (dx > 0 && siblingIndex > 0) {
          if (onNavigateSibling) onNavigateSibling(pictureSiblings[siblingIndex - 1]);
          return;
        }
      }
      resetZoom();
    }
  }

  function onStagePointerCancel(e) {
    gestureRef.current.pointers.delete(e.pointerId);
    if (gestureRef.current.scale <= 1.02) resetZoom();
  }

  // Header subtitle matching `files.html`.
  let topSub = '';
  if (category === 'image' && siblingIndex >= 0 && pictureSiblings.length > 1) {
    const parts = [
      `${siblingIndex + 1} of ${pictureSiblings.length}`,
      effectiveEntry.size != null ? formatBytes(effectiveEntry.size) : null,
      effectiveEntry.mtime ? formatRelativeTime(effectiveEntry.mtime) : null,
    ].filter(Boolean);
    topSub = parts.join(' · ');
  } else if ((category === 'video' || category === 'audio') && !mediaError && caps?.hasContentUrl) {
    const parts = [
      effectiveEntry.size != null ? formatBytes(effectiveEntry.size) : null,
      `playing from ${machine}`,
    ].filter(Boolean);
    topSub = parts.join(' · ');
  } else {
    const parts = [
      plainCategoryLabel(effectiveEntry),
      effectiveEntry.size != null ? formatBytes(effectiveEntry.size) : null,
      effectiveEntry.mtime ? formatRelativeTime(effectiveEntry.mtime) : null,
    ].filter(Boolean);
    topSub = parts.join(' · ');
  }

  function renderFallbackCard(heading, body, { showRetry = false, hideActions = false } = {}) {
    const showExternal = !hideActions && !showRetry && Boolean(caps?.hasOpenExternally);
    const showDownload = !hideActions && !showRetry && Boolean(caps?.hasDownload);
    return html`
      <div class="said">
        <div class="viewer-said-icon" aria-hidden="true">
          <${Icon} name=${entryIconName(effectiveEntry)} />
        </div>
        <strong>${heading}</strong>
        <span>${body}</span>
        ${showRetry || showExternal || showDownload
          ? html`
              <div class="viewer-said-actions">
                ${showRetry
                  ? html`
                      <button type="button" class="btn primary" onClick=${handleRetryMedia}>
                        Try again
                      </button>
                    `
                  : null}
                ${showExternal
                  ? html`
                      <button
                        type="button"
                        class="btn primary"
                        onClick=${() => onOpenExternally && onOpenExternally(effectiveEntry)}
                      >
                        Open in another app
                      </button>
                    `
                  : null}
                ${showDownload
                  ? html`
                      <button
                        type="button"
                        class=${showExternal ? 'btn' : 'btn primary'}
                        onClick=${() => onDownload && onDownload(effectiveEntry)}
                      >
                        Download to this ${deviceNoun}
                      </button>
                    `
                  : null}
              </div>
            `
          : null}
      </div>
    `;
  }

  function renderStage() {
    if (category === 'image') {
      if (mediaError) {
        if (mediaError.code === 'unreachable') {
          return renderFallbackCard(
            "Couldn't reach this file",
            `${machine} may be off, or the file may have moved or changed.`,
            { showRetry: true },
          );
        }
        if (mediaError.code === 'denied') {
          return renderFallbackCard(
            "Can't open this file",
            mediaError.message || "You're not allowed to open this file.",
            { hideActions: true },
          );
        }
        const msg =
          mediaError.code === 'unsupported'
            ? `This version of Legion on ${machine} doesn't serve picture previews yet.`
            : caps?.hasOpenExternally || caps?.hasDownload
              ? 'Open it in another app that can, or download it first.'
              : `This ${deviceNoun} can't show this kind of picture.`;
        return renderFallbackCard("Can't show this picture here", msg);
      }
      if (!mediaUrl) {
        return html`<div class="said"><span>${mediaLoading ? 'Loading picture…' : 'Preparing picture…'}</span></div>`;
      }
      const expectedKey = fileKey;
      const expectedUrl = mediaUrl;
      return html`
        <img
          key=${expectedKey}
          ref=${imgRef}
          class="viewer-img"
          src=${expectedUrl}
          alt=${name}
          onError=${(e) => handleMediaElementError('image', expectedKey, expectedUrl, e.currentTarget)}
        />
      `;
    }

    if (category === 'video') {
      if (mediaError) {
        if (mediaError.code === 'unreachable') {
          return renderFallbackCard(
            "Couldn't reach this file",
            `${machine} may be off, or the file may have moved or changed.`,
            { showRetry: true },
          );
        }
        if (mediaError.code === 'denied') {
          return renderFallbackCard(
            "Can't open this file",
            mediaError.message || "You're not allowed to open this file.",
            { hideActions: true },
          );
        }
        const heading =
          mediaError.code === 'unsupported'
            ? `Video playback isn't available from ${machine} yet`
            : `This ${deviceNoun} can't play this kind of video`;
        const body =
          caps?.hasOpenExternally || caps?.hasDownload
            ? 'Open it in another app that can, or download it first.'
            : 'This machine needs a newer Legion version to stream video here.';
        return renderFallbackCard(heading, body);
      }
      if (!mediaUrl) {
        return html`<div class="said"><span>${mediaLoading ? 'Starting video…' : 'Preparing video…'}</span></div>`;
      }
      const expectedKey = fileKey;
      const expectedUrl = mediaUrl;
      return html`
        <video
          key=${expectedKey}
          ref=${videoRef}
          class="viewer-video"
          src=${expectedUrl}
          controls
          playsinline
          preload="metadata"
          aria-label=${name}
          onLoadedMetadata=${(e) => applyPendingResume(expectedKey, expectedUrl, e.currentTarget)}
          onTimeUpdate=${(e) => noteMediaTime(expectedKey, e.currentTarget)}
          onSeeked=${(e) => noteMediaTime(expectedKey, e.currentTarget)}
          onPlay=${(e) => noteMediaPlay(expectedKey, e.currentTarget)}
          onPlaying=${(e) => noteMediaPlay(expectedKey, e.currentTarget)}
          onPause=${(e) => noteMediaPause(expectedKey, e.currentTarget)}
          onEnded=${(e) => noteMediaPause(expectedKey, e.currentTarget)}
          onError=${(e) => handleMediaElementError('video', expectedKey, expectedUrl, e.currentTarget)}
        />
      `;
    }

    if (category === 'audio') {
      if (mediaError) {
        if (mediaError.code === 'unreachable') {
          return renderFallbackCard(
            "Couldn't reach this file",
            `${machine} may be off, or the file may have moved or changed.`,
            { showRetry: true },
          );
        }
        if (mediaError.code === 'denied') {
          return renderFallbackCard(
            "Can't open this file",
            mediaError.message || "You're not allowed to open this file.",
            { hideActions: true },
          );
        }
        const heading =
          mediaError.code === 'unsupported'
            ? `Audio playback isn't available from ${machine} yet`
            : `This ${deviceNoun} can't play this audio file`;
        const body =
          caps?.hasOpenExternally || caps?.hasDownload
            ? 'Open it in another app that can, or download it first.'
            : 'This machine needs a newer Legion version to stream audio here.';
        return renderFallbackCard(heading, body);
      }
      if (!mediaUrl) {
        return html`<div class="said"><span>${mediaLoading ? 'Starting audio…' : 'Preparing audio…'}</span></div>`;
      }
      const expectedKey = fileKey;
      const expectedUrl = mediaUrl;
      return html`
        <div class="viewer-audio">
          <div class="viewer-audio-art" aria-hidden="true">
            <${Icon} name="music" />
          </div>
          <strong>${name}</strong>
          <audio
            key=${expectedKey}
            ref=${audioRef}
            src=${expectedUrl}
            controls
            preload="metadata"
            aria-label=${name}
            onLoadedMetadata=${(e) => applyPendingResume(expectedKey, expectedUrl, e.currentTarget)}
            onTimeUpdate=${(e) => noteMediaTime(expectedKey, e.currentTarget)}
            onSeeked=${(e) => noteMediaTime(expectedKey, e.currentTarget)}
            onPlay=${(e) => noteMediaPlay(expectedKey, e.currentTarget)}
            onPlaying=${(e) => noteMediaPlay(expectedKey, e.currentTarget)}
            onPause=${(e) => noteMediaPause(expectedKey, e.currentTarget)}
            onEnded=${(e) => noteMediaPause(expectedKey, e.currentTarget)}
            onError=${(e) => handleMediaElementError('audio', expectedKey, expectedUrl, e.currentTarget)}
          />
        </div>
      `;
    }

    if (category === 'text') {
      if (activeText.error) {
        return html`<div class="said"><${Notice} error=${activeText.error} /></div>`;
      }
      if (activeText.binary) {
        return renderFallbackCard(
          "Can't show this file as text",
          caps?.hasOpenExternally || caps?.hasDownload
            ? 'Open it in another app or download it first.'
            : 'This file is not plain text.',
        );
      }
      if (activeText.loading && !activeText.content) {
        return html`<div class="said"><span>Loading text…</span></div>`;
      }
      const toggleWrap = () => {
        setWrapLines((prev) => {
          const next = !prev;
          writeTextWrapPreference(safeStorage(), next);
          return next;
        });
      };
      const showPartial = !activeText.eof || activeText.size > TEXT_CHUNK_BYTES;
      const sizeLabel = showPartial
        ? `Showing ${formatBytes(activeText.loaded)} of ${formatBytes(activeText.size)}`
        : activeText.size > 0
          ? formatBytes(activeText.size)
          : '';
      return html`
        <div>
          <div class="viewer-text-bar">
            <span>${sizeLabel}</span>
            <button
              type="button"
              class="btn small"
              aria-pressed=${wrapLines ? 'true' : 'false'}
              onClick=${toggleWrap}
            >
              Wrap lines
            </button>
          </div>
          <pre class=${wrapLines ? 'viewer-text' : 'viewer-text nowrap'}>${activeText.content || '(Empty file)'}</pre>
          ${!activeText.eof
            ? html`
                <div class="viewer-text-foot">
                  <span>
                    Showing ${formatBytes(activeText.loaded)} of ${formatBytes(activeText.size)}
                  </span>
                  <button
                    type="button"
                    class="btn small"
                    disabled=${activeText.loading}
                    onClick=${loadMoreText}
                  >
                    ${activeText.loading ? 'Loading…' : 'Load more'}
                  </button>
                </div>
              `
            : null}
        </div>
      `;
    }

    const otherMeta = [
      plainCategoryLabel(effectiveEntry),
      effectiveEntry.size != null ? formatBytes(effectiveEntry.size) : null,
    ]
      .filter(Boolean)
      .join(' · ');
    return renderFallbackCard(name, otherMeta || `File on ${machine}`);
  }

  const viewerCls = ['viewer', chromeHidden ? 'chrome-hidden' : ''].filter(Boolean).join(' ');
  const stageCls = category === 'text' ? 'viewer-stage scrollable' : 'viewer-stage';

  return html`
    <div class=${viewerCls} role="dialog" aria-modal="true" aria-label=${name}>
      <div class="viewer-top">
        <button type="button" class="btn icon" aria-label="Close" onClick=${handleClose}>
          <${Icon} name="down" />
        </button>
        <span class="name">
          ${name}
          ${topSub ? html`<small>${topSub}</small>` : null}
        </span>
        <button
          type="button"
          class="btn icon"
          aria-label="More options"
          onClick=${() => setMoreOpen(true)}
        >
          <${Icon} name="more" />
        </button>
      </div>

      <div
        class=${stageCls}
        onPointerDown=${onStagePointerDown}
        onPointerMove=${onStagePointerMove}
        onPointerUp=${onStagePointerUp}
        onPointerCancel=${onStagePointerCancel}
      >
        ${renderStage()}
      </div>

      <div class="viewer-bar">
        <div class="bar4">
          ${caps?.hasDownload
            ? html`
                <button type="button" onClick=${() => onDownload && onDownload(effectiveEntry)}>
                  <${Icon} name="download" />Download
                </button>
              `
            : null}
          ${caps?.hasTransfer || caps?.hasDownload
            ? html`
                <button type="button" onClick=${() => onSendTo && onSendTo([effectiveEntry])}>
                  <${Icon} name="send" />Send to
                </button>
              `
            : null}
          ${caps?.hasFavWrite
            ? html`
                <button
                  type="button"
                  class=${isStarred ? 'active' : ''}
                  aria-pressed=${isStarred ? 'true' : 'false'}
                  onClick=${() => onToggleStar && onToggleStar(effectiveEntry)}
                >
                  <${Icon} name="star" />${isStarred ? 'Starred' : 'Star'}
                </button>
              `
            : null}
          ${caps?.hasOpenExternally
            ? html`
                <button type="button" onClick=${() => onOpenExternally && onOpenExternally(effectiveEntry)}>
                  <${Icon} name="more" />Open in…
                </button>
              `
            : null}
        </div>
      </div>

      ${moreOpen
        ? html`
            <${Sheet} title=${name} onClose=${() => setMoreOpen(false)}>
              <div class="list">
                <dl class=${technical ? 'kv' : 'kv plain'}>
                  <dt>${technical ? 'category' : 'Kind'}</dt>
                  <dd>${technical ? `${category} (${mime})` : plainCategoryLabel(effectiveEntry)}</dd>
                  ${effectiveEntry.size != null
                    ? html`
                        <dt>${technical ? 'size' : 'Size'}</dt>
                        <dd>
                          ${technical
                            ? `${formatBytes(effectiveEntry.size)} (${effectiveEntry.size} bytes)`
                            : formatBytes(effectiveEntry.size)}
                        </dd>
                      `
                    : null}
                  ${effectiveEntry.mtime
                    ? html`
                        <dt>${technical ? 'mtime' : 'Changed'}</dt>
                        <dd>${technical ? effectiveEntry.mtime : formatRelativeTime(effectiveEntry.mtime)}</dd>
                      `
                    : null}
                  <dt>${technical ? 'machine' : 'Machine'}</dt>
                  <dd>${machine}</dd>
                  ${technical
                    ? html`
                        <dt>path</dt>
                        <dd>${path}</dd>
                      `
                    : null}
                  ${technical && activeHash
                    ? html`
                        <dt>hash</dt>
                        <dd>${activeHash}</dd>
                      `
                    : null}
                </dl>
              </div>

              <div class="list spaced">
                ${caps?.hasFavWrite
                  ? html`
                      <button
                        type="button"
                        class="row"
                        onClick=${() => {
                          setMoreOpen(false);
                          if (onTogglePin) onTogglePin(effectiveEntry);
                        }}
                      >
                        <span class="body">
                          <span class="title">${isPinned ? 'Unpin from Files' : 'Pin to Files'}</span>
                          <span class="sub">
                            ${isPinned ? 'Remove from the tiles at the top of Files' : 'Show as a tile at the top of Files'}
                          </span>
                        </span>
                      </button>
                    `
                  : null}
                ${caps?.hasMove && onRename
                  ? html`
                      <button
                        type="button"
                        class="row"
                        onClick=${() => {
                          setMoreOpen(false);
                          onRename(effectiveEntry);
                        }}
                      >
                        <span class="body"><span class="title">Rename</span></span>
                      </button>
                    `
                  : null}
                <div class="row">
                  <span class="body">
                    <span class="title">Copy path</span>
                    ${technical ? html`<span class="sub mono">${path}</span>` : null}
                  </span>
                  <${CopyButton} text=${path} label="Copy" />
                </div>
                ${caps?.hasRemove && onDelete
                  ? html`
                      <button
                        type="button"
                        class="row"
                        onClick=${() => {
                          setMoreOpen(false);
                          onDelete([effectiveEntry]);
                        }}
                      >
                        <span class="body">
                          <span class="title err">Delete</span>
                        </span>
                      </button>
                    `
                  : null}
              </div>
              <div class="actions">
                <button type="button" class="btn full" onClick=${() => setMoreOpen(false)}>Done</button>
              </div>
            <//>
          `
        : null}
    </div>
  `;
}
