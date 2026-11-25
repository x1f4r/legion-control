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

  const [chromeHidden, setChromeHidden] = useState(false);
  const [moreOpen, setMoreOpen] = useState(false);
  const [wrapLines, setWrapLines] = useState(() => readTextWrapPreference(safeStorage()));
  const [mediaUrl, setMediaUrl] = useState(null);
  const [mediaLoading, setMediaLoading] = useState(false);
  const [mediaError, setMediaError] = useState(null);
  const [textState, setTextState] = useState({ content: '', size: 0, loaded: 0, eof: true, binary: false, loading: false, error: null });
  const [hashInfo, setHashInfo] = useState(null);

  const imgRef = useRef(null);
  const videoRef = useRef(null);
  const audioRef = useRef(null);
  const abortRef = useRef(null);

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

  // Load media URL or text content whenever machine/path changes; abort immediately on close/switch.
  useEffect(() => {
    if (abortRef.current) abortRef.current.abort();
    const ctrl = new AbortController();
    abortRef.current = ctrl;

    resetZoom();
    setMediaUrl(null);
    setMediaError(null);
    setMediaLoading(false);
    setHashInfo(null);
    setTextState({ content: '', size: 0, loaded: 0, eof: true, binary: false, loading: false, error: null });

    if (category === 'text') {
      setTextState((prev) => ({ ...prev, loading: true, error: null }));
      link
        .call('fs.read', { path, offset: 0, length: TEXT_CHUNK_BYTES }, { on: machine, signal: ctrl.signal })
        .then((res) => {
          if (ctrl.signal.aborted) return;
          if (res?.encoding === 'base64') {
            setTextState({ content: '', size: res.size || 0, loaded: 0, eof: true, binary: true, loading: false, error: null });
          } else {
            const content = String(res?.content ?? '');
            setTextState({
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
          if (ctrl.signal.aborted) return;
          setTextState((prev) => ({ ...prev, loading: false, error: err }));
        });
    } else if (category === 'image' || category === 'video' || category === 'audio') {
      if (caps?.hasContentUrl && typeof link?.contentUrl === 'function') {
        setMediaLoading(true);
        link
          .contentUrl({ machine, path, signal: ctrl.signal })
          .then((res) => {
            if (ctrl.signal.aborted) return;
            setMediaUrl(res?.url || null);
            setMediaLoading(false);
            if (!res?.url) setMediaError({ code: 'unsupported', message: 'Preview is not available.' });
          })
          .catch((err) => {
            if (ctrl.signal.aborted) return;
            setMediaLoading(false);
            setMediaError(err);
          });
      } else if (category === 'image' && caps?.hasThumb) {
        setMediaLoading(true);
        link
          .call('fs.thumb', { paths: [path], size: 512, fit: 'contain' }, { on: machine, signal: ctrl.signal })
          .then((res) => {
            if (ctrl.signal.aborted) return;
            const t = res?.thumbnails?.[path];
            if (t?.content && t?.mime) {
              setMediaUrl(`data:${t.mime};base64,${t.content}`);
            } else {
              setMediaError({ code: 'unsupported', message: 'Preview is not available.' });
            }
            setMediaLoading(false);
          })
          .catch((err) => {
            if (ctrl.signal.aborted) return;
            setMediaLoading(false);
            setMediaError(err);
          });
      } else {
        setMediaError({ code: 'unsupported', message: 'Preview is not available on this machine yet.' });
      }
    }

    return () => {
      ctrl.abort();
      stopMediaElements();
    };
  }, [machine, path, category]);

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
    if (!moreOpen || !technical || !caps?.hasHash || hashInfo) return;
    let alive = true;
    link
      .call('fs.hash', { path }, { on: machine })
      .then((res) => {
        if (alive && res?.hash) setHashInfo(res.hash);
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [moreOpen, technical, caps?.hasHash, machine, path, hashInfo]);

  async function loadMoreText() {
    if (textState.loading || textState.eof) return;
    setTextState((prev) => ({ ...prev, loading: true, error: null }));
    try {
      const res = await link.call(
        'fs.read',
        { path, offset: textState.loaded, length: TEXT_CHUNK_BYTES },
        { on: machine, signal: abortRef.current?.signal },
      );
      const chunk = String(res?.content ?? '');
      setTextState((prev) => ({
        ...prev,
        content: prev.content + chunk,
        size: res?.size ?? prev.size,
        loaded: (res?.offset ?? prev.loaded) + (res?.length ?? chunk.length),
        eof: Boolean(res?.eof),
        loading: false,
      }));
    } catch (err) {
      if (err?.code !== 'cancelled') {
        setTextState((prev) => ({ ...prev, loading: false, error: err }));
      }
    }
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

  function renderFallbackCard(heading, body) {
    return html`
      <div class="said">
        <div class="viewer-said-icon" aria-hidden="true">
          <${Icon} name=${entryIconName(effectiveEntry)} />
        </div>
        <strong>${heading}</strong>
        <span>${body}</span>
        ${caps?.hasOpenExternally || caps?.hasDownload
          ? html`
              <div class="viewer-said-actions">
                ${caps?.hasOpenExternally
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
                ${caps?.hasDownload
                  ? html`
                      <button
                        type="button"
                        class=${caps?.hasOpenExternally ? 'btn' : 'btn primary'}
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
        const msg =
          mediaError.code === 'unsupported'
            ? `This version of Legion on ${machine} doesn't serve picture previews yet.`
            : mediaError.message || 'Could not load this picture.';
        return renderFallbackCard("Can't show this picture here", msg);
      }
      if (!mediaUrl) {
        return html`<div class="said"><span>${mediaLoading ? 'Loading picture…' : 'Preparing picture…'}</span></div>`;
      }
      return html`
        <img
          ref=${imgRef}
          class="viewer-img"
          src=${mediaUrl}
          alt=${name}
          onError=${() => setMediaError({ code: 'decode', message: 'Could not display this picture.' })}
        />
      `;
    }

    if (category === 'video') {
      if (mediaError) {
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
      return html`
        <video
          ref=${videoRef}
          class="viewer-video"
          src=${mediaUrl}
          controls
          playsinline
          preload="metadata"
          aria-label=${name}
          onError=${() =>
            setMediaError({ code: 'decode', message: `This ${deviceNoun} can't play this kind of video.` })}
        />
      `;
    }

    if (category === 'audio') {
      if (mediaError) {
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
      return html`
        <div class="viewer-audio">
          <div class="viewer-audio-art" aria-hidden="true">
            <${Icon} name="music" />
          </div>
          <strong>${name}</strong>
          <audio
            ref=${audioRef}
            src=${mediaUrl}
            controls
            preload="metadata"
            aria-label=${name}
            onError=${() =>
              setMediaError({ code: 'decode', message: `This ${deviceNoun} can't play this audio file.` })}
          />
        </div>
      `;
    }

    if (category === 'text') {
      if (textState.error) {
        return html`<div class="said"><${Notice} error=${textState.error} /></div>`;
      }
      if (textState.binary) {
        return renderFallbackCard(
          "Can't show this file as text",
          caps?.hasOpenExternally || caps?.hasDownload
            ? 'Open it in another app or download it first.'
            : 'This file is not plain text.',
        );
      }
      if (textState.loading && !textState.content) {
        return html`<div class="said"><span>Loading text…</span></div>`;
      }
      const toggleWrap = () => {
        setWrapLines((prev) => {
          const next = !prev;
          writeTextWrapPreference(safeStorage(), next);
          return next;
        });
      };
      const showPartial = !textState.eof || textState.size > TEXT_CHUNK_BYTES;
      const sizeLabel = showPartial
        ? `Showing ${formatBytes(textState.loaded)} of ${formatBytes(textState.size)}`
        : textState.size > 0
          ? formatBytes(textState.size)
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
          <pre class=${wrapLines ? 'viewer-text' : 'viewer-text nowrap'}>${textState.content || '(Empty file)'}</pre>
          ${!textState.eof
            ? html`
                <div class="viewer-text-foot">
                  <span>
                    Showing ${formatBytes(textState.loaded)} of ${formatBytes(textState.size)}
                  </span>
                  <button
                    type="button"
                    class="btn small"
                    disabled=${textState.loading}
                    onClick=${loadMoreText}
                  >
                    ${textState.loading ? 'Loading…' : 'Load more'}
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
                  ${technical && hashInfo
                    ? html`
                        <dt>hash</dt>
                        <dd>${hashInfo}</dd>
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
