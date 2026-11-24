// Inline SVG symbols from apps/ui/design/prototype.html.

import { html } from './vendor/preact-htm.js';

export function IconSprite() {
  return html`
    <svg class="sprite" aria-hidden="true">
      <symbol id="i-machines" viewBox="0 0 24 24">
        <rect x="3.5" y="4" width="17" height="6.5" rx="2" />
        <rect x="3.5" y="13.5" width="17" height="6.5" rx="2" />
        <path d="M7 7.25h.01M7 16.75h.01" />
      </symbol>
      <symbol id="i-jobs" viewBox="0 0 24 24">
        <path d="M4 6h10M4 12h16M4 18h7" />
        <circle cx="18" cy="6" r="2" />
        <circle cx="15" cy="18" r="2" />
      </symbol>
      <symbol id="i-tools" viewBox="0 0 24 24">
        <rect x="4" y="4" width="6.5" height="6.5" rx="1.5" />
        <rect x="13.5" y="4" width="6.5" height="6.5" rx="1.5" />
        <rect x="4" y="13.5" width="6.5" height="6.5" rx="1.5" />
        <rect x="13.5" y="13.5" width="6.5" height="6.5" rx="1.5" />
      </symbol>
      <symbol id="i-device" viewBox="0 0 24 24">
        <rect x="7" y="3" width="10" height="18" rx="2.5" />
        <path d="M11 17.5h2" />
      </symbol>
      <symbol id="i-back" viewBox="0 0 24 24">
        <path d="M14.5 5.5 8 12l6.5 6.5" />
      </symbol>
      <symbol id="i-refresh" viewBox="0 0 24 24">
        <path d="M19.5 12a7.5 7.5 0 1 1-2.2-5.3M19.5 4.5v3.6h-3.6" />
      </symbol>
      <symbol id="i-copy" viewBox="0 0 24 24">
        <rect x="8.5" y="8.5" width="11" height="11" rx="2" />
        <path d="M15.5 5.5h-9a2 2 0 0 0-2 2v9" />
      </symbol>
    </svg>
  `;
}

export function Icon({ name }) {
  return html`<svg aria-hidden="true"><use href=${`#i-${name}`} /></svg>`;
}
