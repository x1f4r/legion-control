// Shared UI primitives: meters, notices, confirmation sheets, copy button, and JSON result viewer.

import { html, useEffect, useRef, useState } from '../vendor/preact-htm.js';
import { Icon } from '../icons.js';
import { clampPct, meterLevel } from '../lib/format.js';
import { classifyResult, fanOutEntries, isFanOut } from '../lib/result.js';

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
  if (!heading) return null;
  const cls = ['notice', kind === 'bad' ? 'bad' : '', className].filter(Boolean).join(' ');
  return html`
    <div class=${cls} role="status">
      <strong>${machine}${heading}</strong>
      ${detail ? html`<span>${detail}</span>` : null}
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
