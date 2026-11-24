// Shared UI primitives: meters, notices, sheets, copy button, QR code, swipe rows, and result viewer.

import { html, useEffect, useMemo, useRef, useState } from '../vendor/preact-htm.js';
import { Icon } from '../icons.js';
import { clampPct, meterLevel, tokenizeCode } from '../lib/format.js';
import { classifyRowSwipe } from '../lib/gestures.js';
import { qrSvgData } from '../lib/qr.js';
import { classifyResult, fanOutEntries, isFanOut } from '../lib/result.js';

// Renders text with single-backtick spans turned into inline <code> elements (text nodes only).
export function CodeText({ text }) {
  const parts = tokenizeCode(text);
  return parts.map((part, i) =>
    part.code ? html`<code key=${i}>${part.text}</code>` : part.text,
  );
}

// Sets the --v custom property through CSSOM (allowed by strict style-src 'self' CSP).
export function Meter({ value, level, className = '' }) {
  const ref = useRef(null);
  const pct = clampPct(value);
  const lvl = level !== undefined ? level : meterLevel(pct);
  useEffect(() => {
    if (ref.current) {
      ref.current.style.setProperty('--v', String(pct));
    }
  }, [pct]);
  const cls = ['meter', lvl, className].filter(Boolean).join(' ');
  return html`<span ref=${ref} class=${cls}><i></i></span>`;
}

export function Notice({ error, title, message, hint, kind = 'bad', className = '' }) {
  const heading = title || (error ? error.message : message);
  const detail = hint ?? (error ? error.hint : null);
  const machine = error?.machine ? `${error.machine}: ` : '';
  const reason = error?.details?.reason;
  const canRepair =
    error?.code === 'denied' &&
    (reason === 'device_removed_or_unknown' ||
      reason === 'auth_failed' ||
      reason === 'host_key_mismatch');
  if (!heading) return null;
  const cls = ['notice', kind === 'bad' ? 'bad' : '', className].filter(Boolean).join(' ');
  return html`
    <div class=${cls} role="status">
      <strong>${machine}${heading}</strong>
      ${detail ? html`<span><${CodeText} text=${detail} /></span>` : null}
      ${canRepair
        ? html`
            <div class="actions">
              <button
                type="button"
                class="btn small primary"
                onClick=${() => window.dispatchEvent(new CustomEvent('legion:open-pairing'))}
              >
                Pair again
              </button>
            </div>
          `
        : null}
    </div>
  `;
}

export function CopyButton({ text, label = 'Copy', iconOnly = false }) {
  const [copied, setCopied] = useState(false);

  async function onCopy(e) {
    if (e) e.stopPropagation();
    try {
      await navigator.clipboard.writeText(String(text ?? ''));
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1400);
    } catch {
      // Clipboard may be unavailable in non-secure contexts; ignore silently.
    }
  }

  if (iconOnly) {
    return html`
      <button
        type="button"
        class="btn icon"
        aria-label=${copied ? 'Copied' : label}
        title=${copied ? 'Copied' : label}
        onClick=${onCopy}
      >
        <${Icon} name="copy" />
      </button>
    `;
  }

  return html`
    <button type="button" class="btn small" onClick=${onCopy}>
      ${copied ? 'Copied' : label}
    </button>
  `;
}

// In-page SVG QR code (no network requests, no inline styles).
export function QrCode({ text, label = 'Invite QR code' }) {
  const data = useMemo(() => {
    if (!text) return null;
    try {
      return qrSvgData(String(text));
    } catch {
      return null;
    }
  }, [text]);

  if (!data) return null;

  return html`
    <div class="qr-box">
      <svg
        viewBox=${`0 0 ${data.viewBoxSize} ${data.viewBoxSize}`}
        role="img"
        aria-label=${label}
      >
        <path d=${data.path} fill="currentColor" />
      </svg>
    </div>
  `;
}

