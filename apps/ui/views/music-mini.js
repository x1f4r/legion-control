// Small player bar (56 px tall) sitting directly above the tab bar on phone and at the bottom of
// the main column on desktop whenever a song is loaded and its own screen is not showing.
// Tapping anywhere except the buttons opens the song's screen with a 220 ms cover grow feel;
// swiping it down dismisses it and releases the stream.

import { html, useMemo, useRef } from '../vendor/preact-htm.js';
import { Icon } from '../icons.js';
import { formatMiniPlayerDisplay, MINI_PLAYER_SWIPE_DISMISS_PX } from '../lib/player.js';
import { FileNameSingle } from './file-name.js';

export function MiniPlayer({ musicState, musicController, onOpenScreen }) {
  const barRef = useRef(null);
  const coverRef = useRef(null);
  const dragRef = useRef({
    active: false,
    pointerId: null,
    startX: 0,
    startY: 0,
    startTime: 0,
    dy: 0,
    dragging: false,
    suppressClick: false,
  });

  const display = useMemo(
    () => formatMiniPlayerDisplay(musicState?.tags, musicState?.name),
    [musicState?.tags, musicState?.name],
  );

  if (!musicState?.loaded) return null;

  const duration = Number.isFinite(musicState.duration) && musicState.duration > 0 ? musicState.duration : 0;
  const currentTime =
    musicState.pendingSeekTime != null
      ? musicState.pendingSeekTime
      : Number.isFinite(musicState.currentTime)
        ? musicState.currentTime
        : 0;
  const progress = duration > 0 ? Math.max(0, Math.min(1, currentTime / duration)) : 0;
  const hasNext = Boolean(musicState.queue?.next);
  const busy = Boolean(musicState.stalled || musicState.mediaLoading);

  function resetDragVisual() {
    const el = barRef.current;
    if (!el) return;
    el.style.transform = '';
    el.style.opacity = '';
    el.classList.remove('dragging');
  }

  function onPointerDown(e) {
    if (e.button > 0) return;
    if (e.target && typeof e.target.closest === 'function' && e.target.closest('.mini-player-actions')) {
      return;
    }
    const d = dragRef.current;
    d.active = true;
    d.pointerId = e.pointerId;
    d.startX = e.clientX;
    d.startY = e.clientY;
    d.startTime = Date.now();
    d.dy = 0;
    d.dragging = false;
    d.suppressClick = false;
  }

  function onPointerMove(e) {
    const d = dragRef.current;
    if (!d.active || d.pointerId !== e.pointerId) return;
    const dx = e.clientX - d.startX;
    const dy = e.clientY - d.startY;
    if (!d.dragging) {
      if (dy > 8 && dy > Math.abs(dx) * 1.2) {
        d.dragging = true;
        d.suppressClick = true;
        try {
          barRef.current?.setPointerCapture?.(e.pointerId);
        } catch {
          // Synthetic events may not have active pointer capture.
        }
        barRef.current?.classList.add('dragging');
      } else {
        return;
      }
    }
    d.dy = Math.max(0, dy);
    const el = barRef.current;
    if (el) {
      el.style.transform = `translateY(${d.dy}px)`;
      el.style.opacity = String(Math.max(0.25, 1 - d.dy / 84));
    }
  }

  function finishDrag(e, cancelled = false) {
    const d = dragRef.current;
    if (!d.active || d.pointerId !== e.pointerId) return;
    const wasDragging = d.dragging;
    const dy = d.dy;
    const dt = Math.max(1, Date.now() - d.startTime);
    const velocity = dy / dt;
    d.active = false;
    d.pointerId = null;
    d.dragging = false;
    d.dy = 0;
    try {
      if (barRef.current?.hasPointerCapture?.(e.pointerId)) {
        barRef.current.releasePointerCapture(e.pointerId);
      }
    } catch {
      // Ignore release errors.
    }
    resetDragVisual();
    if (!cancelled && wasDragging && (dy >= MINI_PLAYER_SWIPE_DISMISS_PX || (dy >= 16 && velocity > 0.35))) {
      musicController.dismiss();
    }
  }

  function handleOpenClick() {
    if (dragRef.current.suppressClick) {
      dragRef.current.suppressClick = false;
      return;
    }
    const rect = coverRef.current?.getBoundingClientRect?.() || null;
    const fromRect = rect
      ? {
          left: rect.left,
          top: rect.top,
          width: rect.width,
          height: rect.height,
        }
      : null;
    onOpenScreen?.(fromRect);
  }

  return html`
    <section
      ref=${barRef}
      class="mini-player"
      aria-label="Now playing"
      data-no-swipe="true"
      onPointerDown=${onPointerDown}
      onPointerMove=${onPointerMove}
      onPointerUp=${(e) => finishDrag(e, false)}
      onPointerCancel=${(e) => finishDrag(e, true)}
    >
      <div class="mini-player-progress" aria-hidden="true">
        <i style=${`transform:scaleX(${progress})`}></i>
      </div>
      <button
        type="button"
        class="mini-player-open"
        aria-label=${display.artist ? `Open player: ${display.title} by ${display.artist}` : `Open player: ${display.title}`}
        onClick=${handleOpenClick}
      >
        <span ref=${coverRef} class="mini-player-cover" aria-hidden="true">
          ${musicState.coverUrl
            ? html`<img src=${musicState.coverUrl} alt="" />`
            : html`<span class="mini-player-cover-tile"><${Icon} name="music" /></span>`}
        </span>
        <span class="mini-player-meta">
          <span class="mini-player-title" aria-label=${display.title} title=${display.title}>
            <${FileNameSingle} name=${display.title} lines=${1} titleOnHover=${false} />
          </span>
          ${display.artist ? html`<span class="mini-player-artist">${display.artist}</span>` : null}
        </span>
      </button>
      <div class="mini-player-actions">
        <button
          type="button"
          class="btn icon mini-player-btn"
          aria-label=${musicState.playing ? 'Pause' : 'Play'}
          onClick=${(e) => {
            e.stopPropagation();
            musicController.togglePlay();
          }}
        >
          ${busy
            ? html`<span class="mini-player-spinner" aria-hidden="true"></span>`
            : html`<${Icon} name=${musicState.playing ? 'pause' : 'play'} />`}
        </button>
        ${hasNext
          ? html`
              <button
                type="button"
                class="btn icon mini-player-btn"
                aria-label="Next track"
                onClick=${(e) => {
                  e.stopPropagation();
                  musicController.next();
                }}
              >
                <${Icon} name="next" />
              </button>
            `
          : null}
      </div>
    </section>
  `;
}
