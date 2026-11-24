// Inline SVG symbols shared across the shell, machine parts, and settings.

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
        <circle cx="12" cy="12" r="3" />
        <path d="M12 3.5v2.2M12 18.3v2.2M3.5 12h2.2M18.3 12h2.2M6 6l1.6 1.6M16.4 16.4 18 18M18 6l-1.6 1.6M7.6 16.4 6 18" />
      </symbol>
      <symbol id="i-back" viewBox="0 0 24 24">
        <path d="M14.5 5.5 8 12l6.5 6.5" />
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
    </svg>
  `;
}

export function Icon({ name }) {
  return html`<svg aria-hidden="true"><use href=${`#i-${name}`} /></svg>`;
}
