// Full-screen file viewer: pictures (pinch/double-tap zoom, pan, swipe neighbours, swipe down),
// machine-made previews for formats the WebView can't decode, video & audio via platform players
// on content addresses, text (128 KiB + load more), extensionless text probing, and in-place
// "Open in another app" staging with progress, cancel, and honest errors.

import { html, useEffect, useRef, useState } from '../vendor/preact-htm.js';
import { Icon } from '../icons.js';
import { plainErrorMessage } from '../lib/errors.js';
import {
  baseFileName,
  canOpenExternally,
  entryCategory,
  entryIconName,
  extensionFromName,
  isExtensionlessName,
  mimeFromName,
  plainCategoryLabel,
  viewerModeFromName,
} from '../lib/files.js';
import { readTextWrapPreference, writeTextWrapPreference } from '../lib/files-store.js';
import { formatBytePair, formatBytes, formatRelativeTime } from '../lib/format.js';
import { CopyButton, Notice, Sheet } from './common.js';

const TEXT_CHUNK_BYTES = 131072; // 128 KiB
const MEDIA_RECOVERY_WINDOW_MS = 10_000;
const PREVIEW_THUMB_SIZE = 512;

function isLocalMediaUrl(url) {
  return typeof url === 'string' && (url.startsWith('data:') || url.startsWith('blob:'));
}

function browserDecodesHeic() {
  if (typeof navigator === 'undefined') return false;
  const ua = String(navigator.userAgent || '');
  const isAppleWebKit =
    (/iPad|iPhone|iPod|Macintosh/i.test(ua) &&
      /AppleWebKit/i.test(ua) &&
      !/Chrome|Chromium|Edg|Firefox|CriOS|FxiOS/i.test(ua)) ||
    (navigator.platform === 'MacIntel' && Number(navigator.maxTouchPoints || 0) > 1);
  return isAppleWebKit;
}

function contentTypeMatchesMedia(kind, contentType) {
  if (typeof contentType !== 'string' || !contentType.trim()) return false;
  const base = contentType.split(';')[0].trim().toLowerCase();
  if (kind === 'image') return base.startsWith('image/');
  if (kind === 'audio') return base.startsWith('audio/') || base === 'application/ogg';
  if (kind === 'video') return base.startsWith('video/') || base === 'application/ogg';
  return false;
}

