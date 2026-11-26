// Shared canvas measurement, per-list ResizeObserver + IntersectionObserver, and `<FileName>`
// rendering components for every file and folder listing.

import {
  createContext,
  html,
  useCallback,
  useContext,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from '../vendor/preact-htm.js';
import { applySearchHighlight, fitName } from '../lib/file-name.js';

// ── Shared offscreen canvas text measurer & bounded caches ───────────────────────────────────

const MAX_FONT_TEXT_CACHE = 4000;
const MAX_FIT_CACHE = 2000;

let sharedCanvasCtx = null;
const fontWidthCaches = new Map();
const fitResultCache = new Map();

function getCanvasCtx() {
  if (sharedCanvasCtx) return sharedCanvasCtx;
  if (typeof document === 'undefined') return null;
  const canvas = document.createElement('canvas');
  sharedCanvasCtx = canvas.getContext('2d');
  return sharedCanvasCtx;
}

function readElementFontSpec(el) {
  if (!el || typeof window === 'undefined' || typeof window.getComputedStyle !== 'function') {
    return { font: '550 15px sans-serif', letterSpacing: 0 };
  }
  const cs = window.getComputedStyle(el);
  const style = cs.fontStyle || 'normal';
  const weight = cs.fontWeight || '400';
  const size = cs.fontSize || '15px';
  const family = cs.fontFamily || 'sans-serif';
  const lsRaw = cs.letterSpacing;
  const letterSpacing = lsRaw && lsRaw !== 'normal' ? Number.parseFloat(lsRaw) || 0 : 0;
  return {
    font: `${style} ${weight} ${size} ${family}`,
    letterSpacing,
  };
}

function createFontMeasurer(font, letterSpacing = 0) {
  const ctx = getCanvasCtx();
  const cacheKey = `${font}|${letterSpacing}`;
  let widths = fontWidthCaches.get(cacheKey);
  if (!widths) {
    widths = new Map();
    fontWidthCaches.set(cacheKey, widths);
  }

  return (text) => {
    if (!text) return 0;
    const hit = widths.get(text);
    if (hit !== undefined) return hit;

    let w;
    if (ctx) {
      if (ctx.font !== font) ctx.font = font;
      let usedNativeSpacing = false;
      if ('letterSpacing' in ctx) {
        const target = letterSpacing ? `${letterSpacing}px` : '0px';
        if (ctx.letterSpacing !== target) ctx.letterSpacing = target;
        usedNativeSpacing = true;
      }
      w = ctx.measureText(text).width;
      if (!usedNativeSpacing && letterSpacing && text.length > 1) {
        w += (text.length - 1) * letterSpacing;
      }
    } else {
      // Fallback for non-browser environments.
      w = text.length * 8;
    }

    if (widths.size >= MAX_FONT_TEXT_CACHE) {
      widths.clear();
    }
    widths.set(text, w);
    return w;
  };
}

function recordPerfStats(batchMs, batchRows, cacheHitDelta, cacheMissDelta) {
  if (typeof window === 'undefined') return;
  const prev = window.__LEGION_NAME_FIT_STATS__ || {
    lastBatchMs: 0,
    lastBatchRows: 0,
    totalMeasuredRows: 0,
    totalBatches: 0,
    cacheHits: 0,
    cacheMisses: 0,
    activeListResizeObservers: 0,
  };
  prev.lastBatchMs = batchMs;
  prev.lastBatchRows = batchRows;
  prev.totalMeasuredRows += batchRows;
  prev.totalBatches += 1;
  prev.cacheHits += cacheHitDelta;
  prev.cacheMisses += cacheMissDelta;
  window.__LEGION_NAME_FIT_STATS__ = prev;
}

function adjustObserverCount(delta) {
  if (typeof window === 'undefined') return;
  const prev = window.__LEGION_NAME_FIT_STATS__ || {
    lastBatchMs: 0,
    lastBatchRows: 0,
    totalMeasuredRows: 0,
    totalBatches: 0,
    cacheHits: 0,
    cacheMisses: 0,
    activeListResizeObservers: 0,
  };
  prev.activeListResizeObservers = Math.max(0, (prev.activeListResizeObservers || 0) + delta);
  window.__LEGION_NAME_FIT_STATS__ = prev;
}

function cachedFitName(name, before, after, measure, width, lines, { dim = true, peers = null, fontKey = '' } = {}) {
  const intWidth = Math.max(1, Math.floor(width));
  const peerKey = Array.isArray(peers) && peers.length > 0 ? peers.join('\u0001') : '';
  const key = `${fontKey}\0${intWidth}\0${lines}\0${dim ? 1 : 0}\0${name}\0${before || ''}\0${after || ''}\0${peerKey}`;
  const hit = fitResultCache.get(key);
  if (hit) {
    return { result: hit, hit: true };
  }
  const result = fitName(name, before, after, measure, intWidth, lines, { dim, peers });
  if (fitResultCache.size >= MAX_FIT_CACHE) {
    fitResultCache.clear();
  }
  fitResultCache.set(key, result);
  return { result, hit: false };
}

// ── Single document-level watcher for text-size, density, technical mode, and web fonts ──────

const fontChangeListeners = new Set();
let rootObserverStarted = false;

function ensureRootFontObserver() {
  if (rootObserverStarted || typeof document === 'undefined') return;
  rootObserverStarted = true;

  const notifyAll = () => {
    // Font/text-size changes invalidate cached measurements cleanly.
    fontWidthCaches.clear();
    fitResultCache.clear();
    for (const fn of fontChangeListeners) fn();
  };

  if (typeof MutationObserver === 'function' && document.documentElement) {
    const mo = new MutationObserver(notifyAll);
    mo.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ['data-text', 'data-density', 'data-technical', 'class', 'style'],
    });
  }

  if (document.fonts && typeof document.fonts.addEventListener === 'function') {
    document.fonts.addEventListener('loadingdone', notifyAll);
  }
  if (document.fonts && document.fonts.ready && typeof document.fonts.ready.then === 'function') {
    document.fonts.ready.then(notifyAll).catch(() => {});
  }
}