// Wraps a list row so a leftward pointer swipe reveals a destructive/secondary action button.
export function SwipeRow({ actionLabel, onAction, disabled = false, children }) {
  const [open, setOpen] = useState(false);
  const startRef = useRef(null);
  const suppressClickRef = useRef(false);

  function onPointerDown(e) {
    if (disabled || e.button > 0) return;
    startRef.current = { x: e.clientX, y: e.clientY, id: e.pointerId };
  }

  function onPointerUp(e) {
    const start = startRef.current;
    startRef.current = null;
    if (!start || start.id !== e.pointerId) return;
    const dx = e.clientX - start.x;
    const dy = e.clientY - start.y;
    const decision = classifyRowSwipe({ dx, dy, currentlyOpen: open });
    if (decision === 'reveal') {
      e.stopPropagation();
      suppressClickRef.current = true;
      setOpen(true);
    } else if (decision === 'close') {
      e.stopPropagation();
      suppressClickRef.current = true;
      setOpen(false);
    }
  }

  function onPointerCancel() {
    startRef.current = null;
  }

  function onClickCapture(e) {
    if (suppressClickRef.current) {
      suppressClickRef.current = false;
      e.preventDefault();
      e.stopPropagation();
    }
  }

  return html`
    <div
      class="swipe-row"
      data-no-swipe="true"
      onPointerDown=${onPointerDown}
      onPointerUp=${onPointerUp}
      onPointerCancel=${onPointerCancel}
      onClickCapture=${onClickCapture}
    >
      ${children}
      ${open && !disabled
        ? html`
            <button
              type="button"
              class="swipe-action"
              onClick=${(e) => {
                e.preventDefault();
                e.stopPropagation();
                setOpen(false);
                if (onAction) onAction();
              }}
            >
              ${actionLabel}
            </button>
          `
        : null}
    </div>
  `;
}

export function Sheet({ title, onClose, children }) {
  const sheetRef = useRef(null);

  useEffect(() => {
    const prev = document.activeElement;
    if (sheetRef.current) {
      sheetRef.current.focus();
    }
    const onKey = (e) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        onClose();
      }
    };
    window.addEventListener('keydown', onKey);
    return () => {
      window.removeEventListener('keydown', onKey);
      if (prev && typeof prev.focus === 'function') {
        prev.focus();
      }
    };
  }, [onClose]);

  function onScrimClick(e) {
    if (e.target === e.currentTarget) onClose();
  }

  return html`
    <div class="scrim" onClick=${onScrimClick}>
      <div
        ref=${sheetRef}
        class="sheet"
        role="dialog"
        aria-modal="true"
        aria-labelledby="sheet-title"
        tabIndex="-1"
      >
        <h2 id="sheet-title">${title}</h2>
        ${children}
      </div>
    </div>
  `;
}

function ResultNode({ node }) {
  if (!node) return null;
  if (node.kind === 'scalar') return html`<span>${node.text}</span>`;
  if (node.kind === 'empty-array') return html`<span class="dim">none</span>`;
  if (node.kind === 'empty-object') return html`<span class="dim">empty</span>`;
  if (node.kind === 'scalar-array') {
    return html`<span>${node.items.join(', ')}</span>`;
  }
  if (node.kind === 'object-array') {
    return html`
      <div class="kv-nested">
        ${node.items.map(
          (item, i) => html`
            <div key=${i} class="kv-item">
              <${ResultNode} node=${item} />
            </div>
          `,
        )}
      </div>
    `;
  }
  if (node.kind === 'object') {
    return html`
      <dl class="kv">
        ${node.entries.map(
          ({ key, node: child }) => html`
            <dt key=${`dt-${key}`}>${key}</dt>
            <dd key=${`dd-${key}`}><${ResultNode} node=${child} /></dd>
          `,
        )}
      </dl>
    `;
  }
  return null;
}

export function ResultView({ value, title = 'Result' }) {
  const [raw, setRaw] = useState(false);
  if (value === undefined || value === null) return null;
  const jsonText = JSON.stringify(value, null, 2);
  const fanned = isFanOut(value);

  return html`
    <div>
      <div class="result-head">
        <p class="label">${title}</p>
        <div class="result-tools">
          <button
            type="button"
            class="btn small quiet"
            onClick=${() => setRaw((v) => !v)}
          >
            ${raw ? 'Readable' : 'Raw JSON'}
          </button>
          <${CopyButton} text=${jsonText} label="Copy" />
        </div>
      </div>

      ${raw
        ? html`<pre class="out wrap">${jsonText}</pre>`
        : fanned
          ? fanOutEntries(value).map(
              (entry) => html`
                <div key=${entry.machine}>
                  <p class="label">${entry.machine}</p>
                  ${entry.ok
                    ? html`<div class="list"><${ResultNode} node=${classifyResult(entry.value)} /></div>`
                    : html`<${Notice} error=${entry.error} />`}
                </div>
              `,
            )
          : html`<div class="list"><${ResultNode} node=${classifyResult(value)} /></div>`}
    </div>
  `;
}
