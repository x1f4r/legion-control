// Inline SVG symbols shared across the shell, machine parts, Files, and settings.
// All icons sit on a 24x24 grid with >= 2 px padding, 2 px stroke for outline glyphs,
// solid fills for transport controls and `more`, and optical centring within 0.35 px.

import { html } from './vendor/preact-htm.js';

export function IconSprite() {
  return html`
    <svg class="sprite" aria-hidden="true">
      <symbol id="i-machines" viewBox="0 0 24 24">
        <rect x="4" y="4" width="16" height="6.5" rx="2" />
        <rect x="4" y="13.5" width="16" height="6.5" rx="2" />
        <path d="M7.5 7.25h.01M7.5 16.75h.01" />
      </symbol>
      <symbol id="i-activity" viewBox="0 0 24 24">
        <path d="M3 12h4l2.5-7 5 14 2.5-7h4" />
      </symbol>
      <symbol id="i-actions" viewBox="0 0 24 24">
        <path d="M13.5 3 5.5 13.5h6l-1 7.5 8-10.5h-6z" />
      </symbol>
      <symbol id="i-gear" viewBox="0 0 24 24">
        <path d="M10.4 5.5 10.6 3.2h2.8l.2 2.3 2 .8 1.6-1.6 2 2-1.6 1.6.8 2 2.3.2v2.8l-2.3.2-.8 2 1.6 1.6-2 2-1.6-1.6-2 .8-.2 2.3h-2.8l-.2-2.3-2-.8-1.6 1.6-2-2 1.6-1.6-.8-2-2.3-.2v-2.8l2.3-.2.8-2-1.6-1.6 2-2 1.6 1.6z" />
        <circle cx="12" cy="12" r="3" />
      </symbol>
      <symbol id="i-back" viewBox="0 0 24 24">
        <path d="M15.7 4 7.7 12l8 8" />
      </symbol>
      <symbol id="i-down" viewBox="0 0 24 24">
        <path d="M4 8.3l8 8 8-8" />
      </symbol>
      <symbol id="i-overview" viewBox="0 0 24 24">
        <rect x="3" y="8.8" width="3.4" height="9.2" rx="1" />
        <rect x="9.9" y="3.4" width="3.4" height="14.6" rx="1" />
        <rect x="16.8" y="6.4" width="3.4" height="11.6" rx="1" />
      </symbol>
      <symbol id="i-files" viewBox="0 0 24 24">
        <path d="M3.2 6.5a2 2 0 0 1 2-2h5.2l2 2H19a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5.2a2 2 0 0 1-2-2z" />
      </symbol>
      <symbol id="i-power" viewBox="0 0 24 24">
        <path d="M12 3v7" />
        <path data-part="ring" data-cx="12" data-cy="12" d="M6.2 6.2a8.2 8.2 0 1 0 11.6 0" />
      </symbol>
      <symbol id="i-more" viewBox="0 0 24 24">
        <circle cx="5" cy="12" r="2" fill="currentColor" stroke="none" />
        <circle cx="12" cy="12" r="2" fill="currentColor" stroke="none" />
        <circle cx="19" cy="12" r="2" fill="currentColor" stroke="none" />
      </symbol>
      <symbol id="i-copy" viewBox="0 0 24 24">
        <rect x="8.5" y="8.5" width="11.5" height="11.5" rx="2" />
        <path d="M15.5 4.5H6a2 2 0 0 0-2 2v9.5" />
      </symbol>
      <symbol id="i-refresh" viewBox="0 0 24 24">
        <path data-part="ring" data-cx="12" data-cy="12" d="M20.5 12a8.5 8.5 0 1 1-2.5-6" />
        <path d="M20.5 3.5V8H16" />
      </symbol>
      <symbol id="i-video" viewBox="0 0 24 24">
        <rect x="3" y="5" width="18" height="14" rx="2.5" />
        <path d="m10 9 5.2 3-5.2 3z" fill="currentColor" stroke="none" />
      </symbol>
      <symbol id="i-image" viewBox="0 0 24 24">
        <rect x="3" y="5" width="18" height="14" rx="2.5" />
        <circle cx="9.2" cy="9.2" r="1.4" />
        <path d="m4.5 16.8 4.8-4.6 2.8 2.4 3.4-3.6L20 15.5" />
      </symbol>
      <symbol id="i-music" viewBox="0 0 24 24">
        <path d="M8.3 17.1V5.6l10-2v11.5" />
        <circle cx="5.8" cy="17.1" r="2.5" />
        <circle cx="15.8" cy="15.1" r="2.5" />
      </symbol>
      <symbol id="i-doc" viewBox="0 0 24 24">
        <path d="M5 3h9l5 5v13H5z" />
        <path d="M14 3v5h5M8.5 11h7M8.5 14.8h7" />
      </symbol>
      <symbol id="i-archive" viewBox="0 0 24 24">
        <rect x="3" y="5.3" width="18" height="4" rx="1" />
        <path d="M4.5 9.3v9.5a1.5 1.5 0 0 0 1.5 1.5h12a1.5 1.5 0 0 0 1.5-1.5V9.3" />
        <path d="M9.5 14.8h5" />
      </symbol>
      <symbol id="i-grid" viewBox="0 0 24 24">
        <rect x="4" y="4" width="6.5" height="6.5" rx="1.5" />
        <rect x="13.5" y="4" width="6.5" height="6.5" rx="1.5" />
        <rect x="4" y="13.5" width="6.5" height="6.5" rx="1.5" />
        <rect x="13.5" y="13.5" width="6.5" height="6.5" rx="1.5" />
      </symbol>
      <symbol id="i-list" viewBox="0 0 24 24">
        <path d="M3.5 6.5h17M3.5 12h17M3.5 17.5h17" />
      </symbol>
      <symbol id="i-sort" viewBox="0 0 24 24">
        <path d="M4 8.2h16M6.25 13.2h11.5M8.5 18.2h7" />
      </symbol>
      <symbol id="i-download" viewBox="0 0 24 24">
        <path d="M12 3.2v9.8M7.2 8.2 12 13l4.8-4.8M6.5 17.6h11" />
      </symbol>
      <symbol id="i-upload" viewBox="0 0 24 24">
        <path d="M12 14.6V4M7.2 8.8 12 4 16.8 8.8M6.5 19.3h11" />
      </symbol>
      <symbol id="i-send" viewBox="0 0 24 24">
        <path d="M3 12h10.8M8.8 7 13.8 12l-5 5M18 7v10" />
      </symbol>
      <symbol id="i-star" viewBox="0 0 24 24">
        <path d="m12 2.9 2.7 5.5 6 .9-4.3 4.2 1 6-5.4-2.9-5.4 2.9 1-6-4.3-4.2 6-.9z" />
      </symbol>
      <symbol id="i-bin" viewBox="0 0 24 24">
        <path d="M4.5 6.5h15M9 6.5V4a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2.5M6.5 6.5l1 13.5a1 1 0 0 0 1 1h7a1 1 0 0 0 1-1l1-13.5M10 11v6M14 11v6" />
      </symbol>
      <symbol id="i-sync" viewBox="0 0 24 24">
        <path data-part="ring" data-cx="12" data-cy="12" d="M3.5 12a8.5 8.5 0 0 1 14.5-6l1.5 1.5M20.5 12a8.5 8.5 0 0 1-14.5 6L4.5 16.5" />
        <path d="M19.5 3.5V8H15M4.5 20.5V16H9" />
      </symbol>
      <symbol id="i-pause" viewBox="0 0 24 24">
        <rect x="6" y="5" width="4" height="14" rx="1" fill="currentColor" stroke="none" />
        <rect x="14" y="5" width="4" height="14" rx="1" fill="currentColor" stroke="none" />
      </symbol>
      <symbol id="i-play" viewBox="0 0 24 24">
        <path d="M7.5 4.2 21 12 7.5 19.8z" fill="currentColor" stroke="none" />
      </symbol>
      <symbol id="i-prev" viewBox="0 0 24 24">
        <rect x="3.8" y="5" width="2.5" height="14" rx="1" fill="currentColor" stroke="none" />
        <path d="M18.8 5 7.5 12l11.3 7z" fill="currentColor" stroke="none" />
      </symbol>
      <symbol id="i-next" viewBox="0 0 24 24">
        <path d="M5.2 5 16.5 12 5.2 19z" fill="currentColor" stroke="none" />
        <rect x="17.7" y="5" width="2.5" height="14" rx="1" fill="currentColor" stroke="none" />
      </symbol>
      <symbol id="i-skip-back-10" viewBox="0 0 24 24">
        <path data-part="circle" data-cx="12" data-cy="12" d="M12 4.6a7.4 7.4 0 1 1-6.409 3.7" />
        <path data-part="arrow" d="M14.2 3 12 4.6l2.2 1.8" />
        <g data-part="digits" stroke-width="1.95">
          <path d="M8.5 10.6 9.8 9.4v5.2" />
          <rect x="12.2" y="9.4" width="3.3" height="5.2" rx="1.6" />
        </g>
      </symbol>
      <symbol id="i-skip-fwd-10" viewBox="0 0 24 24">
        <path data-part="circle" data-cx="12" data-cy="12" d="M12 4.6a7.4 7.4 0 1 0 6.409 3.7" />
        <path data-part="arrow" d="M9.8 3 12 4.6 9.8 6.4" />
        <g data-part="digits" stroke-width="1.95">
          <path d="M8.5 10.6 9.8 9.4v5.2" />
          <rect x="12.2" y="9.4" width="3.3" height="5.2" rx="1.6" />
        </g>
      </symbol>
      <symbol id="i-fast-forward" viewBox="0 0 24 24">
        <path d="M5.1 5.5 13.3 12 5.1 18.5z" fill="currentColor" stroke="none" />
        <path d="M13.3 5.5 21.5 12 13.3 18.5z" fill="currentColor" stroke="none" />
      </symbol>
      <symbol id="i-fullscreen" viewBox="0 0 24 24">
        <path d="M4 9V5.5A1.5 1.5 0 0 1 5.5 4H9M15 4h3.5A1.5 1.5 0 0 1 20 5.5V9M20 15v3.5a1.5 1.5 0 0 1-1.5 1.5H15M9 20H5.5A1.5 1.5 0 0 1 4 18.5V15" />
      </symbol>
      <symbol id="i-exit-fullscreen" viewBox="0 0 24 24">
        <path d="M9 4v3.5A1.5 1.5 0 0 1 7.5 9H4M20 9h-3.5A1.5 1.5 0 0 1 15 7.5V4M15 20v-3.5a1.5 1.5 0 0 1 1.5-1.5H20M4 15h3.5A1.5 1.5 0 0 1 9 16.5V20" />
      </symbol>
      <symbol id="i-pip" viewBox="0 0 24 24">
        <path d="M20.6 9.8V6.8a2.5 2.5 0 0 0-2.5-2.5h-13a2.5 2.5 0 0 0-2.5 2.5v10a2.5 2.5 0 0 0 2.5 2.5h4" />
        <rect x="12.1" y="12.3" width="8.5" height="7" rx="1.5" />
      </symbol>
      <symbol id="i-volume" viewBox="0 0 24 24">
        <path d="M3.5 9.2H7l4.5-3.7v13L7 14.8H3.5z" />
        <path d="M15 9.1a4.2 4.2 0 0 1 0 5.8M17.8 6.4a7.8 7.8 0 0 1 0 11.2" />
      </symbol>
      <symbol id="i-volume-mute" viewBox="0 0 24 24">
        <path d="M4.1 9.2h3.5l4.5-3.7v13L7.6 14.8H4.1z" />
        <path d="m15.6 9.2 5.2 5.6M20.8 9.2l-5.2 5.6" />
      </symbol>
      <symbol id="i-shuffle" viewBox="0 0 24 24">
        <path d="M3.1 7.5h3.5l6.4 9h5.6M3.1 16.5h3.5l6.4-9h5.6" />
        <path d="M16.2 4.9 18.8 7.5l-2.6 2.6M16.2 13.9l2.6 2.6-2.6 2.6" />
      </symbol>
      <symbol id="i-repeat" viewBox="0 0 24 24">
        <path d="M4.5 10.6V8.8a2.5 2.5 0 0 1 2.5-2.5h12.5" />
        <path d="M16.9 3.7 19.5 6.3l-2.6 2.6" />
        <path d="M19.5 13.4v1.8a2.5 2.5 0 0 1-2.5 2.5H4.5" />
        <path d="M7.1 20.3 4.5 17.7l2.6-2.6" />
      </symbol>
      <symbol id="i-repeat-1" viewBox="0 0 24 24">
        <path d="M4.5 10.6V8.8a2.5 2.5 0 0 1 2.5-2.5h12.5" />
        <path d="M16.9 3.7 19.5 6.3l-2.6 2.6" />
        <path d="M19.5 13.4v1.8a2.5 2.5 0 0 1-2.5 2.5H4.5" />
        <path d="M7.1 20.3 4.5 17.7l2.6-2.6" />
        <path d="M10.9 10.7 12.3 9.5v5" stroke-width="1.9" />
      </symbol>
    </svg>
  `;
}

export function Icon({ name }) {
  return html`<svg aria-hidden="true"><use href=${`#i-${name}`} /></svg>`;
}