function subscribeFontChanges(cb) {
  ensureRootFontObserver();
  fontChangeListeners.add(cb);
  return () => {
    fontChangeListeners.delete(cb);
  };
}

// ── Shared line rendering helper ─────────────────────────────────────────────────────────────

function renderFittedLines(result, highlight = '') {
  const lines = highlight ? applySearchHighlight(result.lines, highlight) : result.lines;
  return lines.map(
    (spans, lineIdx) => html`
      <span key=${lineIdx} class="fn-line" dir="auto">
        ${spans.map((span, spanIdx) => {
          if (span.match) {
            return html`<mark key=${spanIdx} class="fn-match">${span.text}</mark>`;
          }
          if (span.dim) {
            return html`<span key=${spanIdx} class="fn-dim">${span.text}</span>`;
          }
          return span.text;
        })}
      </span>
    `,
  );
}

// ── Per-list coordinator: ONE ResizeObserver + ONE IntersectionObserver per list ─────────────

const FileNameListContext = createContext(null);

function findScrollRoot(el) {
  if (!el || typeof window === 'undefined' || typeof window.getComputedStyle !== 'function') return null;
  let cur = el.parentElement;
  while (cur && cur !== document.body && cur !== document.documentElement) {
    const oy = window.getComputedStyle(cur).overflowY;
    if (oy === 'auto' || oy === 'scroll' || oy === 'overlay') {
      return cur;
    }
    cur = cur.parentElement;
  }
  return null;
}