function classifyMintError(err) {
  if (err?.details?.reason === 'too_large_for_inline' || err?.status === 413) {
    return {
      code: 'too_large',
      status: err?.status || 413,
      message: err?.message || '',
      hint: err?.hint || '',
      details: err?.details || null,
      machine: err?.machine || null,
    };
  }
  if (err?.code === 'unsupported' || err?.code === 'needs_newer_legion') {
    return {
      code: err.code,
      message: err.message || '',
      hint: err.hint || '',
      details: err.details || null,
      machine: err.machine || null,
    };
  }
  if (err?.code === 'denied') {
    return {
      code: 'denied',
      message: err.message || "You're not allowed to open this file.",
      hint: err.hint || '',
      details: err.details || null,
      machine: err.machine || null,
    };
  }
  if (err?.code === 'not_found') {
    return {
      code: 'not_found',
      message: err.message || '',
      hint: err.hint || '',
      details: err.details || null,
      machine: err.machine || null,
    };
  }
  return {
    code: 'unreachable',
    message: err?.message || '',
    hint: err?.hint || '',
    details: err?.details || null,
    machine: err?.machine || null,
  };
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

function makeIdleOpenState(key) {
  return {
    key,
    status: 'idle', // 'idle' | 'starting' | 'downloading' | 'paused' | 'error'
    transferId: null,
    bytesDone: 0,
    bytesTotal: 0,
    pausedReason: null,
    error: null,
  };
}

export function FilesViewer({
  machine,
  path,
  entry,
  siblings = [],
  link,
  capsStore = null,
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
  const rawCategory = entryCategory(effectiveEntry);
  const ext = extensionFromName(name);
  const tableMode = viewerModeFromName(name);
  const mime = effectiveEntry.mime || mimeFromName(name);
  const fileKey = `${machine || ''}\0${path || ''}`;
  const hasContentUrl = Boolean(caps?.hasContentUrl);
  const allowExternalOpen = Boolean(caps?.hasOpenExternally && canOpenExternally(effectiveEntry));

  const [chromeHidden, setChromeHidden] = useState(false);
  const [moreOpen, setMoreOpen] = useState(false);
  const [wrapLines, setWrapLines] = useState(() => readTextWrapPreference(safeStorage()));
  const [retryNonce, setRetryNonce] = useState(0);
  const [binaryTsVideo, setBinaryTsVideo] = useState(() => ({ key: fileKey, active: false }));
  const [mediaState, setMediaState] = useState(() => ({
    key: fileKey,
    url: null,
    loading: false,
    error: null,
    isFreshAddress: false,
    isMachinePreview: false,
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
  const [openState, setOpenState] = useState(() => makeIdleOpenState(fileKey));
  const [viewerNotice, setViewerNotice] = useState(null);

  const isBinaryTs = binaryTsVideo.key === fileKey && binaryTsVideo.active;
  const isExtensionlessProbe = rawCategory === 'other' && isExtensionlessName(name) && Boolean(caps?.hasRead);
  const category = isBinaryTs ? 'video' : rawCategory;
  const skipMediaPlayer =
    !isBinaryTs && (category === 'video' || category === 'audio') && tableMode === 'fallback';
  const preferThumbPreview =
    category === 'image' &&
    tableMode === 'thumb_or_fallback' &&
    !((ext === 'heic' || ext === 'heif') && browserDecodesHeic());

  // Key media, text, hash, and external-open state synchronously to the active file so switching
  // a mounted viewer never flashes a previous file's address, progress, or error for even one frame.
  const activeMedia =
    mediaState.key === fileKey
      ? mediaState
      : {
          key: fileKey,
          url: null,
          loading: false,
          error: null,
          isFreshAddress: false,
          isMachinePreview: false,
        };
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
  const activeOpen = openState.key === fileKey ? openState : makeIdleOpenState(fileKey);

  const imgRef = useRef(null);
  const videoRef = useRef(null);
  const audioRef = useRef(null);
  const abortRef = useRef(null);
  const noticeTimerRef = useRef(null);
  const activeFileKeyRef = useRef(fileKey);
  activeFileKeyRef.current = fileKey;
  const activeMediaUrlRef = useRef(mediaUrl);
  activeMediaUrlRef.current = mediaUrl;
  const mintedUrlRef = useRef(null);
  const mediaStateRef = useRef(activeMedia);
  mediaStateRef.current = activeMedia;
  const openStateRef = useRef(activeOpen);
  openStateRef.current = activeOpen;
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

  function showViewerNotice(text, tone = 'neutral') {
    if (noticeTimerRef.current) {
      window.clearTimeout(noticeTimerRef.current);
      noticeTimerRef.current = null;
    }
    setViewerNotice({ key: fileKey, text, tone });
    if (tone !== 'error') {
      noticeTimerRef.current = window.setTimeout(() => {
        noticeTimerRef.current = null;
        setViewerNotice((prev) => (prev && prev.text === text ? null : prev));
      }, 2800);
    }
  }

  function revokeContentUrlNow(url) {
    if (!url || typeof link?.revokeContentUrl !== 'function') return;
    if (mintedUrlRef.current === url) {
      mintedUrlRef.current = null;
    }
    try {
      const res = link.revokeContentUrl(url);
      if (res && typeof res.catch === 'function') res.catch(() => {});
    } catch {
      // Best-effort ticket cleanup when closing or replacing a viewer address.
    }
  }

  function revokeActiveMintedUrl() {
    const url = mintedUrlRef.current;
    if (url) {
      mintedUrlRef.current = null;
      revokeContentUrlNow(url);
    }
  }

  useEffect(() => {
    return () => {
      if (noticeTimerRef.current) {
        window.clearTimeout(noticeTimerRef.current);
        noticeTimerRef.current = null;
      }
      revokeActiveMintedUrl();
    };
  }, []);

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
    revokeActiveMintedUrl();
    if (onClose) onClose();
  }

  function syncOpenStateFromTransfers(list) {
    if (!Array.isArray(list)) return;
    const cur = openStateRef.current;
    if (cur.key !== fileKey) return;

    let match = null;
    if (cur.transferId) {
      match = list.find((t) => t && t.id === cur.transferId) || null;
    } else if (cur.status === 'idle') {
      match =
        list.find(
          (t) =>
            t &&
            t.openExternally &&
            t.direction === 'download' &&
            t.machine === machine &&
            t.path === path &&
            (t.state === 'queued' || t.state === 'running'),
        ) || null;
    }
    if (!match) return;

    const total = match.bytes_total || cur.bytesTotal || effectiveEntry.size || 0;
    const done = match.bytes_done || 0;

    if (match.state === 'queued' || match.state === 'running') {
      setOpenState({
        key: fileKey,
        status: 'downloading',
        transferId: match.id,
        bytesDone: done,
        bytesTotal: total,
        pausedReason: null,
        error: null,
      });
    } else if (match.state === 'paused') {
      setOpenState({
        key: fileKey,
        status: 'paused',
        transferId: match.id,
        bytesDone: done,
        bytesTotal: total,
        pausedReason: match.pausedReason || 'user',
        error: match.error || null,
      });
    } else if (match.state === 'failed') {
      const errObj =
        typeof match.error === 'string'
          ? { code: 'failed', message: match.error }
          : match.error || { code: 'failed', message: "Couldn't get this file ready." };
      setOpenState({
        key: fileKey,
        status: 'error',
        transferId: match.id,
        bytesDone: done,
        bytesTotal: total,
        pausedReason: null,
        error: errObj,
      });
    } else if (match.state === 'cancelling' || match.state === 'cancelled') {
      setOpenState(makeIdleOpenState(fileKey));
    } else if (match.state === 'done') {
      if (match.openError) {
        setOpenState({
          key: fileKey,
          status: 'error',
          transferId: match.id,
          bytesDone: total,
          bytesTotal: total,
          pausedReason: null,
          error: match.openError,
        });
      } else {
        setOpenState(makeIdleOpenState(fileKey));
      }
    }
  }

  // Subscribe to live device transfer updates while the viewer is mounted so in-place
  // "Open in another app" progress, pauses, cancellations, and post-download open errors update
  // immediately inside `.viewer`.
  useEffect(() => {
    setOpenState(makeIdleOpenState(fileKey));
    setViewerNotice(null);
    if (!allowExternalOpen) return undefined;

    let alive = true;
    const unsub =
      typeof link?.onTransferEvent === 'function'
        ? link.onTransferEvent((list) => {
            if (alive) syncOpenStateFromTransfers(list);
          })
        : () => {};

    if (typeof link?.transfers === 'function') {
      link
        .transfers()
        .then((list) => {
          if (alive) syncOpenStateFromTransfers(list);
        })
        .catch(() => {});
    }

    return () => {
      alive = false;
      if (typeof unsub === 'function') unsub();
    };
  }, [fileKey, allowExternalOpen, link]);

  async function handleOpenExternally() {
    if (!allowExternalOpen) return;
    const targetKey = fileKey;
    setViewerNotice(null);
    setOpenState({
      key: targetKey,
      status: 'starting',
      transferId: null,
      bytesDone: 0,
      bytesTotal: effectiveEntry.size || 0,
      pausedReason: null,
      error: null,
    });

    try {
      const invokeOpen =
        typeof onOpenExternally === 'function'
          ? () => onOpenExternally(effectiveEntry)
          : typeof link?.openExternally === 'function'
            ? () =>
                link.openExternally({
                  machine,
                  path,
                  name,
                  size: effectiveEntry.size,
                  mime,
                })
            : null;
      if (!invokeOpen) {
        setOpenState({
          key: targetKey,
          status: 'error',
          transferId: null,
          bytesDone: 0,
          bytesTotal: effectiveEntry.size || 0,
          pausedReason: null,
          error: {
            code: 'unsupported',
            message: `Opening files in another app isn't available on this ${deviceNoun}.`,
          },
        });
        return;
      }

      const res = await invokeOpen();
      if (activeFileKeyRef.current !== targetKey) return;

      if (res?.openedImmediately) {
        setOpenState(makeIdleOpenState(targetKey));
        return;
      }

      if (res?.id) {
        setOpenState((prev) =>
          prev.key === targetKey
            ? {
                ...prev,
                status: prev.status === 'starting' ? 'downloading' : prev.status,
                transferId: res.id,
              }
            : prev,
        );
        if (typeof link?.transfers === 'function') {
          link
            .transfers()
            .then((list) => {
              if (activeFileKeyRef.current === targetKey) {
                syncOpenStateFromTransfers(list);
              }
            })
            .catch(() => {});
        }
        return;
      }

      // Simple mock/caller that resolved without a queued transfer id.
      setOpenState(makeIdleOpenState(targetKey));
    } catch (err) {
      if (activeFileKeyRef.current !== targetKey) return;
      const errObj =
        err && typeof err === 'object' && typeof err.message === 'string'
          ? {
              code: err.code || 'failed',
              message: err.message,
              hint: err.hint || null,
              reason: err.reason || err.details?.reason || null,
            }
          : {
              code: 'failed',
              message: typeof err === 'string' ? err : "Couldn't open this file in another app.",
              hint: null,
              reason: null,
            };
      setOpenState({
        key: targetKey,
        status: 'error',
        transferId: null,
        bytesDone: 0,
        bytesTotal: effectiveEntry.size || 0,
        pausedReason: null,
        error: errObj,
      });
    }
  }

  async function handleCancelOpen() {
    const id = activeOpen.transferId;
    setOpenState(makeIdleOpenState(fileKey));
    if (id && typeof link?.transferControl === 'function') {
      try {
        await link.transferControl(id, 'cancel');
      } catch {
        // Ignore cancel race if transfer already finished.
      }
    }
  }

  async function handleResumeOpen() {
    const id = activeOpen.transferId;
    if (!id || typeof link?.transferControl !== 'function') return;
    try {
      const updated = await link.transferControl(id, 'resume');
      if (activeFileKeyRef.current === fileKey && updated) {
        syncOpenStateFromTransfers([updated]);
      }
    } catch (err) {
      if (activeFileKeyRef.current === fileKey) {
        setOpenState((prev) => ({
          ...prev,
          status: 'error',
          error: {
            code: err?.code || 'failed',
            message: err?.message || "Couldn't resume getting this file ready.",
            hint: err?.hint || null,
          },
        }));
      }
    }
  }

  function handleDownloadClick() {
    if (!caps?.hasDownload || !onDownload) return;
    setOpenState((prev) => (prev.status === 'error' ? makeIdleOpenState(fileKey) : prev));
    const result = onDownload(effectiveEntry);
    showViewerNotice(`Downloading ${name}. Watch progress in Activity.`);
    if (result && typeof result.catch === 'function') {
      result.catch((err) => {
        if (activeFileKeyRef.current === fileKey) {
          showViewerNotice(
            plainErrorMessage(err, "Couldn't start download.", { technical, deviceNoun }),
            'error',
          );
        }
      });
    }
  }

  async function fetchMachinePreview(targetKey, signal) {
    if (!caps?.hasThumb || (!capsStore && typeof link?.call !== 'function')) return false;
    try {
      const thumbArgs = { paths: [path], size: PREVIEW_THUMB_SIZE, fit: 'contain' };
      const res = capsStore
        ? (await capsStore.call('fs.thumb', thumbArgs, { on: machine, signal })).value
        : await link.call('fs.thumb', thumbArgs, { on: machine, signal });
      if (signal?.aborted || activeFileKeyRef.current !== targetKey) return true;
      const t = res?.thumbnails?.[path];
      if (t?.content && t?.mime) {
        setMediaState({
          key: targetKey,
          url: `data:${t.mime};base64,${t.content}`,
          loading: false,
          error: null,
          isFreshAddress: false,
          isMachinePreview: true,
        });
        return true;
      }
    } catch {
      if (signal?.aborted || activeFileKeyRef.current !== targetKey) return true;
    }
    return false;
  }

  // If content support flips from unsupported to supported while this viewer is sitting on an
  // unsupported fallback (for example after capabilities refresh), reload automatically.
  useEffect(() => {
    const prev = prevHasContentRef.current;
    prevHasContentRef.current = hasContentUrl;
    const errCode = mediaStateRef.current.error?.code;
    if (
      !prev &&
      hasContentUrl &&
      mediaStateRef.current.key === fileKey &&
      (errCode === 'unsupported' || errCode === 'needs_newer_legion')
    ) {
      setRetryNonce((n) => n + 1);
    }
  }, [hasContentUrl, fileKey]);

  // Load media URL, machine preview, or text content whenever machine/path/mode changes.
  useEffect(() => {
    if (abortRef.current) abortRef.current.abort();
    const ctrl = new AbortController();
    abortRef.current = ctrl;
    const targetKey = fileKey;

    resetZoom();
    playbackRef.current = { key: targetKey, time: 0, wantPlay: false };
    pendingResumeRef.current = null;
    recoveryRef.current = { key: targetKey, lastAttemptAt: 0, inFlight: false };
    setMediaState({
      key: targetKey,
      url: null,
      loading: false,
      error: null,
      isFreshAddress: false,
      isMachinePreview: false,
    });
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

    const runCall = (tool, args, opts = {}) =>
      capsStore
        ? capsStore.call(tool, args, { ...opts, on: machine }).then((r) => r.value)
        : link.call(tool, args, { ...opts, on: machine });

    if (caps && caps.known === false) {
      if (category === 'text' || isExtensionlessProbe) {
        setTextState((prev) => ({ ...prev, key: targetKey, loading: true, error: null }));
      } else if (category === 'image' || category === 'video' || category === 'audio') {
        setMediaState({
          key: targetKey,
          url: null,
          loading: true,
          error: null,
          isFreshAddress: false,
          isMachinePreview: false,
        });
      }
      return () => {
        ctrl.abort();
        stopMediaElements();
        revokeActiveMintedUrl();
      };
    }

    if (category === 'text' || isExtensionlessProbe) {
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
      runCall('fs.read', { path, offset: 0, length: TEXT_CHUNK_BYTES }, { signal: ctrl.signal })
        .then((res) => {
          if (ctrl.signal.aborted || activeFileKeyRef.current !== targetKey) return;
          if (res?.encoding === 'base64') {
            if (ext === 'ts' && !isBinaryTs) {
              setBinaryTsVideo({ key: targetKey, active: true });
              return;
            }
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
          if (isExtensionlessProbe) {
            setTextState({
              key: targetKey,
              content: '',
              size: effectiveEntry.size || 0,
              loaded: 0,
              eof: true,
              binary: true,
              loading: false,
              error: null,
            });
          } else {
            setTextState((prev) => ({ ...prev, key: targetKey, loading: false, error: err }));
          }
        });
    } else if (skipMediaPlayer) {
      // Known-unplayable formats (e.g. WMA, MIDI, AVI, WMV, FLV, RM) skip mounting a player
      // so we never waste bandwidth before showing the honest fallback card.
    } else if (preferThumbPreview) {
      if (caps?.hasThumb) {
        setMediaState({
          key: targetKey,
          url: null,
          loading: true,
          error: null,
          isFreshAddress: false,
          isMachinePreview: false,
        });
        fetchMachinePreview(targetKey, ctrl.signal).then((ok) => {
          if (!ok && !ctrl.signal.aborted && activeFileKeyRef.current === targetKey) {
            setMediaState({
              key: targetKey,
              url: null,
              loading: false,
              error: { code: 'decode', message: '' },
              isFreshAddress: false,
              isMachinePreview: false,
            });
          }
        });
      } else {
        setMediaState({
          key: targetKey,
          url: null,
          loading: false,
          error: { code: 'decode', message: '' },
          isFreshAddress: false,
          isMachinePreview: false,
        });
      }
    } else if (category === 'image' || category === 'video' || category === 'audio') {
      if (caps?.hasContentUrl && typeof link?.contentUrl === 'function') {
        setMediaState({
          key: targetKey,
          url: null,
          loading: true,
          error: null,
          isFreshAddress: false,
          isMachinePreview: false,
        });
        link
          .contentUrl({ machine, path, signal: ctrl.signal })
          .then((res) => {
            if (ctrl.signal.aborted || activeFileKeyRef.current !== targetKey) {
              if (res?.url) revokeContentUrlNow(res.url);
              return;
            }
            if (!res?.url) {
              setMediaState({
                key: targetKey,
                url: null,
                loading: false,
                error: { code: 'unreachable', message: 'Preview is not available right now.' },
                isFreshAddress: false,
                isMachinePreview: false,
              });
              return;
            }
            revokeActiveMintedUrl();
            mintedUrlRef.current = res.url;
            setMediaState({
              key: targetKey,
              url: res.url,
              loading: false,
              error: null,
              isFreshAddress: false,
              isMachinePreview: false,
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
              isMachinePreview: false,
            });
          });
      } else if (category === 'image' && caps?.hasThumb) {
        setMediaState({
          key: targetKey,
          url: null,
          loading: true,
          error: null,
          isFreshAddress: false,
          isMachinePreview: false,
        });
        fetchMachinePreview(targetKey, ctrl.signal).then((ok) => {
          if (!ok && !ctrl.signal.aborted && activeFileKeyRef.current === targetKey) {
            setMediaState({
              key: targetKey,
              url: null,
              loading: false,
              error: { code: 'unsupported', message: 'Preview is not available.' },
              isFreshAddress: false,
              isMachinePreview: false,
            });
          }
        });
      } else if (
        category === 'image' &&
        caps?.hasRead &&
        effectiveEntry.size != null &&
        effectiveEntry.size > 0 &&
        effectiveEntry.size <= 1048576
      ) {
        // Inline fallback for small pictures on older machines without content addresses or thumbnails.
        setMediaState({
          key: targetKey,
          url: null,
          loading: true,
          error: null,
          isFreshAddress: false,
          isMachinePreview: false,
        });
        runCall('fs.read', { path, offset: 0, length: 1048576, encoding: 'base64' }, { signal: ctrl.signal })
          .then((res) => {
            if (ctrl.signal.aborted || activeFileKeyRef.current !== targetKey) return;
            if (res?.eof && res?.content) {
              setMediaState({
                key: targetKey,
                url: `data:${mime};base64,${res.content}`,
                loading: false,
                error: null,
                isFreshAddress: false,
                isMachinePreview: false,
              });
            } else {
              setMediaState({
                key: targetKey,
                url: null,
                loading: false,
                error: { code: 'unsupported', message: 'Preview is not available on this machine yet.' },
                isFreshAddress: false,
                isMachinePreview: false,
              });
            }
          })
          .catch((err) => {
            if (ctrl.signal.aborted || activeFileKeyRef.current !== targetKey) return;
            setMediaState({
              key: targetKey,
              url: null,
              loading: false,
              error: err,
              isFreshAddress: false,
              isMachinePreview: false,
            });
          });
      } else {
        setMediaState({
          key: targetKey,
          url: null,
          loading: false,
          error: { code: 'unsupported', message: '' },
          isFreshAddress: false,
          isMachinePreview: false,
        });
      }
    }

    return () => {
      ctrl.abort();
      stopMediaElements();
      revokeActiveMintedUrl();
    };
  }, [
    machine,
    path,
    category,
    isExtensionlessProbe,
    skipMediaPlayer,
    preferThumbPreview,
    retryNonce,
    caps?.known,
    caps?.hasContentUrl,
    caps?.hasThumb,
    caps?.hasRead,
    capsStore,
  ]);

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
    const hashReq = capsStore
      ? capsStore.call('fs.hash', { path }, { on: machine }).then((r) => r.value)
      : link.call('fs.hash', { path }, { on: machine });
    hashReq
      .then((res) => {
        if (alive && res?.hash && activeFileKeyRef.current === targetKey) {
          setHashInfo({ key: targetKey, value: res.hash });
        }
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [moreOpen, technical, caps?.hasHash, capsStore, machine, path, fileKey, activeHash]);

  async function loadMoreText() {
    if (activeText.loading || activeText.eof) return;
    const targetKey = fileKey;
    setTextState((prev) => ({ ...prev, key: targetKey, loading: true, error: null }));
    try {
      const readOpts = { on: machine, signal: abortRef.current?.signal };
      const res = capsStore
        ? (await capsStore.call('fs.read', { path, offset: activeText.loaded, length: TEXT_CHUNK_BYTES }, readOpts)).value
        : await link.call('fs.read', { path, offset: activeText.loaded, length: TEXT_CHUNK_BYTES }, readOpts);
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

  async function inspectFailedMediaUrl(kind, expectedUrl, el, signal) {
    const isMediaNetworkErr = kind !== 'image' && el?.error?.code === 2;

    if (typeof link?.contentStatus === 'function') {
      const st = await link.contentStatus(expectedUrl);
      if (signal?.aborted) return null;
      if (!st || typeof st !== 'object') {
        return { confirmedDecode: false, code: 'unreachable', retryable: true, message: '' };
      }
      if (st.status === 413 || st.error_code === 'too_large_for_inline') {
        return {
          confirmedDecode: false,
          code: 'too_large',
          retryable: false,
          status: st.status || 413,
          errorCode: st.error_code || null,
          message: st.error_message || '',
        };
      }
      if (st.status === 403) {
        return {
          confirmedDecode: false,
          code: 'denied',
          retryable: false,
          status: 403,
          errorCode: st.error_code || null,
          message: st.error_message || '',
        };
      }
      if (st.status === 404 || st.status === 410 || st.ticket_live === false) {
        return {
          confirmedDecode: false,
          code: 'not_found',
          retryable: true,
          status: st.status || null,
          errorCode: st.error_code || null,
          message: st.error_message || '',
        };
      }
      if (
        st.stream_failed ||
        st.request_count === 0 ||
        st.status === 429 ||
        (typeof st.status === 'number' && st.status >= 500)
      ) {
        return {
          confirmedDecode: false,
          code: 'unreachable',
          retryable: true,
          status: st.status || null,
          errorCode: st.stream_failed ? 'stream_failed' : st.error_code || null,
          message: st.error_message || '',
        };
      }
      if (
        (st.status === 200 || st.status === 206) &&
        !st.stream_failed &&
        contentTypeMatchesMedia(kind, st.content_type)
      ) {
        if (isMediaNetworkErr) {
          return { confirmedDecode: false, code: 'unreachable', retryable: true, errorCode: 'media_err_network', message: '' };
        }
        return { confirmedDecode: true, code: 'decode', retryable: false, message: '' };
      }
      return {
        confirmedDecode: false,
        code: 'unreachable',
        retryable: true,
        status: st.status || null,
        errorCode: st.error_code || null,
        message: st.error_message || '',
      };
    }

    if (isLocalMediaUrl(expectedUrl)) {
      return { confirmedDecode: true, code: 'decode', retryable: false, message: '' };
    }

    // Browser HTTP link: inspect the content URL with a HEAD request.
    try {
      const probe = await fetch(expectedUrl, { method: 'HEAD', signal });
      if (signal?.aborted) return null;
      if (probe.status === 413) {
        return { confirmedDecode: false, code: 'too_large', retryable: false, status: 413, message: '' };
      }
      if (probe.status === 403) {
        return { confirmedDecode: false, code: 'denied', retryable: false, status: 403, message: '' };
      }
      if (probe.status === 404 || probe.status === 410) {
        return { confirmedDecode: false, code: 'not_found', retryable: true, status: probe.status, message: '' };
      }
      if (probe.status === 429 || probe.status >= 500) {
        return { confirmedDecode: false, code: 'unreachable', retryable: true, status: probe.status, message: '' };
      }
      if (probe.ok && contentTypeMatchesMedia(kind, probe.headers.get('content-type'))) {
        if (isMediaNetworkErr) {
          return { confirmedDecode: false, code: 'unreachable', retryable: true, errorCode: 'media_err_network', message: '' };
        }
        return { confirmedDecode: true, code: 'decode', retryable: false, message: '' };
      }
      return { confirmedDecode: false, code: 'unreachable', retryable: true, status: probe.status, message: '' };
    } catch {
      if (signal?.aborted) return null;
      return { confirmedDecode: false, code: 'unreachable', retryable: true, errorCode: 'network_error', message: '' };
    }
  }

  async function handleMediaElementError(kind, expectedKey, expectedUrl, el) {
    if (activeFileKeyRef.current !== expectedKey || activeMediaUrlRef.current !== expectedUrl) {
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

    const inspection = await inspectFailedMediaUrl(kind, expectedUrl, el, signal);
    if (!inspection || activeFileKeyRef.current !== expectedKey || activeMediaUrlRef.current !== expectedUrl) {
      return;
    }

    // On a first failure outside the 10-second loop guard, silently request ONE fresh content
    // address for non-fatal responses (never retry 413/403 or local blob:/data: URLs) so
    // expired tickets, restarted doors, and evicted streams self-heal transparently.
    if (
      !isLocalMediaUrl(expectedUrl) &&
      inspection.code !== 'too_large' &&
      inspection.code !== 'denied' &&
      !recentlyTried &&
      canRefresh
    ) {
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
        if (signal?.aborted || activeFileKeyRef.current !== expectedKey) {
          if (res?.url) revokeContentUrlNow(res.url);
          return;
        }
        recoveryRef.current.inFlight = false;
        if (!res?.url) {
          revokeContentUrlNow(expectedUrl);
          setMediaState({
            key: expectedKey,
            url: null,
            loading: false,
            error: { code: 'unreachable', message: '' },
            isFreshAddress: false,
            isMachinePreview: false,
          });
          return;
        }
        if (expectedUrl && expectedUrl !== res.url) {
          revokeContentUrlNow(expectedUrl);
        }
        mintedUrlRef.current = res.url;
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
          isMachinePreview: false,
        });
      } catch (err) {
        if (err?.code === 'cancelled' || signal?.aborted || activeFileKeyRef.current !== expectedKey) {
          return;
        }
        recoveryRef.current.inFlight = false;
        revokeContentUrlNow(expectedUrl);
        setMediaState({
          key: expectedKey,
          url: null,
          loading: false,
          error: classifyMintError(err),
          isFreshAddress: false,
          isMachinePreview: false,
        });
      }
      return;
    }

    // Confirmed picture decode failure: try a machine-made thumbnail preview before giving up.
    if (
      inspection.confirmedDecode &&
      kind === 'image' &&
      caps?.hasThumb &&
      !mediaStateRef.current.isMachinePreview
    ) {
      const ok = await fetchMachinePreview(expectedKey, signal);
      if (ok) {
        revokeContentUrlNow(expectedUrl);
        return;
      }
      if (signal?.aborted || activeFileKeyRef.current !== expectedKey) return;
    }

    if (activeFileKeyRef.current !== expectedKey || activeMediaUrlRef.current !== expectedUrl) {
      return;
    }
    revokeContentUrlNow(expectedUrl);
    setMediaState({
      key: expectedKey,
      url: null,
      loading: false,
      error: {
        code: inspection.code,
        message: inspection.message || '',
        status: inspection.status || null,
        errorCode: inspection.errorCode || null,
      },
      isFreshAddress: false,
      isMachinePreview: false,
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

  function openProgressLabel() {
    if (activeOpen.status === 'paused') {
      return activeOpen.pausedReason === 'wifi_only' ? 'Paused until Wi-Fi.' : 'Paused.';
    }
    if (activeOpen.bytesTotal > 0 && activeOpen.bytesDone > 0) {
      return `Getting it ready… ${formatBytePair(activeOpen.bytesDone, activeOpen.bytesTotal)}`;
    }
    if (activeOpen.bytesTotal > 0) {
      return `Getting it ready… 0 of ${formatBytes(activeOpen.bytesTotal)}`;
    }
    return 'Getting it ready…';
  }

  function openProgressPct() {
    if (activeOpen.bytesTotal > 0 && activeOpen.bytesDone > 0) {
      return Math.max(2, Math.min(100, Math.round((activeOpen.bytesDone / activeOpen.bytesTotal) * 100)));
    }
    return null;
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
  } else if ((category === 'video' || category === 'audio') && !mediaError && !skipMediaPlayer && caps?.hasContentUrl) {
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

  // Track whether the current stage is showing a fallback card (so open-externally progress/error
  // renders right inside the card instead of duplicating into the bottom banner).
  const isShowingText =
    (category === 'text' || isExtensionlessProbe) && !activeText.error && !activeText.binary;
  const isShowingInlineMedia =
    !skipMediaPlayer &&
    (category === 'image' || category === 'video' || category === 'audio') &&
    !mediaError &&
    Boolean(mediaUrl);
  const stageIsFallbackCard = !isShowingText && !isShowingInlineMedia;

  function renderFallbackCard(heading, body, { showRetry = false, hideActions = false, hint = '' } = {}) {
    if (activeOpen.status === 'starting' || activeOpen.status === 'downloading' || activeOpen.status === 'paused') {
      const pct = openProgressPct();
      return html`
        <div class="said">
          <div class="viewer-said-icon" aria-hidden="true">
            <${Icon} name=${entryIconName(effectiveEntry)} />
          </div>
          <strong>${name}</strong>
          <span>${openProgressLabel()}</span>
          <div class="viewer-open-progress">
            <span class=${pct == null ? 'viewer-open-track indeterminate' : 'viewer-open-track'}>
              <i style=${pct == null ? '' : `--v:${pct}`}></i>
            </span>
            <div class="viewer-said-actions">
              ${activeOpen.status === 'paused'
                ? html`
                    <button type="button" class="btn primary" onClick=${handleResumeOpen}>
                      Resume now
                    </button>
                  `
                : null}
              <button type="button" class="btn" onClick=${handleCancelOpen}>Cancel</button>
            </div>
          </div>
        </div>
      `;
    }

    if (activeOpen.status === 'error') {
      const err = activeOpen.error || {};
      const isNoHandler = err.reason === 'no_handler' || err.code === 'unsupported';
      const isDenied = err.code === 'denied';
      const errHeading = isNoHandler
        ? `No app on this ${deviceNoun} can open this file`
        : "Couldn't open in another app";
      const defaultErrBody = isNoHandler
        ? `No app on this ${deviceNoun} handles this file type.`
        : 'Try again, or download the file instead.';
      const errBody = plainErrorMessage(err, defaultErrBody, { technical, deviceNoun });
      const techMeta = technical
        ? [err.hint, err.code ? `code: ${err.code}` : '', err.reason ? `reason: ${err.reason}` : '']
            .filter(Boolean)
            .join(' · ')
        : '';
      const showDownload = Boolean(caps?.hasDownload);
      const showTryAgain = allowExternalOpen && !isNoHandler && !isDenied;
      return html`
        <div class="said">
          <div class="viewer-said-icon" aria-hidden="true">
            <${Icon} name=${entryIconName(effectiveEntry)} />
          </div>
          <strong>${errHeading}</strong>
          <span>${errBody}</span>
          ${techMeta ? html`<span class="viewer-open-hint">${techMeta}</span>` : null}
          <div class="viewer-said-actions">
            ${showTryAgain
              ? html`
                  <button type="button" class="btn primary" onClick=${handleOpenExternally}>
                    Try again
                  </button>
                `
              : null}
            ${showDownload
              ? html`
                  <button
                    type="button"
                    class=${showTryAgain ? 'btn' : 'btn primary'}
                    onClick=${handleDownloadClick}
                  >
                    Download to this ${deviceNoun}
                  </button>
                `
              : null}
            <button
              type="button"
              class="btn"
              onClick=${() => setOpenState(makeIdleOpenState(fileKey))}
            >
              Back
            </button>
          </div>
        </div>
      `;
    }

    const showExternal = !hideActions && !showRetry && allowExternalOpen;
    const showDownload = !hideActions && Boolean(caps?.hasDownload);
    return html`
      <div class="said">
        <div class="viewer-said-icon" aria-hidden="true">
          <${Icon} name=${entryIconName(effectiveEntry)} />
        </div>
        <strong>${heading}</strong>
        <span>${body}</span>
        ${hint ? html`<span class="viewer-open-hint">${hint}</span>` : null}
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
                      <button type="button" class="btn primary" onClick=${handleOpenExternally}>
                        Open in another app
                      </button>
                    `
                  : null}
                ${showDownload
                  ? html`
                      <button
                        type="button"
                        class=${showRetry || showExternal ? 'btn' : 'btn primary'}
                        onClick=${handleDownloadClick}
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

  function technicalErrorDetail(err) {
    if (!technical || !err) return '';
    return [
      err.status ? `HTTP ${err.status}` : '',
      err.errorCode || err.details?.reason || (err.code && err.code !== 'decode' ? err.code : ''),
      err.hint || '',
    ]
      .filter(Boolean)
      .join(' · ');
  }

  function renderCommonMediaError(err, kindLabel) {
    const techHint = technicalErrorDetail(err);
    if (err.code === 'unreachable') {
      return renderFallbackCard(
        "Couldn't reach this file",
        plainErrorMessage(
          err,
          `Couldn't load this file from ${machine}. Check that it's on and try again.`,
          { technical, deviceNoun },
        ),
        { showRetry: true, hint: techHint },
      );
    }
    if (err.code === 'not_found') {
      return renderFallbackCard(
        "Couldn't reach this file",
        plainErrorMessage(
          err,
          `${machine} couldn't serve this file, or it may have moved.`,
          { technical, deviceNoun },
        ),
        { showRetry: true, hint: techHint },
      );
    }
    if (err.code === 'too_large') {
      return renderFallbackCard(
        'This file is too large to open inline',
        allowExternalOpen
          ? `Open it in another app, or download it to this ${deviceNoun}.`
          : `Download it to this ${deviceNoun} to open it.`,
        { hint: techHint },
      );
    }
    if (err.code === 'denied') {
      return renderFallbackCard(
        "Can't open this file",
        plainErrorMessage(err, "You're not allowed to open this file.", { technical, deviceNoun }),
        { hideActions: true, hint: techHint },
      );
    }
    if (err.code === 'unsupported' || err.code === 'needs_newer_legion') {
      const heading =
        kindLabel === 'picture'
          ? "Can't show this picture here"
          : kindLabel === 'video'
            ? `Video playback isn't available from ${machine} yet`
            : `Audio playback isn't available from ${machine} yet`;
      const body =
        kindLabel === 'picture'
          ? `This version of Legion on ${machine} doesn't serve picture previews yet.`
          : allowExternalOpen
            ? 'Open it in another app that can, or download it first.'
            : caps?.hasDownload
              ? `Download it to this ${deviceNoun} to play it.`
              : `${machine} needs a newer Legion to stream ${kindLabel} here.`;
      return renderFallbackCard(heading, body, { hint: techHint });
    }
    return null;
  }

  function renderStage() {
    if (category === 'image') {
      if (mediaError) {
        const common = renderCommonMediaError(mediaError, 'picture');
        if (common) return common;
        if (mediaError.code !== 'decode') {
          return renderFallbackCard(
            "Couldn't reach this file",
            plainErrorMessage(
              mediaError,
              `Couldn't load this file from ${machine}. Check that it's on and try again.`,
              { technical, deviceNoun },
            ),
            { showRetry: true, hint: technicalErrorDetail(mediaError) },
          );
        }
        const msg =
          allowExternalOpen || caps?.hasDownload
            ? allowExternalOpen
              ? 'Open it in another app that can, or download it first.'
              : `Download it to this ${deviceNoun} to view it.`
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
        ${activeMedia.isMachinePreview
          ? html`
              <div class="viewer-preview-note">
                ${allowExternalOpen ? 'Preview. Open in another app for the original.' : 'Preview.'}
              </div>
            `
          : null}
      `;
    }

    if (category === 'video') {
      if (skipMediaPlayer) {
        const fmt = ext ? `${ext.toUpperCase()} ` : '';
        return renderFallbackCard(
          `This ${deviceNoun} can't play ${fmt}video directly`,
          allowExternalOpen
            ? 'Open it in another app that can, or download it first.'
            : `Download it to this ${deviceNoun} to open it.`,
        );
      }
      if (mediaError) {
        const common = renderCommonMediaError(mediaError, 'video');
        if (common) return common;
        if (mediaError.code !== 'decode') {
          return renderFallbackCard(
            "Couldn't reach this file",
            plainErrorMessage(
              mediaError,
              `Couldn't load this file from ${machine}. Check that it's on and try again.`,
              { technical, deviceNoun },
            ),
            { showRetry: true, hint: technicalErrorDetail(mediaError) },
          );
        }
        const body = allowExternalOpen
          ? 'Open it in another app that can, or download it first.'
          : caps?.hasDownload
            ? `Download it to this ${deviceNoun} to play it.`
            : `This ${deviceNoun} can't play this video format.`;
        return renderFallbackCard(`This ${deviceNoun} can't play this kind of video`, body);
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
      if (skipMediaPlayer) {
        const fmt = ext ? `${ext.toUpperCase()} ` : '';
        return renderFallbackCard(
          `This ${deviceNoun} can't play ${fmt}audio directly`,
          allowExternalOpen
            ? 'Open it in another app that can, or download it first.'
            : `Download it to this ${deviceNoun} to open it.`,
        );
      }
      if (mediaError) {
        const common = renderCommonMediaError(mediaError, 'audio');
        if (common) return common;
        if (mediaError.code !== 'decode') {
          return renderFallbackCard(
            "Couldn't reach this file",
            plainErrorMessage(
              mediaError,
              `Couldn't load this file from ${machine}. Check that it's on and try again.`,
              { technical, deviceNoun },
            ),
            { showRetry: true, hint: technicalErrorDetail(mediaError) },
          );
        }
        const body = allowExternalOpen
          ? 'Open it in another app that can, or download it first.'
          : caps?.hasDownload
            ? `Download it to this ${deviceNoun} to play it.`
            : `This ${deviceNoun} can't play this audio format.`;
        return renderFallbackCard(`This ${deviceNoun} can't play this audio file`, body);
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

    if (category === 'text' || isExtensionlessProbe) {
      if (activeText.error) {
        return html`<div class="said"><${Notice} error=${activeText.error} technical=${technical} deviceNoun=${deviceNoun} /></div>`;
      }
      if (activeText.binary) {
        if (isExtensionlessProbe) {
          const otherMeta = [
            plainCategoryLabel(effectiveEntry),
            effectiveEntry.size != null ? formatBytes(effectiveEntry.size) : null,
          ]
            .filter(Boolean)
            .join(' · ');
          return renderFallbackCard(name, otherMeta || `File on ${machine}`);
        }
        return renderFallbackCard(
          "Can't show this file as text",
          allowExternalOpen
            ? 'Open it in another app or download it first.'
            : caps?.hasDownload
              ? `Download it to this ${deviceNoun} to inspect it.`
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

  function renderOpenBanner() {
    if (stageIsFallbackCard) {
      if (viewerNotice && viewerNotice.key === fileKey) {
        return html`
          <div class=${viewerNotice.tone === 'error' ? 'viewer-open-banner error' : 'viewer-open-banner'} role="status">
            <div class="viewer-open-banner-row">
              <div class="viewer-open-banner-copy">${viewerNotice.text}</div>
              <button
                type="button"
                class="btn small"
                onClick=${() => setViewerNotice(null)}
              >
                Dismiss
              </button>
            </div>
          </div>
        `;
      }
      return null;
    }

    if (activeOpen.status === 'starting' || activeOpen.status === 'downloading' || activeOpen.status === 'paused') {
      const pct = openProgressPct();
      return html`
        <div class="viewer-open-banner" role="status">
          <div class="viewer-open-banner-row">
            <div class="viewer-open-banner-copy">${openProgressLabel()}</div>
            <div class="viewer-open-banner-actions">
              ${activeOpen.status === 'paused'
                ? html`
                    <button type="button" class="btn small primary" onClick=${handleResumeOpen}>
                      Resume now
                    </button>
                  `
                : null}
              <button type="button" class="btn small" onClick=${handleCancelOpen}>Cancel</button>
            </div>
          </div>
          <span class=${pct == null ? 'viewer-open-track indeterminate' : 'viewer-open-track'}>
            <i style=${pct == null ? '' : `--v:${pct}`}></i>
          </span>
        </div>
      `;
    }

    if (activeOpen.status === 'error') {
      const err = activeOpen.error || {};
      const isNoHandler = err.reason === 'no_handler' || err.code === 'unsupported';
      const errMsg = plainErrorMessage(
        err,
        isNoHandler
          ? `No app on this ${deviceNoun} can open this file type.`
          : "Couldn't open this file in another app.",
        { technical, deviceNoun },
      );
      const techMeta = technical
        ? [err.hint, err.code ? `code: ${err.code}` : '', err.reason ? `reason: ${err.reason}` : '']
            .filter(Boolean)
            .join(' · ')
        : '';
      return html`
        <div class="viewer-open-banner error" role="alert">
          <div class="viewer-open-banner-row">
            <div class="viewer-open-banner-copy">
              ${errMsg}
              ${techMeta ? html`<small>${techMeta}</small>` : null}
            </div>
            <div class="viewer-open-banner-actions">
              ${caps?.hasDownload
                ? html`
                    <button
                      type="button"
                      class=${isNoHandler ? 'btn small primary' : 'btn small'}
                      onClick=${handleDownloadClick}
                    >
                      Download
                    </button>
                  `
                : null}
              <button
                type="button"
                class="btn small"
                onClick=${() => setOpenState(makeIdleOpenState(fileKey))}
              >
                Dismiss
              </button>
            </div>
          </div>
        </div>
      `;
    }

    if (viewerNotice && viewerNotice.key === fileKey) {
      return html`
        <div class=${viewerNotice.tone === 'error' ? 'viewer-open-banner error' : 'viewer-open-banner'} role="status">
          <div class="viewer-open-banner-row">
            <div class="viewer-open-banner-copy">${viewerNotice.text}</div>
            <button type="button" class="btn small" onClick=${() => setViewerNotice(null)}>
              Dismiss
            </button>
          </div>
        </div>
      `;
    }
    return null;
  }

  const effectiveChromeHidden =
    chromeHidden && activeOpen.status === 'idle' && !(viewerNotice && viewerNotice.key === fileKey);
  const viewerCls = ['viewer', effectiveChromeHidden ? 'chrome-hidden' : ''].filter(Boolean).join(' ');
  const stageCls = isShowingText ? 'viewer-stage scrollable' : 'viewer-stage';
  const openBusy = activeOpen.status === 'starting' || activeOpen.status === 'downloading';

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
        ${renderOpenBanner()}
        <div class="bar4">
          ${caps?.hasDownload
            ? html`
                <button type="button" onClick=${handleDownloadClick}>
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
          ${allowExternalOpen
            ? html`
                <button type="button" disabled=${openBusy} onClick=${handleOpenExternally}>
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
                        <span class="body"><span class="title" style="color:var(--bad)">Delete</span></span>
                      </button>
                    `
                  : null}
              </div>

              ${technical
                ? html`
                    <div class="sheet-cta">
                      <${CopyButton} text=${`${machine}:${path}`} label="Copy path" />
                    </div>
                  `
                : null}
            </${Sheet}>
          `
        : null}
    </div>
  `;
}
