// Inline SVG symbols shared across the shell, machine parts, Files, and settings.

import { html } from './vendor/preact-htm.js';

export function IconSprite() {
  return html`
    <svg class="sprite" aria-hidden="true">
      <symbol id="i-machines" viewBox="0 0 24 24">
        <rect x="3.5" y="4" width="17" height="6.5" rx="2" />
        <rect x="3.5" y="13.5" width="17" height="6.5" rx="2" />
        <path d="M7 7.25h.01M7 16.75h.01" />
      </symbol>
      <symbol id="i-activity" viewBox="0 0 24 24">
        <path d="M3.5 12h4l2.5-6 4 12 2.5-6h4" />
      </symbol>
      <symbol id="i-actions" viewBox="0 0 24 24">
        <path d="M13 3.5 5.5 13.5H11l-1 7 7.5-10H12z" />
      </symbol>
      <symbol id="i-gear" viewBox="0 0 24 24">
        <path d="M10.4 5.3 10.5 2.9h3l.1 2.4 2.1.8 1.6-1.6 2.2 2.2-1.6 1.6.8 2.1 2.4.1v3l-2.4.1-.8 2.1 1.6 1.6-2.2 2.2-1.6-1.6-2.1.8-.1 2.4h-3l-.1-2.4-2.1-.8-1.6 1.6-2.2-2.2 1.6-1.6-.8-2.1-2.4-.1v-3l2.4-.1.8-2.1-1.6-1.6 2.2-2.2 1.6 1.6z" />
        <circle cx="12" cy="12" r="3" />
      </symbol>
      <symbol id="i-back" viewBox="0 0 24 24">
        <path d="M14.5 5.5 8 12l6.5 6.5" />
      </symbol>
      <symbol id="i-down" viewBox="0 0 24 24">
        <path d="M5.5 9 12 15.5 18.5 9" />
      </symbol>
      <symbol id="i-overview" viewBox="0 0 24 24">
        <path d="M4 19V9M10 19V5M16 19v-7M20.5 19h-17" />
      </symbol>
      <symbol id="i-files" viewBox="0 0 24 24">
        <path d="M4 7.5a2 2 0 0 1 2-2h3.5l2 2H18a2 2 0 0 1 2 2V17a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2z" />
      </symbol>
      <symbol id="i-power" viewBox="0 0 24 24">
        <path d="M12 4v7" />
        <path d="M7.2 6.8a7 7 0 1 0 9.6 0" />
      </symbol>
      <symbol id="i-more" viewBox="0 0 24 24">
        <path d="M6 12h.01M12 12h.01M18 12h.01" />
      </symbol>
      <symbol id="i-copy" viewBox="0 0 24 24">
        <rect x="8.5" y="8.5" width="11" height="11" rx="2" />
        <path d="M15.5 5.5h-9a2 2 0 0 0-2 2v9" />
      </symbol>
      <symbol id="i-refresh" viewBox="0 0 24 24">
        <path d="M19.5 12a7.5 7.5 0 1 1-2.2-5.3M19.5 4.5v3.6h-3.6" />
      </symbol>
      <symbol id="i-video" viewBox="0 0 24 24">
        <rect x="3.5" y="5.5" width="17" height="13" rx="2.5" />
        <path d="m10.5 9.5 4 2.5-4 2.5z" />
      </symbol>
      <symbol id="i-image" viewBox="0 0 24 24">
        <rect x="3.5" y="5" width="17" height="14" rx="2.5" />
        <circle cx="9" cy="10" r="1.5" />
        <path d="m5 17 4.5-4 3 2.5 3-3L20 16" />
      </symbol>
      <symbol id="i-music" viewBox="0 0 24 24">
        <path d="M9.5 17V6.5l9-2V15" />
        <circle cx="7" cy="17" r="2.5" />
        <circle cx="16" cy="15" r="2.5" />
      </symbol>
      <symbol id="i-doc" viewBox="0 0 24 24">
        <path d="M7 3.5h7l4 4V20.5H7z" />
        <path d="M14 3.5V8h4M9.5 12.5h6M9.5 16h6" />
      </symbol>
      <symbol id="i-archive" viewBox="0 0 24 24">
        <rect x="3.5" y="4.5" width="17" height="4" rx="1" />
        <path d="M5 8.5v10a1.5 1.5 0 0 0 1.5 1.5h11a1.5 1.5 0 0 0 1.5-1.5v-10" />
        <path d="M10 13h4" />
      </symbol>
      <symbol id="i-grid" viewBox="0 0 24 24">
        <rect x="4" y="4" width="6.5" height="6.5" rx="1.5" />
        <rect x="13.5" y="4" width="6.5" height="6.5" rx="1.5" />
        <rect x="4" y="13.5" width="6.5" height="6.5" rx="1.5" />
        <rect x="13.5" y="13.5" width="6.5" height="6.5" rx="1.5" />
      </symbol>
      <symbol id="i-list" viewBox="0 0 24 24">
        <path d="M4.5 7h15M4.5 12h15M4.5 17h15" />
      </symbol>
      <symbol id="i-sort" viewBox="0 0 24 24">
        <path d="M5 7h14M7.5 12h9M10 17h4" />
      </symbol>
      <symbol id="i-download" viewBox="0 0 24 24">
        <path d="M12 4v11M7.5 11 12 15.5 16.5 11M5 19.5h14" />
      </symbol>
      <symbol id="i-upload" viewBox="0 0 24 24">
        <path d="M12 15.5V4.5M7.5 9 12 4.5 16.5 9M5 19.5h14" />
      </symbol>
      <symbol id="i-send" viewBox="0 0 24 24">
        <path d="M4 12h13M12.5 6.5 18 12l-5.5 5.5" />
        <path d="M20.5 5v14" />
      </symbol>
      <symbol id="i-star" viewBox="0 0 24 24">
        <path d="m12 4 2.4 5 5.4.7-4 3.8 1 5.5-4.8-2.7L7.2 19l1-5.5-4-3.8 5.4-.7z" />
      </symbol>
      <symbol id="i-bin" viewBox="0 0 24 24">
        <path d="M5 7h14M9.5 7V4.5h5V7M7 7l1 12.5h8L17 7" />
      </symbol>
      <symbol id="i-sync" viewBox="0 0 24 24">
        <path d="M5 12a7 7 0 0 1 12-4.9L19 9M19 12a7 7 0 0 1-12 4.9L5 15" />
        <path d="M19 4.5V9h-4.5M5 19.5V15h4.5" />
      </symbol>
      <symbol id="i-pause" viewBox="0 0 24 24">
        <path d="M8 5.5v13M16 5.5v13" />
      </symbol>
      <symbol id="i-play" viewBox="0 0 24 24">
        <path d="m8 5.5 11 6.5-11 6.5z" />
      </symbol>
      <symbol id="i-prev" viewBox="0 0 24 24">
        <path d="M6.5 6v12" />
        <path d="m17.5 6.5-8 5.5 8 5.5z" />
      </symbol>
      <symbol id="i-next" viewBox="0 0 24 24">
        <path d="M17.5 6v12" />
        <path d="m6.5 6.5 8 5.5-8 5.5z" />
      </symbol>
      <symbol id="i-skip-back-10" viewBox="0 0 24 24">
        <path d="M11.5 5.5a6.5 6.5 0 1 1-5.8 3.6" />
        <path d="M8 5.5H4.5V9" />
        <path d="M10.2 10.6v4.8" />
        <rect x="12.6" y="10.6" width="3.2" height="4.8" rx="1.4" />
      </symbol>
      <symbol id="i-skip-fwd-10" viewBox="0 0 24 24">
        <path d="M12.5 5.5a6.5 6.5 0 1 0 5.8 3.6" />
        <path d="M16 5.5h3.5V9" />
        <path d="M9.4 10.6v4.8" />
        <rect x="11.8" y="10.6" width="3.2" height="4.8" rx="1.4" />
      </symbol>
      <symbol id="i-fullscreen" viewBox="0 0 24 24">
        <path d="M4.5 9V5.5A1 1 0 0 1 5.5 4.5H9M15 4.5h3.5a1 1 0 0 1 1 1V9M19.5 15v3.5a1 1 0 0 1-1 1H15M9 19.5H5.5a1 1 0 0 1-1-1V15" />
      </symbol>
      <symbol id="i-exit-fullscreen" viewBox="0 0 24 24">
        <path d="M9 4.5V8a1 1 0 0 1-1 1H4.5M19.5 9H16a1 1 0 0 1-1-1V4.5M15 19.5V16a1 1 0 0 1 1-1h3.5M4.5 15H8a1 1 0 0 1 1 1v3.5" />
      </symbol>
      <symbol id="i-volume" viewBox="0 0 24 24">
        <path d="M4.5 9.5H8l4.5-3.8v12.6L8 14.5H4.5z" />
        <path d="M15.8 9.2a4 4 0 0 1 0 5.6M18.3 6.8a7.4 7.4 0 0 1 0 10.4" />
      </symbol>
      <symbol id="i-volume-mute" viewBox="0 0 24 24">
        <path d="M4.5 9.5H8l4.5-3.8v12.6L8 14.5H4.5z" />
        <path d="m15.5 9.5 4.5 5M20 9.5l-4.5 5" />
      </symbol>
    </svg>
  `;
}

export function Icon({ name }) {
  return html`<svg aria-hidden="true"><use href=${`#i-${name}`} /></svg>`;
}