export function FileNameList({
  as = 'div',
  className = 'list',
  lines = 2,
  dim = true,
  children,
  ...rest
}) {
  const containerRef = useRef(null);
  const itemsRef = useRef(new Map()); // id -> entry
  const elToIdRef = useRef(new WeakMap()); // DOM Element -> id
  const visibleIdsRef = useRef(new Set());
  const ioRef = useRef(null);
  const rafRef = useRef(0);
  const nextIdRef = useRef(1);
  const defaultsRef = useRef({ lines, dim });
  defaultsRef.current = { lines, dim };

  const flushMeasure = useCallback(() => {
    rafRef.current = 0;
    const items = itemsRef.current;
    const visibleIds = visibleIdsRef.current;
    if (visibleIds.size === 0) return;

    const t0 = typeof performance !== 'undefined' ? performance.now() : Date.now();

    // Phase 1: Batch all DOM reads (clientWidth + computed font) before any state writes so we
    // never interleave reads and writes (zero forced layout thrashing).
    const active = [];
    const fontCache = new Map(); // sample class -> { font, letterSpacing, measure, fontKey }

    for (const id of visibleIds) {
      const item = items.get(id);
      if (!item || !item.el) continue;
      const rawWidth = item.el.getBoundingClientRect().width;
      if (!(rawWidth > 1)) continue;
      // Leave a 1px safety cushion so subpixel rounding between canvas and DOM never triggers an
      // extra wrap.
      const width = Math.max(1, Math.floor(rawWidth) - 1);

      const styleGroup = item.className || '';
      let spec = fontCache.get(styleGroup);
      if (!spec) {
        const read = readElementFontSpec(item.el);
        spec = {
          ...read,
          fontKey: `${read.font}|${read.letterSpacing}`,
          measure: createFontMeasurer(read.font, read.letterSpacing),
        };
        fontCache.set(styleGroup, spec);
      }

      active.push({ item, width, spec });
    }

    if (active.length === 0) return;

    // Phase 2: Pure layout via `fitName` (cached per width & font).
    let hits = 0;
    let misses = 0;
    const results = new Array(active.length);

    for (let i = 0; i < active.length; i += 1) {
      const { item, width, spec } = active[i];
      const useLines = item.lines ?? defaultsRef.current.lines;
      const useDim = item.dim ?? defaultsRef.current.dim;
      const fitted = cachedFitName(item.name, item.before, item.after, spec.measure, width, useLines, {
        dim: useDim,
        fontKey: spec.fontKey,
      });
      if (fitted.hit) hits += 1;
      else misses += 1;
      results[i] = fitted.result;
    }

    // Phase 3: Guarantee no two distinct visible names in this list share the same drawn text
    // (handles non-adjacent collisions across any sort order).
    for (let pass = 0; pass < 3; pass += 1) {
      const buckets = new Map();
      for (let i = 0; i < active.length; i += 1) {
        const drawn = results[i].text;
        let bucket = buckets.get(drawn);
        if (!bucket) {
          bucket = [];
          buckets.set(drawn, bucket);
        }
        bucket.push(i);
      }
      let resolvedAny = false;
      for (const idxs of buckets.values()) {
        if (idxs.length < 2) continue;
        const distinctNames = new Set(idxs.map((k) => active[k].item.name));
        if (distinctNames.size < 2) continue;
        for (const k of idxs) {
          const { item, width, spec } = active[k];
          const useLines = item.lines ?? defaultsRef.current.lines;
          const useDim = item.dim ?? defaultsRef.current.dim;
          const peers = Array.from(distinctNames).filter((n) => n !== item.name);
          const fitted = cachedFitName(item.name, item.before, item.after, spec.measure, width, useLines, {
            dim: useDim,
            peers,
            fontKey: spec.fontKey,
          });
          if (fitted.hit) hits += 1;
          else misses += 1;
          if (fitted.result.text !== results[k].text) {
            results[k] = fitted.result;
            resolvedAny = true;
          }
        }
      }
      if (!resolvedAny) break;
    }

    // Phase 4: Commit changed fit results in one pass.
    for (let i = 0; i < active.length; i += 1) {
      const { item } = active[i];
      const next = results[i];
      if (item.lastResult !== next) {
        item.lastResult = next;
        item.setResult(next);
      }
    }

    const t1 = typeof performance !== 'undefined' ? performance.now() : Date.now();
    recordPerfStats(t1 - t0, active.length, hits, misses);
  }, []);

  const scheduleMeasure = useCallback(() => {
    if (rafRef.current) return;
    if (typeof window !== 'undefined' && typeof window.requestAnimationFrame === 'function') {
      rafRef.current = window.requestAnimationFrame(flushMeasure);
    } else {
      rafRef.current = 1;
      Promise.resolve().then(flushMeasure);
    }
  }, [flushMeasure]);

  // Set up ONE ResizeObserver and ONE IntersectionObserver for the whole list.
  useEffect(() => {
    const container = containerRef.current;
    if (!container) return undefined;

    let ro = null;
    if (typeof ResizeObserver === 'function') {
      ro = new ResizeObserver(() => {
        scheduleMeasure();
      });
      ro.observe(container);
      adjustObserverCount(1);
    }

    let io = null;
    if (typeof IntersectionObserver === 'function') {
      const scrollRoot = findScrollRoot(container);
      io = new IntersectionObserver(
        (entries) => {
          let changed = false;
          for (const entry of entries) {
            const id = elToIdRef.current.get(entry.target);
            if (!id) continue;
            if (entry.isIntersecting) {
              if (!visibleIdsRef.current.has(id)) {
                visibleIdsRef.current.add(id);
                changed = true;
              }
            } else if (visibleIdsRef.current.has(id)) {
              visibleIdsRef.current.delete(id);
            }
          }
          if (changed) scheduleMeasure();
        },
        { root: scrollRoot, rootMargin: '240px 0px' },
      );
      ioRef.current = io;
      for (const item of itemsRef.current.values()) {
        if (item.el) io.observe(item.el);
      }
    } else {
      for (const id of itemsRef.current.keys()) visibleIdsRef.current.add(id);
      scheduleMeasure();
    }

    const unsubFont = subscribeFontChanges(() => {
      scheduleMeasure();
    });

    return () => {
      if (rafRef.current && typeof window !== 'undefined' && typeof window.cancelAnimationFrame === 'function') {
        window.cancelAnimationFrame(rafRef.current);
      }
      rafRef.current = 0;
      if (ro) {
        ro.disconnect();
        adjustObserverCount(-1);
      }
      if (io) {
        io.disconnect();
        ioRef.current = null;
      }
      unsubFont();
      visibleIdsRef.current.clear();
    };
  }, [scheduleMeasure]);

  const api = useMemo(
    () => ({
      register(entry) {
        const id = nextIdRef.current++;
        itemsRef.current.set(id, entry);
        if (entry.el) {
          elToIdRef.current.set(entry.el, id);
          if (ioRef.current) {
            ioRef.current.observe(entry.el);
          } else {
            visibleIdsRef.current.add(id);
            scheduleMeasure();
          }
        }
        return id;
      },
      update(id, patch) {
        const existing = itemsRef.current.get(id);
        if (!existing) return;
        const prevEl = existing.el;
        Object.assign(existing, patch);
        if (patch.el && patch.el !== prevEl) {
          if (prevEl && ioRef.current) ioRef.current.unobserve(prevEl);
          elToIdRef.current.set(patch.el, id);
          if (ioRef.current) ioRef.current.observe(patch.el);
        }
        if (visibleIdsRef.current.has(id)) {
          scheduleMeasure();
        }
      },
      unregister(id) {
        const existing = itemsRef.current.get(id);
        if (existing?.el && ioRef.current) {
          ioRef.current.unobserve(existing.el);
        }
        itemsRef.current.delete(id);
        visibleIdsRef.current.delete(id);
      },
    }),
    [scheduleMeasure],
  );

  return html`
    <${FileNameListContext.Provider} value=${api}>
      <${as} ref=${containerRef} class=${className} ...${rest}>
        ${children}
      <//>
    <//>
  `;
}

// ── FileName (list item name) & FileNameSingle (standalone header/viewer title) ──────────────

export function FileName({
  name,
  before = null,
  after = null,
  lines = undefined,
  dim = undefined,
  highlight = '',
  className = '',
  as = 'span',
  titleOnHover = true,
  ...rest
}) {
  const listApi = useContext(FileNameListContext);
  const elRef = useRef(null);
  const idRef = useRef(null);
  const [result, setResult] = useState(null);
  const fullName = String(name ?? '');

  // Register once with the enclosing `<FileNameList>` coordinator.
  useLayoutEffect(() => {
    if (!listApi) return undefined;
    const id = listApi.register({
      el: elRef.current,
      name: fullName,
      before,
      after,
      lines,
      dim,
      className,
      setResult,
      lastResult: null,
    });
    idRef.current = id;
    return () => {
      listApi.unregister(id);
      idRef.current = null;
    };
  }, [listApi]);

  // Keep entry metadata fresh whenever props (name, neighbours, sort/filter, lines, dim) change.
  useLayoutEffect(() => {
    if (!listApi || idRef.current == null) return;
    listApi.update(idRef.current, {
      el: elRef.current,
      name: fullName,
      before,
      after,
      lines,
      dim,
      className,
    });
  }, [listApi, fullName, before, after, lines, dim, className]);

  // Fallback when rendered outside a `<FileNameList>` (e.g. standalone slot).
  useEffect(() => {
    if (listApi) return undefined;
    const el = elRef.current;
    if (!el) return undefined;

    const compute = () => {
      const rawWidth = el.getBoundingClientRect().width;
      if (!(rawWidth > 1)) return;
      const width = Math.max(1, Math.floor(rawWidth) - 1);
      const spec = readElementFontSpec(el);
      const measure = createFontMeasurer(spec.font, spec.letterSpacing);
      const { result: next } = cachedFitName(fullName, before, after, measure, width, lines ?? 2, {
        dim: dim ?? true,
        fontKey: `${spec.font}|${spec.letterSpacing}`,
      });
      setResult(next);
    };

    compute();
    let ro = null;
    if (typeof ResizeObserver === 'function') {
      ro = new ResizeObserver(compute);
      ro.observe(el);
    }
    const unsubFont = subscribeFontChanges(compute);
    return () => {
      if (ro) ro.disconnect();
      unsubFont();
    };
  }, [listApi, fullName, before, after, lines, dim, className]);

  const cls = ['fn-slot', className].filter(Boolean).join(' ');
  return html`
    <${as}
      ref=${elRef}
      class=${cls}
      title=${titleOnHover ? fullName : undefined}
      data-fn-ready=${result ? '1' : '0'}
      ...${rest}
    >
      ${result
        ? renderFittedLines(result, highlight)
        : html`<span class="fn-pending" dir="auto">${fullName}</span>`}
    <//>
  `;
}

/**
 * Single standalone name slot (viewer header, music title, split pane folder heading): always uses
 * `fitName` rule 2 without dimming and watches its own width + font changes.
 */
export function FileNameSingle({
  name,
  lines = 1,
  className = '',
  as = 'span',
  titleOnHover = true,
  ...rest
}) {
  return html`
    <${FileName}
      as=${as}
      name=${name}
      before=${null}
      after=${null}
      lines=${lines}
      dim=${false}
      className=${className}
      titleOnHover=${titleOnHover}
      ...${rest}
    />
  `;
}
