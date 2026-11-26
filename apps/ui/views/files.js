// Files tab views: Home (Pinned, Kept in sync, Machines, More), Folder (list & grid with batched
// thumbnails, 5 000-entry paging, swipe actions, long-press selection), Search, Favourites, Recent,
// and Kept-in-sync folder detail.

import {
  html,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from '../vendor/preact-htm.js';
import { Icon } from '../icons.js';
import {
  baseFileName,
  changedPresetCutoff,
  computeVirtualGridWindow,
  createThumbQueue,
  createVirtualListLayout,
  detectFileCapabilities,
  entryCategory,
  entryIconName,
  filterEntries,
  folderDisplayName,
  formatFolderTotal,
  formatPathForDisplay,
  formatShowingCount,
  isThumbEligible,
  parentFilePath,
  plainCategoryLabel,
  selectionSummary,
  shouldDefaultToGrid,
  sizePresetBounds,
  sortDescription,
  sortEntries,
  splitFileCrumbs,
} from '../lib/files.js';
import {
  readFolderViewMode,
  readRecentFiles,
  recordRecentFile,
  writeFolderViewMode,
} from '../lib/files-store.js';
import { plainErrorMessage } from '../lib/errors.js';
import { createCoalescedRefetch, STALE_AFTER_MS } from '../lib/folder-changes.js';
import { formatBytes, formatElapsed, formatRelativeTime, primaryDisk } from '../lib/format.js';
import {
  isRealMachineName,
  migrateLocalFavouritesOnNode,
  normalizeFavouritesMachine,
  PENDING_LOCAL_MACHINE_NAME,
} from '../lib/local-machine.js';
import { routeHref } from '../lib/route.js';
import { thumbCacheKey } from '../lib/thumb-cache.js';
import { Notice, OlderMachineNote, ProgressBar, SwipeRow } from './common.js';
import {
  FolderMoreSheet,
  ItemSheet,
  NewFolderSheet,
  RenameSheet,
  SearchFiltersSheet,
  SendToSheet,
  SortSheet,
  SyncAddMachineSheet,
  SyncBinSheet,
  SyncConflictsSheet,
} from './files-sheets.js';
import { FileName, FileNameList, FileNameSingle } from './file-name.js';
import { FilesViewer } from './files-viewer.js';
import { SplitView } from './split-view.js';

const PAGE_SIZE = 200;
const NEXT_PAGE_SIZE = 500;
const RENDER_STEP = 120;
const LONG_PRESS_MS = 450;
const LONG_PRESS_MOVE_TOLERANCE_PX = 8;
const VIRTUAL_MIN_ITEMS = 60;
const VIRTUAL_GRID_MIN_ITEMS = 60;
const INITIAL_ROW_HEIGHT_FALLBACK_PX = 56;

function findScrollParent(el) {
  let node = el?.parentElement || null;
  while (node && node !== document.body && node !== document.documentElement) {
    const oy = window.getComputedStyle ? window.getComputedStyle(node).overflowY : '';
    if (oy === 'auto' || oy === 'scroll' || oy === 'overlay') return node;
    node = node.parentElement;
  }
  return document.scrollingElement || document.documentElement;
}

function readViewportMetrics(containerEl) {
  const scroller = findScrollParent(containerEl);
  const isDoc =
    !scroller || scroller === document.documentElement || scroller === document.body;
  const listRect = containerEl.getBoundingClientRect();
  const listStyle = window.getComputedStyle ? window.getComputedStyle(containerEl) : null;
  const listBorderTop = listStyle ? parseFloat(listStyle.borderTopWidth) || 0 : 0;
  let scrollerTop = 0;
  let scrollTop = 0;
  let viewportHeight = typeof window !== 'undefined' ? window.innerHeight || 720 : 720;
  let padTop = 0;
  let padBottom = 0;

  if (isDoc) {
    scrollTop = window.scrollY || scroller?.scrollTop || 0;
    viewportHeight = window.innerHeight || 720;
  } else {
    const sr = scroller.getBoundingClientRect();
    scrollerTop = sr.top;
    scrollTop = scroller.scrollTop;
    viewportHeight = scroller.clientHeight || sr.height || window.innerHeight || 720;
    const cs = window.getComputedStyle ? window.getComputedStyle(scroller) : null;
    if (cs) {
      padTop = parseFloat(cs.paddingTop) || 0;
      padBottom =
        (parseFloat(cs.paddingBottom) || 0) + (parseFloat(cs.scrollPaddingBottom) || 0);
    }
  }

  const listTop = listRect.top - scrollerTop + scrollTop + listBorderTop;
  const scrollOffset = scrollTop - listTop;

  let stickyTop = 0;
  const whereEl = !isDoc
    ? scroller.querySelector('.where')
    : document.querySelector('.where');
  if (whereEl) {
    const wr = whereEl.getBoundingClientRect();
    if (wr.height > 0 && wr.top <= scrollerTop + padTop + 8) {
      stickyTop = wr.height;
    }
  }

  return {
    scroller,
    isDoc,
    scrollTop,
    scrollerTop,
    listTop,
    scrollOffset,
    viewportHeight,
    padTop,
    padBottom,
    stickyTop,
    listWidth: containerEl.clientWidth || listRect.width || 0,
  };
}

/**
 * Measures the live one-line row height from mounted `.vlist-item` rows (never a hardcoded
 * constant). Prefers a measured single-line fitted row, then a pending one-line row, and finally
 * derives the exact single-line height from a multi-line row using live thumb/body/padding/border
 * geometry so folders where every name wraps still estimate unmeasured rows with live metrics.
 */
function measureLiveOneLineHeight(listEl) {
  if (!listEl) return null;
  let readyZeroIdxHeight = null;
  let pendingHeight = null;
  let borderWidthSample = null;
  let multiLineSample = null;

  for (const child of listEl.children) {
    if (!child.classList?.contains('vlist-item')) continue;
    const rectH = child.getBoundingClientRect().height;
    if (!(rectH > 1)) continue;
    const idxAttr = child.dataset?.vlistIndex;
    const isFirst = idxAttr === '0';
    if (!isFirst && borderWidthSample === null) {
      const bw = window.getComputedStyle ? parseFloat(window.getComputedStyle(child).borderTopWidth) : 0;
      if (Number.isFinite(bw)) borderWidthSample = bw;
    }

    const slot = child.querySelector('.fn-slot');
    if (slot) {
      const ready = slot.getAttribute('data-fn-ready') === '1';
      if (ready) {
        const lines = slot.querySelectorAll('.fn-line');
        if (lines.length === 1) {
          if (!isFirst) return rectH;
          if (readyZeroIdxHeight === null) readyZeroIdxHeight = rectH;
        } else if (lines.length > 1 && !multiLineSample) {
          multiLineSample = { child, lines, isFirst };
        }
      } else if (!isFirst && pendingHeight === null) {
        pendingHeight = rectH;
      }
    } else if (!isFirst && pendingHeight === null) {
      pendingHeight = rectH;
    }
  }

  const divider = borderWidthSample ?? 1;
  if (readyZeroIdxHeight !== null) return readyZeroIdxHeight + divider;
  if (pendingHeight !== null) return pendingHeight;

  if (multiLineSample) {
    const { child, lines, isFirst } = multiLineSample;
    const rowEl = child.querySelector('.row');
    const bodyEl = rowEl?.querySelector('.body');
    const thumbEl = rowEl?.querySelector('.thumb');
    const lineH = lines[0]?.getBoundingClientRect().height || 0;
    if (rowEl && bodyEl && lineH > 0) {
      const cs = window.getComputedStyle(rowEl);
      const padY = (parseFloat(cs.paddingTop) || 0) + (parseFloat(cs.paddingBottom) || 0);
      const minH = parseFloat(cs.minHeight) || 0;
      const thumbH = thumbEl ? thumbEl.getBoundingClientRect().height : 0;
      const bodyH = bodyEl.getBoundingClientRect().height;
      const oneLineBodyH = Math.max(0, bodyH - Math.max(0, lines.length - 1) * lineH);
      const itemBorder = isFirst
        ? divider
        : parseFloat(window.getComputedStyle(child).borderTopWidth) || 0;
      const derived = Math.max(minH, Math.max(thumbH, oneLineBodyH) + padY) + itemBorder;
      if (derived > 1) return derived;
    }
  }

  return null;
}

function setSpacerHeight(el, heightPx) {
  if (!el) return;
  const h = Math.max(0, Math.round(heightPx));
  el.style.height = `${h}px`;
  el.dataset.empty = h === 0 ? 'true' : 'false';
}

function VirtualFileList({
  items,
  renderRow,
  resetKey,
  onNearEnd = null,
  returnTarget = null,
  onReturnHandled = null,
  lines = 2,
}) {
  const listRef = useRef(null);
  const topSpacerRef = useRef(null);
  const bottomSpacerRef = useRef(null);
  const layoutRef = useRef(null);
  if (!layoutRef.current) {
    layoutRef.current = createVirtualListLayout({
      estimatedHeight: INITIAL_ROW_HEIGHT_FALLBACK_PX,
    });
  }
  const layout = layoutRef.current;

  const prevResetKeyRef = useRef(resetKey);
  const lastListWidthRef = useRef(0);
  const lastScrollerBoxRef = useRef({ w: 0, h: 0, clientH: 0, padBottom: 0 });
  const observedRowsRef = useRef(new Set());
  const rowRoRef = useRef(null);
  const nearEndForCountRef = useRef(-1);
  const handledReturnTokenRef = useRef(0);
  const onNearEndRef = useRef(onNearEnd);
  onNearEndRef.current = onNearEnd;

  const total = items.length;
  const totalRef = useRef(total);
  totalRef.current = total;
  const virtualized = total > VIRTUAL_MIN_ITEMS;

  // Keep the layout index synchronized with `items` during render so offsets and window bounds are
  // always valid before paint.
  const resetChanged = prevResetKeyRef.current !== resetKey;
  if (resetChanged) {
    prevResetKeyRef.current = resetKey;
    layout.resetMeasurements();
    lastListWidthRef.current = 0;
    nearEndForCountRef.current = -1;
  }

  const itemKeys = useMemo(() => items.map((e, i) => e?.path || String(i)), [items]);
  layout.syncItems(itemKeys);

  const [win, setWin] = useState(() =>
    layout.computeWindow({
      scrollOffset: 0,
      viewportHeight: typeof window !== 'undefined' ? window.innerHeight || 720 : 720,
      overscanPx: 640,
    }),
  );

  // Clamp/recompute synchronously if `resetKey` changed or `total` changed before effects run.
  const effectiveWin = useMemo(() => {
    if (!virtualized) {
      return { start: 0, end: total, topPad: 0, bottomPad: 0, totalHeight: 0 };
    }
    if (resetChanged || (win.end === 0 && total > 0)) {
      const vp = typeof window !== 'undefined' ? window.innerHeight || 720 : 720;
      return layout.computeWindow({
        scrollOffset: 0,
        viewportHeight: vp,
        overscanPx: Math.max(600, Math.round(vp * 0.85)),
      });
    }
    const start = Math.max(0, Math.min(total, win.start));
    const end = Math.max(start, Math.min(total, win.end));
    const totalHeight = layout.getTotalHeight();
    return {
      start,
      end,
      topPad: Math.round(layout.getOffset(start)),
      bottomPad: Math.round(Math.max(0, totalHeight - layout.getOffset(end))),
      totalHeight: Math.round(totalHeight),
    };
  }, [virtualized, total, resetChanged, win.start, win.end, itemKeys]);

  const winRef = useRef(effectiveWin);
  winRef.current = effectiveWin;

  const syncWindowFromDom = useCallback(
    ({ forceImmediate = false } = {}) => {
      const listEl = listRef.current;
      const count = totalRef.current;
      if (!listEl || count <= VIRTUAL_MIN_ITEMS) return;
      const m = readViewportMetrics(listEl);
      const overscanPx = Math.max(600, Math.round(m.viewportHeight * 0.85));
      const minBufferPx = forceImmediate ? 0 : Math.max(280, Math.round(overscanPx * 0.45));

      const next = layout.computeWindow({
        scrollOffset: m.scrollOffset,
        viewportHeight: m.viewportHeight,
        overscanPx,
        minBufferPx,
        prevWindow: forceImmediate ? null : winRef.current,
      });

      if (typeof onNearEndRef.current === 'function' && count > 0) {
        const preloadMargin = Math.max(400, Math.round(m.viewportHeight * 0.75));
        const nearBottom =
          m.scrollOffset + m.viewportHeight + preloadMargin >= layout.getTotalHeight();
        if (nearBottom && nearEndForCountRef.current !== count) {
          nearEndForCountRef.current = count;
          onNearEndRef.current();
        }
      }

      const cur = winRef.current;
      if (next.start !== cur.start || next.end !== cur.end) {
        winRef.current = next;
        setWin(next);
      } else if (next.topPad !== cur.topPad || next.bottomPad !== cur.bottomPad) {
        winRef.current = next;
        setSpacerHeight(topSpacerRef.current, next.topPad);
        setSpacerHeight(bottomSpacerRef.current, next.bottomPad);
      }
    },
    [layout],
  );

  const measureMountedAndStabilize = useCallback(
    (targetElements = null) => {
      const listEl = listRef.current;
      if (!listEl || totalRef.current <= VIRTUAL_MIN_ITEMS) return;
      const m = readViewportMetrics(listEl);
      const unobstructedTop = m.scrollerTop + m.padTop + m.stickyTop;

      // If list width changed (window resize, orientation change, split-pane resize), previously
      // measured heights for wrapped names may no longer match. Keep the visible anchor pinned.
      if (
        m.listWidth > 1 &&
        lastListWidthRef.current > 1 &&
        Math.abs(m.listWidth - lastListWidthRef.current) > 1
      ) {
        let anchorKey = null;
        let anchorVisualTop = 0;
        for (const child of listEl.children) {
          if (!child.classList?.contains('vlist-item')) continue;
          const r = child.getBoundingClientRect();
          if (r.height > 0 && r.top >= unobstructedTop - 0.5) {
            anchorKey = child.dataset?.vlistKey || null;
            anchorVisualTop = r.top - m.scrollerTop;
            break;
          }
        }
        lastListWidthRef.current = m.listWidth;
        const liveOneLine = measureLiveOneLineHeight(listEl);
        layout.resetMeasurements({ estimatedHeight: liveOneLine });

        const allUpdates = [];
        for (const child of listEl.children) {
          if (!child.classList?.contains('vlist-item')) continue;
          const idx = Number(child.dataset?.vlistIndex);
          const key = child.dataset?.vlistKey;
          const h = child.getBoundingClientRect().height;
          if (Number.isInteger(idx) && key !== undefined && h > 1) {
            allUpdates.push({ index: idx, key, height: h });
          }
        }
        layout.applyMeasurements({
          updates: allUpdates,
          estimatedHeight: liveOneLine,
          scrollOffset: 0,
        });

        const cur = winRef.current;
        const totalH = layout.getTotalHeight();
        setSpacerHeight(topSpacerRef.current, layout.getOffset(cur.start));
        setSpacerHeight(bottomSpacerRef.current, totalH - layout.getOffset(cur.end));

        if (anchorKey !== null && m.scrollTop > 1) {
          const anchorIdx = layout.indexOfKey(anchorKey);
          if (anchorIdx >= 0) {
            const nextTop = Math.max(0, m.listTop + layout.getOffset(anchorIdx) - anchorVisualTop);
            if (Math.abs(nextTop - m.scrollTop) >= 0.5) {
              if (m.isDoc) window.scrollTo(0, nextTop);
              else m.scroller.scrollTop = nextTop;
            }
          }
        }
        syncWindowFromDom({ forceImmediate: true });
        return;
      }

      if (lastListWidthRef.current === 0 && m.listWidth > 1) {
        lastListWidthRef.current = m.listWidth;
      }

      const liveOneLine = measureLiveOneLineHeight(listEl);
      const updates = [];
      const source = targetElements || listEl.children;
      for (const child of source) {
        if (!child || !child.isConnected || !child.classList?.contains('vlist-item')) continue;
        const idx = Number(child.dataset?.vlistIndex);
        const key = child.dataset?.vlistKey;
        const h = child.getBoundingClientRect().height;
        if (Number.isInteger(idx) && key !== undefined && h > 1) {
          updates.push({ index: idx, key, height: h });
        }
      }

      // Anchor on the first row visible below any sticky header / top padding when scrolled away
      // from the top so rows off-screen OR covered by the sticky breadcrumb bar absorb wrap deltas
      // into scrollTop without shifting visible rows below them.
      const anchorScrollOffset =
        m.scrollTop > 1 ? Math.max(0, m.scrollOffset + m.padTop + m.stickyTop) : 0;

      const cur = winRef.current;
      const res = layout.applyMeasurements({
        updates,
        estimatedHeight: liveOneLine,
        scrollOffset: anchorScrollOffset,
      });
      if (!res.changed) return;

      const nextTopPad = Math.round(layout.getOffset(cur.start));
      const nextBottomPad = Math.round(Math.max(0, res.totalHeight - layout.getOffset(cur.end)));
      setSpacerHeight(topSpacerRef.current, nextTopPad);
      setSpacerHeight(bottomSpacerRef.current, nextBottomPad);
      winRef.current = {
        ...cur,
        topPad: nextTopPad,
        bottomPad: nextBottomPad,
        totalHeight: Math.round(res.totalHeight),
      };

      // Only shift scrollTop when the anchor row is mounted in the current window so DOM row
      // height deltas plus spacer deltas match `res.scrollDelta` identically.
      const anchorMounted = res.anchorIndex >= cur.start && res.anchorIndex < cur.end;
      if (anchorMounted && Math.abs(res.scrollDelta) >= 0.5) {
        if (m.isDoc) {
          window.scrollBy(0, res.scrollDelta);
        } else {
          m.scroller.scrollTop = Math.max(0, m.scroller.scrollTop + res.scrollDelta);
        }
      }

      // If rows shrank or total height shifted enough to thin our overscan buffer, extend window
      // immediately so no blank gap can ever appear.
      syncWindowFromDom({ forceImmediate: false });
    },
    [layout, syncWindowFromDom],
  );

  // Sync spacers, ResizeObserver row subscriptions, and initial row measurements before paint.
  useLayoutEffect(() => {
    const listEl = listRef.current;
    if (!virtualized || !listEl) return;

    setSpacerHeight(topSpacerRef.current, effectiveWin.topPad);
    setSpacerHeight(bottomSpacerRef.current, effectiveWin.bottomPad);

    const rowRo = rowRoRef.current;
    if (rowRo) {
      const nextObserved = new Set();
      for (const child of listEl.children) {
        if (!child.classList?.contains('vlist-item')) continue;
        nextObserved.add(child);
        if (!observedRowsRef.current.has(child)) {
          rowRo.observe(child);
        }
      }
      for (const prev of observedRowsRef.current) {
        if (!nextObserved.has(prev)) {
          rowRo.unobserve(prev);
        }
      }
      observedRowsRef.current = nextObserved;
    }

    measureMountedAndStabilize();
  }, [virtualized, effectiveWin.start, effectiveWin.end, effectiveWin.topPad, effectiveWin.bottomPad, itemKeys, resetKey, measureMountedAndStabilize]);

  // Attach scroll listener, scroller ResizeObserver (tracks dynamic viewport height AND bottom
  // padding/inset changes), and ONE shared ResizeObserver for mounted `.vlist-item` rows.
  useEffect(() => {
    const listEl = listRef.current;
    if (!virtualized || !listEl) return undefined;

    const scroller = findScrollParent(listEl);
    const isDoc =
      !scroller || scroller === document.documentElement || scroller === document.body;
    const scrollTarget = isDoc ? window : scroller;

    const onScroll = () => {
      syncWindowFromDom({ forceImmediate: false });
    };

    let rowRo = null;
    if (typeof ResizeObserver === 'function') {
      rowRo = new ResizeObserver((entries) => {
        const changedEls = [];
        for (const entry of entries) {
          if (entry.target && entry.target.isConnected) {
            changedEls.push(entry.target);
          }
        }
        if (changedEls.length > 0) {
          measureMountedAndStabilize(changedEls);
        }
      });
      rowRoRef.current = rowRo;
      observedRowsRef.current.clear();
      for (const child of listEl.children) {
        if (child.classList?.contains('vlist-item')) {
          rowRo.observe(child);
          observedRowsRef.current.add(child);
        }
      }
    }

    let scrollerRo = null;
    if (typeof ResizeObserver === 'function' && scroller) {
      scrollerRo = new ResizeObserver((entries) => {
        const m = readViewportMetrics(listEl);
        const prev = lastScrollerBoxRef.current;
        let scrollerContentH = prev.h;
        let scrollerContentW = prev.w;
        for (const entry of entries) {
          if (entry.target === scroller && entry.contentRect) {
            scrollerContentW = entry.contentRect.width;
            scrollerContentH = entry.contentRect.height;
          }
        }
        const widthChanged =
          m.listWidth > 1 &&
          lastListWidthRef.current > 1 &&
          Math.abs(m.listWidth - lastListWidthRef.current) > 1;
        if (
          !widthChanged &&
          Math.abs(scrollerContentW - prev.w) <= 0.5 &&
          Math.abs(scrollerContentH - prev.h) <= 0.5 &&
          Math.abs(m.viewportHeight - prev.clientH) <= 0.5 &&
          Math.abs(m.padBottom - prev.padBottom) <= 0.5
        ) {
          return;
        }
        lastScrollerBoxRef.current = {
          w: scrollerContentW,
          h: scrollerContentH,
          clientH: m.viewportHeight,
          padBottom: m.padBottom,
        };
        if (widthChanged) {
          measureMountedAndStabilize();
        } else {
          syncWindowFromDom({ forceImmediate: true });
        }
      });
      scrollerRo.observe(scroller);
      if (scroller !== listEl) {
        scrollerRo.observe(listEl);
      }
    }

    const onViewportResize = () => {
      syncWindowFromDom({ forceImmediate: true });
    };

    scrollTarget.addEventListener('scroll', onScroll, { passive: true });
    window.addEventListener('resize', onViewportResize, { passive: true });
    if (window.visualViewport) {
      window.visualViewport.addEventListener('resize', onViewportResize, { passive: true });
    }

    syncWindowFromDom({ forceImmediate: false });

    return () => {
      scrollTarget.removeEventListener('scroll', onScroll);
      window.removeEventListener('resize', onViewportResize);
      if (window.visualViewport) {
        window.visualViewport.removeEventListener('resize', onViewportResize);
      }
      if (scrollerRo) scrollerRo.disconnect();
      if (rowRo) {
        rowRo.disconnect();
        rowRoRef.current = null;
        observedRowsRef.current.clear();
      }
    };
  }, [virtualized, resetKey, syncWindowFromDom, measureMountedAndStabilize]);

  // Re-sync window whenever new pages append to `items` without recreating observers.
  useEffect(() => {
    if (!virtualized) return;
    syncWindowFromDom({ forceImmediate: false });
  }, [virtualized, total, syncWindowFromDom]);

  // "Scroll to the item I came back from" (viewer sibling swipe close or Back from subfolder).
  useEffect(() => {
    if (!returnTarget || !returnTarget.path || returnTarget.token <= handledReturnTokenRef.current) {
      return undefined;
    }
    const listEl = listRef.current;
    if (!listEl) return undefined;
    const targetIdx = layout.indexOfKey(returnTarget.path);
    if (targetIdx < 0) return undefined;

    handledReturnTokenRef.current = returnTarget.token;
    if (typeof onReturnHandled === 'function') {
      onReturnHandled(returnTarget.token);
    }

    if (!virtualized) {
      const child = listEl.children[targetIdx];
      const anchor = child?.querySelector?.('a.row') || child;
      if (anchor && typeof anchor.scrollIntoView === 'function') {
        const m = readViewportMetrics(listEl);
        const r = anchor.getBoundingClientRect();
        const topEdge = m.scrollerTop + m.padTop + m.stickyTop;
        const bottomEdge = m.scrollerTop + m.viewportHeight - m.padBottom;
        if (!returnTarget.onlyIfOffscreen || r.top < topEdge - 1 || r.bottom > bottomEdge + 1) {
          anchor.scrollIntoView({ block: 'center', behavior: 'auto' });
        }
        if (
          !document.querySelector('.viewer, .scrim') &&
          typeof anchor.focus === 'function' &&
          (!document.activeElement || document.activeElement === document.body)
        ) {
          try {
            anchor.focus({ preventScroll: true });
          } catch {
            // Ignore focus failures.
          }
        }
      }
      return undefined;
    }

    const m = readViewportMetrics(listEl);
    const targetScrollTop = layout.computeScrollTargetForIndex(targetIdx, {
      listTop: m.listTop,
      viewportHeight: m.viewportHeight,
      topInset: m.padTop + m.stickyTop,
      bottomInset: m.padBottom,
      currentScrollTop: m.scrollTop,
      onlyIfOffscreen: Boolean(returnTarget.onlyIfOffscreen),
      align: 'center',
    });

    if (targetScrollTop !== null) {
      const cur = winRef.current;
      setSpacerHeight(topSpacerRef.current, layout.getOffset(cur.start));
      setSpacerHeight(bottomSpacerRef.current, layout.getTotalHeight() - layout.getOffset(cur.end));
      if (m.isDoc) window.scrollTo(0, targetScrollTop);
      else m.scroller.scrollTop = targetScrollTop;
      syncWindowFromDom({ forceImmediate: true });
    }

    // Refine scroll once the target row has mounted and its name has laid out, and restore focus.
    let raf2 = 0;
    const raf1 = window.requestAnimationFrame(() => {
      raf2 = window.requestAnimationFrame(() => {
        const el = listRef.current;
        if (!el) return;
        const rowWrap = el.querySelector(`.vlist-item[data-vlist-index="${targetIdx}"]`);
        if (!rowWrap) return;
        const m2 = readViewportMetrics(el);
        const r = rowWrap.getBoundingClientRect();
        const topEdge = m2.scrollerTop + m2.padTop + m2.stickyTop;
        const bottomEdge = m2.scrollerTop + m2.viewportHeight - m2.padBottom;
        if (r.top < topEdge - 1 || r.bottom > bottomEdge + 1) {
          const usable = Math.max(1, bottomEdge - topEdge);
          const desiredTop = topEdge + Math.max(0, (usable - r.height) / 2);
          const adjust = r.top - desiredTop;
          if (Math.abs(adjust) >= 1) {
            if (m2.isDoc) window.scrollBy(0, adjust);
            else m2.scroller.scrollTop = Math.max(0, m2.scroller.scrollTop + adjust);
            syncWindowFromDom({ forceImmediate: true });
          }
        }
        const focusEl = rowWrap.querySelector('a.row');
        if (
          focusEl &&
          !document.querySelector('.viewer, .scrim') &&
          (!document.activeElement ||
            document.activeElement === document.body ||
            el.contains(document.activeElement))
        ) {
          try {
            focusEl.focus({ preventScroll: true });
          } catch {
            // Ignore focus failures.
          }
        }
      });
    });

    return () => {
      window.cancelAnimationFrame(raf1);
      if (raf2) window.cancelAnimationFrame(raf2);
    };
  }, [returnTarget, virtualized, layout, syncWindowFromDom, total]);

  if (!virtualized) {
    return html`
      <${FileNameList} className="list" lines=${lines} containerRef=${listRef}>
        ${items.map((entry, idx) => renderRow(entry, idx, items))}
      <//>
    `;
  }

  const mounted = [];
  for (let i = effectiveWin.start; i < effectiveWin.end; i += 1) {
    const entry = items[i];
    const key = itemKeys[i];
    mounted.push(html`
      <div key=${key} class="vlist-item" data-vlist-index=${i} data-vlist-key=${key}>
        ${renderRow(entry, i, items)}
      </div>
    `);
  }

  return html`
    <${FileNameList} className="list vlist" lines=${lines} containerRef=${listRef}>
      <div
        key="__vlist_top"
        ref=${topSpacerRef}
        class="vlist-spacer"
        data-empty=${effectiveWin.topPad === 0 ? 'true' : 'false'}
        aria-hidden="true"
      />
      ${mounted}
      <div
        key="__vlist_bottom"
        ref=${bottomSpacerRef}
        class="vlist-spacer"
        data-empty=${effectiveWin.bottomPad === 0 ? 'true' : 'false'}
        aria-hidden="true"
      />
    <//>
  `;
}

function measureGridGeometry(gridEl) {
  if (!gridEl || !window.getComputedStyle) return null;
  const cs = window.getComputedStyle(gridEl);
  const tracks = (cs.gridTemplateColumns || '').trim().split(/\s+/).filter(Boolean);
  const columns = Math.max(1, tracks.length || 3);
  const rowGap = parseFloat(cs.rowGap) || 0;
  const colGap = parseFloat(cs.columnGap) || rowGap;
  const firstCell = gridEl.querySelector('.cell');
  let rowHeight = firstCell ? firstCell.getBoundingClientRect().height : 0;
  if (!(rowHeight > 1)) {
    const gridW = gridEl.clientWidth || gridEl.getBoundingClientRect().width || 0;
    if (gridW > 1) {
      rowHeight = Math.max(24, (gridW - Math.max(0, columns - 1) * colGap) / columns);
    }
  }
  return rowHeight > 1 ? { columns, rowHeight, rowGap } : null;
}

function VirtualFileGrid({
  items,
  renderCell,
  resetKey,
  onNearEnd = null,
  returnTarget = null,
  onReturnHandled = null,
  lines = 3,
}) {
  const gridRef = useRef(null);
  const topSpacerRef = useRef(null);
  const bottomSpacerRef = useRef(null);
  const geomRef = useRef({ columns: 3, rowHeight: 124, rowGap: 3 });
  const nearEndForCountRef = useRef(-1);
  const handledReturnTokenRef = useRef(0);
  const onNearEndRef = useRef(onNearEnd);
  onNearEndRef.current = onNearEnd;

  const total = items.length;
  const totalRef = useRef(total);
  totalRef.current = total;
  const lastGridWidthRef = useRef(0);
  const virtualized = total > VIRTUAL_GRID_MIN_ITEMS;

  const [win, setWin] = useState(() =>
    computeVirtualGridWindow({
      total,
      columns: 3,
      rowHeight: 124,
      rowGap: 3,
      scrollOffset: 0,
      viewportHeight: typeof window !== 'undefined' ? window.innerHeight || 720 : 720,
      overscanRows: 4,
    }),
  );

  const winRef = useRef(win);
  winRef.current = win;

  const computeFromDom = useCallback(() => {
    const gridEl = gridRef.current;
    const count = totalRef.current;
    if (!gridEl || count <= VIRTUAL_GRID_MIN_ITEMS) return;
    const geom = measureGridGeometry(gridEl) || geomRef.current;
    geomRef.current = geom;
    const m = readViewportMetrics(gridEl);
    const next = computeVirtualGridWindow({
      total: count,
      columns: geom.columns,
      rowHeight: geom.rowHeight,
      rowGap: geom.rowGap,
      scrollOffset: m.scrollOffset,
      viewportHeight: m.viewportHeight,
      overscanRows: 4,
    });

    if (typeof onNearEndRef.current === 'function' && count > 0) {
      const preloadMargin = Math.max(400, Math.round(m.viewportHeight * 0.75));
      if (m.scrollOffset + m.viewportHeight + preloadMargin >= next.totalHeight) {
        if (nearEndForCountRef.current !== count) {
          nearEndForCountRef.current = count;
          onNearEndRef.current();
        }
      }
    }

    const cur = winRef.current;
    if (
      next.startIndex !== cur.startIndex ||
      next.endIndex !== cur.endIndex ||
      next.topPad !== cur.topPad ||
      next.bottomPad !== cur.bottomPad
    ) {
      winRef.current = next;
      setWin(next);
    }
  }, []);

  useEffect(() => {
    nearEndForCountRef.current = -1;
  }, [resetKey]);

  const startIndex = virtualized ? Math.min(win.startIndex, total) : 0;
  const endIndex = virtualized ? Math.max(startIndex, Math.min(win.endIndex, total)) : total;

  useLayoutEffect(() => {
    if (!virtualized) return;
    setSpacerHeight(topSpacerRef.current, win.topPad);
    setSpacerHeight(bottomSpacerRef.current, win.bottomPad);
  }, [virtualized, win.topPad, win.bottomPad, startIndex, endIndex]);

  useEffect(() => {
    const gridEl = gridRef.current;
    if (!virtualized || !gridEl) return undefined;
    const scroller = findScrollParent(gridEl);
    const isDoc =
      !scroller || scroller === document.documentElement || scroller === document.body;
    const scrollTarget = isDoc ? window : scroller;

    const onUpdate = () => computeFromDom();
    scrollTarget.addEventListener('scroll', onUpdate, { passive: true });
    window.addEventListener('resize', onUpdate, { passive: true });
    let ro = null;
    if (typeof ResizeObserver === 'function') {
      ro = new ResizeObserver((entries) => {
        let relevant = false;
        for (const entry of entries) {
          if (entry.target === scroller) {
            relevant = true;
          } else if (entry.target === gridEl) {
            const w = entry.contentRect?.width ?? gridEl.clientWidth;
            if (Math.abs(w - lastGridWidthRef.current) > 0.5) {
              lastGridWidthRef.current = w;
              relevant = true;
            }
          }
        }
        if (relevant) onUpdate();
      });
      if (scroller) ro.observe(scroller);
      if (scroller !== gridEl) ro.observe(gridEl);
    }
    computeFromDom();
    return () => {
      scrollTarget.removeEventListener('scroll', onUpdate);
      window.removeEventListener('resize', onUpdate);
      if (ro) ro.disconnect();
    };
  }, [virtualized, resetKey, computeFromDom]);

  useEffect(() => {
    if (!virtualized) return;
    computeFromDom();
  }, [virtualized, total, computeFromDom]);

  useEffect(() => {
    if (!returnTarget || !returnTarget.path || returnTarget.token <= handledReturnTokenRef.current) {
      return;
    }
    const gridEl = gridRef.current;
    if (!gridEl) return;
    const idx = items.findIndex((e) => e?.path === returnTarget.path);
    if (idx < 0) return;
    handledReturnTokenRef.current = returnTarget.token;
    if (typeof onReturnHandled === 'function') {
      onReturnHandled(returnTarget.token);
    }
    if (!virtualized) {
      const cell = gridEl.children[idx];
      if (cell && typeof cell.scrollIntoView === 'function' && !returnTarget.onlyIfOffscreen) {
        cell.scrollIntoView({ block: 'center', behavior: 'auto' });
      }
      return;
    }
    const geom = measureGridGeometry(gridEl) || geomRef.current;
    const m = readViewportMetrics(gridEl);
    const row = Math.floor(idx / geom.columns);
    const pitch = geom.rowHeight + geom.rowGap;
    const itemTop = m.listTop + row * pitch;
    const itemBottom = itemTop + geom.rowHeight;
    const topEdge = m.scrollTop + m.padTop + m.stickyTop;
    const bottomEdge = m.scrollTop + Math.max(1, m.viewportHeight - m.padBottom);
    if (returnTarget.onlyIfOffscreen && itemTop >= topEdge - 1 && itemBottom <= bottomEdge + 1) {
      return;
    }
    const usable = Math.max(1, m.viewportHeight - m.padTop - m.stickyTop - m.padBottom);
    const targetTop = Math.max(0, Math.round(itemTop - m.padTop - m.stickyTop - Math.max(0, (usable - geom.rowHeight) / 2)));
    if (m.isDoc) window.scrollTo(0, targetTop);
    else m.scroller.scrollTop = targetTop;
    computeFromDom();
  }, [returnTarget, items, virtualized, computeFromDom]);

  if (!virtualized) {
    return html`
      <${FileNameList} className="grid" lines=${lines} containerRef=${gridRef}>
        ${items.map((entry, idx) => renderCell(entry, idx, items))}
      <//>
    `;
  }

  const cells = [];
  for (let i = startIndex; i < endIndex; i += 1) {
    cells.push(renderCell(items[i], i, items));
  }

  return html`
    <${FileNameList} className="grid vgrid" lines=${lines} containerRef=${gridRef}>
      <div
        key="__vgrid_top"
        ref=${topSpacerRef}
        class="vgrid-spacer"
        data-empty=${win.topPad === 0 ? 'true' : 'false'}
        aria-hidden="true"
      />
      ${cells}
      <div
        key="__vgrid_bottom"
        ref=${bottomSpacerRef}
        class="vgrid-spacer"
        data-empty=${win.bottomPad === 0 ? 'true' : 'false'}
        aria-hidden="true"
      />
    <//>
  `;
}

function useItemLongPress(onTrigger, { disabled = false } = {}) {
  const timerRef = useRef(null);
  const firedRef = useRef(false);
  const originRef = useRef(null);

  const cancel = useCallback(() => {
    if (timerRef.current) {
      clearTimeout(timerRef.current);
      timerRef.current = null;
    }
    originRef.current = null;
  }, []);

  useEffect(() => cancel, [cancel]);

  const start = useCallback(
    (e, payload) => {
      if (disabled || typeof onTrigger !== 'function') return;
      if (e && typeof e.button === 'number' && e.button !== 0) return;
      firedRef.current = false;
      originRef.current = e ? { x: e.clientX, y: e.clientY } : null;
      if (timerRef.current) clearTimeout(timerRef.current);
      timerRef.current = window.setTimeout(() => {
        firedRef.current = true;
        timerRef.current = null;
        originRef.current = null;
        onTrigger(payload);
      }, LONG_PRESS_MS);
    },
    [disabled, onTrigger],
  );

  const move = useCallback(
    (e) => {
      if (!timerRef.current || !originRef.current || !e) return;
      const dx = e.clientX - originRef.current.x;
      const dy = e.clientY - originRef.current.y;
      if (Math.hypot(dx, dy) > LONG_PRESS_MOVE_TOLERANCE_PX) {
        cancel();
      }
    },
    [cancel],
  );

  const consumeFired = useCallback(() => {
    if (firedRef.current) {
      firedRef.current = false;
      return true;
    }
    return false;
  }, []);

  const contextMenu = useCallback(
    (e, payload) => {
      if (disabled || typeof onTrigger !== 'function') return;
      e.preventDefault();
      cancel();
      if (firedRef.current) {
        return;
      }
      onTrigger(payload);
    },
    [disabled, onTrigger, cancel],
  );

  return { start, move, cancel, consumeFired, contextMenu };
}

const KIND_FILTER_LABELS = {
  image: 'Pictures',
  video: 'Videos',
  audio: 'Music',
  document: 'Documents',
  text: 'Text',
  archive: 'Archives',
};

function useMachineCaps(capsStore, machine, { fallbackTools = [], fallbackAllowedSet = null, link = null, enabled = true } = {}) {
  const [rev, setRev] = useState(0);

  useEffect(() => {
    if (capsStore) {
      return capsStore.subscribe(() => setRev((n) => n + 1));
    }
    if (typeof link?.onContentSupportChange === 'function') {
      return link.onContentSupportChange(() => setRev((n) => n + 1));
    }
    return undefined;
  }, [capsStore, link]);

  useEffect(() => {
    if (!capsStore || !machine || !enabled) return;
    if (!capsStore.isKnown(machine)) {
      capsStore.ensure(machine).catch(() => {});
    }
  }, [capsStore, machine, enabled, rev]);

  return useMemo(() => {
    if (capsStore && machine) {
      return {
        caps: capsStore.fileCapabilities(machine, { fallbackAllowedSet }),
        entry: capsStore.get(machine),
        missing: capsStore.missingFeatures(machine),
      };
    }
    return {
      caps: detectFileCapabilities({
        tools: fallbackTools,
        allowedSet: fallbackAllowedSet,
        link,
        machine: machine || null,
      }),
      entry: null,
      missing: [],
    };
  }, [capsStore, machine, fallbackTools, fallbackAllowedSet, link, rev]);
}

function buildOldFindGlob(query) {
  const trimmed = String(query || '').trim();
  if (!trimmed) return null;
  if (/[*?]/.test(trimmed)) return trimmed;
  const words = trimmed.split(/\s+/).filter(Boolean);
  const longest = words.reduce((a, b) => (b.length > a.length ? b : a), trimmed);
  let pattern = '';
  for (const ch of longest) {
    if (ch >= 'a' && ch <= 'z') {
      pattern += `[${ch}${ch.toUpperCase()}]`;
    } else if (ch >= 'A' && ch <= 'Z') {
      pattern += `[${ch.toLowerCase()}${ch}]`;
    } else if ('[]{}*?!\\'.includes(ch)) {
      pattern += `\\${ch}`;
    } else {
      pattern += ch;
    }
  }
  return `*${pattern}*`;
}

function normalizeFindEntries(rawList) {
  if (!Array.isArray(rawList)) return [];
  return rawList
    .filter((item) => item && typeof item === 'object' && item.path)
    .map((item) => (item.name ? item : { ...item, name: baseFileName(item.path) }));
}

function isMachineFilesRoot(filePath) {
  const p = String(filePath || '').trim().replace(/[/\\]+$/, '');
  if (!p || p === '~') return true;
  if (/^\/(?:home|Users)\/[^/]+$/.test(p)) return true;
  if (/^[A-Za-z]:\\Users\\[^\\]+$/i.test(p)) return true;
  if (p === '/' || /^[A-Za-z]:$/.test(p)) return true;
  return false;
}

function safeStorage() {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

function freeSpaceSub(machine) {
  const disk = primaryDisk(machine?.sys?.disks);
  const free = disk?.disk?.available != null ? `${formatBytes(disk.disk.available)} free` : '';
  if (machine?.isLocalComputer && machine?.name !== PENDING_LOCAL_MACHINE_NAME) {
    return free ? `This computer · ${free}` : 'This computer';
  }
  return free || 'On';
}

function offlineMachineSub(machine) {
  if (machine.lastSeenAt) {
    const ago = formatRelativeTime(machine.lastSeenAt);
    if (ago) return `Off · last on ${ago}`;
  }
  return 'Off';
}

function entrySubtitle(entry, technical = false) {
  if (entry.kind === 'dir') {
    const parts = [
      technical ? `dir${entry.mode ? ` · ${entry.mode}` : ''}` : 'Folder',
      entry.mtime ? formatRelativeTime(entry.mtime) : null,
    ].filter(Boolean);
    return parts.join(' · ');
  }
  const parts = [
    technical ? entry.mime || entryCategory(entry) : plainCategoryLabel(entry),
    entry.size != null
      ? technical
        ? `${formatBytes(entry.size)} (${entry.size} B)`
        : formatBytes(entry.size)
      : null,
    entry.mtime ? formatRelativeTime(entry.mtime) : null,
  ].filter(Boolean);
  return parts.join(' · ');
}

function joinMemberNames(members) {
  const names = (members || []).map((m) => m.machine).filter(Boolean);
  if (names.length === 0) return '';
  if (names.length === 1) return names[0];
  if (names.length === 2) return `${names[0]} and ${names[1]}`;
  return `${names.slice(0, -1).join(', ')} and ${names[names.length - 1]}`;
}

// ── Batched lazy thumbnail loader hooks ──────────────────────────────────────────────────────

function useBatchedThumbs({
  machine,
  entries,
  link,
  capsStore = null,
  enabled,
  allowAudio = false,
  thumbCache,
}) {
  const [, setTick] = useState(0);
  const queueRef = useRef(null);
  const observerRef = useRef(null);
  const observedElsRef = useRef(new Set());
  const entryMapRef = useRef(new Map());

  useEffect(() => {
    const map = new Map();
    for (const e of entries || []) {
      if (e?.path && isThumbEligible(e, { allowAudio: true })) map.set(e.path, e);
    }
    entryMapRef.current = map;
  }, [entries]);

  useEffect(() => () => {
    observedElsRef.current.clear();
  }, []);

  useEffect(() => {
    if (!enabled || !machine) {
      queueRef.current = null;
      return undefined;
    }
    const q = createThumbQueue({
      machine,
      link,
      capsStore,
      thumbCache,
      onUpdate: () => setTick((n) => n + 1),
    });
    queueRef.current = q;
    return () => {
      q.dispose();
      if (queueRef.current === q) queueRef.current = null;
    };
  }, [enabled, machine, link, capsStore, thumbCache]);

  const requestPath = useCallback(
    (entry) => {
      if (!enabled || !machine || !entry || !isThumbEligible(entry, { allowAudio })) return;
      queueRef.current?.enqueue({
        path: entry.path,
        mtime: entry.mtime || '',
        isAudio: entryCategory(entry) === 'audio',
      });
    },
    [enabled, machine, allowAudio],
  );

  // Immediately enqueue the first visible batch so initial paint fills thumbnails promptly.
  useEffect(() => {
    if (!enabled || !machine || !Array.isArray(entries)) return;
    let count = 0;
    const scanLimit = Math.min(entries.length, 60);
    for (let i = 0; i < scanLimit; i += 1) {
      const e = entries[i];
      if (count >= 24) break;
      if (isThumbEligible(e, { allowAudio })) {
        requestPath(e);
        count += 1;
      }
    }
  }, [enabled, machine, entries, allowAudio, link, capsStore, thumbCache, requestPath]);

  useEffect(() => {
    if (!enabled || typeof IntersectionObserver === 'undefined') return undefined;
    const obs = new IntersectionObserver(
      (items) => {
        for (const item of items) {
          if (!item.isIntersecting) continue;
          const p = item.target?.dataset?.thumbPath;
          if (!p) continue;
          const entry = entryMapRef.current.get(p);
          if (entry && isThumbEligible(entry, { allowAudio })) {
            requestPath(entry);
            obs.unobserve(item.target);
            observedElsRef.current.delete(item.target);
          } else if (!entry || entryCategory(entry) !== 'audio') {
            obs.unobserve(item.target);
            observedElsRef.current.delete(item.target);
          }
        }
      },
      { rootMargin: '200px 0px' },
    );
    observerRef.current = obs;
    for (const el of observedElsRef.current) {
      if (el.isConnected) obs.observe(el);
      else observedElsRef.current.delete(el);
    }
    return () => {
      obs.disconnect();
      observerRef.current = null;
    };
  }, [enabled, requestPath]);

  const observeEl = useCallback((el) => {
    if (!el) return;
    const observed = observedElsRef.current;
    if (observed.size > 120) {
      for (const prev of observed) {
        if (!prev.isConnected) {
          observerRef.current?.unobserve(prev);
          observed.delete(prev);
        }
      }
    }
    observed.add(el);
    if (observerRef.current) {
      observerRef.current.observe(el);
    }
  }, []);

  const getUrl = useCallback(
    (entry) => {
      if (!enabled || !machine || !entry) return null;
      const cached = thumbCache.get(thumbCacheKey(machine, entry.path, entry.mtime || '', 128));
      return cached && !cached.failed ? cached.url : null;
    },
    [enabled, machine, thumbCache],
  );

  return { getUrl, observeEl };
}

function useMultiMachineThumbs({ itemsByMachine, enabled, link, capsStore = null, thumbCache }) {
  const [, setTick] = useState(0);
  const [capsRev, setCapsRev] = useState(0);
  const queuesRef = useRef(new Map());

  useEffect(() => {
    if (!capsStore) return undefined;
    return capsStore.subscribe(() => setCapsRev((n) => n + 1));
  }, [capsStore]);

  useEffect(() => {
    const queues = queuesRef.current;
    return () => {
      for (const q of queues.values()) q.dispose();
      queues.clear();
    };
  }, [enabled, link, capsStore, thumbCache]);

  useEffect(() => {
    const queues = queuesRef.current;
    if (!enabled) {
      for (const q of queues.values()) q.dispose();
      queues.clear();
      return;
    }
    const activeMachines = new Set(itemsByMachine.keys());
    for (const [mName, q] of queues.entries()) {
      if (!activeMachines.has(mName)) {
        q.dispose();
        queues.delete(mName);
      }
    }
    for (const [mName, items] of itemsByMachine.entries()) {
      if (!items || items.length === 0) continue;
      let q = queues.get(mName);
      if (!q) {
        q = createThumbQueue({
          machine: mName,
          link,
          capsStore,
          thumbCache,
          onUpdate: () => setTick((n) => n + 1),
        });
        queues.set(mName, q);
      }
      for (const item of items) {
        q.enqueue(item);
      }
    }
  }, [itemsByMachine, enabled, link, capsStore, thumbCache, capsRev]);

  return useCallback(
    (machine, path, mtime = '') => {
      if (!enabled || !machine || !path) return null;
      const hit = thumbCache.get(thumbCacheKey(machine, path, mtime, 128));
      return hit && !hit.failed ? hit.url : null;
    },
    [enabled, thumbCache],
  );
}

// ── Thumbnail box (fixed size, zero layout shift) ────────────────────────────────────────────

function EntryThumb({ entry, thumbUrl, observeEl }) {
  if (entry.kind === 'dir') {
    return html`
      <span class="thumb folder" aria-hidden="true">
        <${Icon} name="files" />
      </span>
    `;
  }
  const canThumb = isThumbEligible(entry, { allowAudio: true });
  return html`
    <span
      class="thumb"
      ref=${canThumb ? observeEl : null}
      data-thumb-path=${canThumb ? entry.path : undefined}
      aria-hidden="true"
    >
      ${thumbUrl
        ? html`<img src=${thumbUrl} alt="" loading="lazy" decoding="async" />`
        : html`<${Icon} name=${entryIconName(entry)} />`}
    </span>
  `;
}

// ── Files First Screen (Home) ────────────────────────────────────────────────────────────────

export function FilesHome({
  machines,
  favourites,
  syncFolders,
  recentFiles,
  caps,
  capsStore = null,
  deviceNoun,
  link,
  thumbCache,
  wide,
  hideSearch = false,
  hideFirstLabel = false,
  activeView = null,
  activeMachine,
  activeSyncId,
  onNavigate,
  onOpenFile,
  onOpenItemSheet = null,
  onStartSearch,
}) {
  const pinned = useMemo(() => (favourites || []).filter((f) => f.pinned), [favourites]);
  const pinnedLongPress = useItemLongPress(
    useCallback(
      (item) => {
        if (!onOpenItemSheet || !item) return;
        onOpenItemSheet(item.machine, {
          name: item.label || baseFileName(item.path),
          path: item.path,
          kind: item.kind || 'file',
          category: item.category,
        });
      },
      [onOpenItemSheet],
    ),
  );
  const hasPinned = Boolean(caps?.hasFav && pinned.length > 0);
  const hasSync = Boolean(caps?.hasSync && syncFolders.length > 0);
  const firstSection = hasPinned ? 'pinned' : hasSync ? 'sync' : 'machines';
  const onlineMachineCount = (machines || []).filter((m) => m.reachable).length;

  const allowPinnedAudio = Boolean(capsStore || caps?.hasAudioThumb);
  const pinnedByMachine = useMemo(() => {
    const map = new Map();
    for (const p of pinned) {
      if (!isThumbEligible(p, { allowAudio: allowPinnedAudio })) continue;
      if (!map.has(p.machine)) map.set(p.machine, []);
      map.get(p.machine).push({
        path: p.path,
        mtime: '',
        isAudio: entryCategory(p) === 'audio',
      });
    }
    return map;
  }, [pinned, allowPinnedAudio]);

  // Batch-load thumbnails for pinned pictures, videos, and audio covers (up to 32 per machine call)
  // with retry backoff.
  const getPinnedThumbUrl = useMultiMachineThumbs({
    itemsByMachine: pinnedByMachine,
    enabled: Boolean((capsStore || caps?.hasThumb) && pinnedByMachine.size > 0),
    link,
    capsStore,
    thumbCache,
  });

  return html`
    <div>
      ${wide && !hideSearch
        ? html`
            <div class="search-row">
              <input
                class="input plain"
                type="search"
                placeholder="Search all machines"
                aria-label="Search all machines"
                onKeyDown=${(e) => {
                  if (e.key === 'Enter') {
                    e.preventDefault();
                    onStartSearch(e.target.value);
                  }
                }}
              />
            </div>
          `
        : null}

      ${hasPinned
        ? html`
            ${!(hideFirstLabel && firstSection === 'pinned') ? html`<p class="label">Pinned</p>` : null}
            <${FileNameList} className="tiles" lines=${2}>
              ${pinned.map((item, idx) => {
                const label = item.label || baseFileName(item.path);
                const prevLabel = idx > 0 ? pinned[idx - 1].label || baseFileName(pinned[idx - 1].path) : null;
                const nextLabel =
                  idx + 1 < pinned.length ? pinned[idx + 1].label || baseFileName(pinned[idx + 1].path) : null;
                const href =
                  item.kind === 'dir'
                    ? routeHref({ section: 'files', view: 'folder', machine: item.machine, path: item.path })
                    : routeHref({ section: 'files', view: 'home', viewer: { machine: item.machine, path: item.path } });
                const thumbUrl = isThumbEligible(item, { allowAudio: allowPinnedAudio })
                  ? getPinnedThumbUrl(item.machine, item.path, '')
                  : null;
                return html`
                  <a
                    key=${item.id}
                    class="tile"
                    href=${href}
                    aria-label=${label}
                    title=${label}
                    onPointerDown=${(e) => pinnedLongPress.start(e, item)}
                    onPointerMove=${pinnedLongPress.move}
                    onPointerUp=${pinnedLongPress.cancel}
                    onPointerCancel=${pinnedLongPress.cancel}
                    onContextMenu=${(e) => pinnedLongPress.contextMenu(e, item)}
                    onClick=${(e) => {
                      e.preventDefault();
                      if (pinnedLongPress.consumeFired()) return;
                      if (item.kind === 'dir') {
                        onNavigate(href);
                      } else {
                        onOpenFile(item.machine, {
                          name: label,
                          path: item.path,
                          kind: 'file',
                          category: item.category,
                        });
                      }
                    }}
                  >
                    <span class="thumb" aria-hidden="true">
                      ${thumbUrl
                        ? html`<img src=${thumbUrl} alt="" />`
                        : html`<${Icon} name=${item.kind === 'dir' ? 'files' : entryIconName(item)} />`}
                    </span>
                    <${FileName}
                      as="strong"
                      name=${label}
                      before=${prevLabel}
                      after=${nextLabel}
                      lines=${2}
                      titleOnHover=${false}
                    />
                    <span>${item.machine}</span>
                  </a>
                `;
              })}
            <//>
          `
        : null}

      ${hasSync
        ? html`
            ${!(hideFirstLabel && firstSection === 'sync') ? html`<p class="label">Kept in sync</p>` : null}
            <${FileNameList} className="list" lines=${2}>
              ${syncFolders.map((sf, idx) => {
                const href = routeHref({ section: 'files', view: 'sync', syncId: sf.id });
                const selected = wide && activeSyncId === sf.id;
                const prevName = idx > 0 ? syncFolders[idx - 1].name : null;
                const nextName = idx + 1 < syncFolders.length ? syncFolders[idx + 1].name : null;
                return html`
                  <a
                    key=${sf.id}
                    class=${selected ? 'row selected' : 'row'}
                    href=${href}
                    aria-label=${sf.name}
                    title=${sf.name}
                    onClick=${(e) => {
                      e.preventDefault();
                      onNavigate(href);
                    }}
                  >
                    <span class="thumb folder" aria-hidden="true"><${Icon} name="sync" /></span>
                    <span class="body">
                      <${FileName}
                        className="title fn-title"
                        name=${sf.name}
                        before=${prevName}
                        after=${nextName}
                        lines=${2}
                        titleOnHover=${false}
                      />
                      <span class="sub">${sf.summary || `${joinMemberNames(sf.members)} · Up to date`}</span>
                    </span>
                    <span class="chev" aria-hidden="true">›</span>
                  </a>
                `;
              })}
            <//>
          `
        : null}

      ${!(hideFirstLabel && firstSection === 'machines')
        ? wide && machines.length > 0
          ? html`
              <p class="label label-row">
                <span>Machines</span>
                <span class="label-fact">${onlineMachineCount} on</span>
              </p>
            `
          : html`<p class="label">Machines</p>`
        : null}
      <div class="list">
        ${machines.map((m) => {
          const href = routeHref({ section: 'files', view: 'folder', machine: m.name, path: '~' });
          const selected = wide && activeMachine === m.name;
          if (!m.reachable) {
            return html`
              <a
                key=${m.name}
                class=${selected ? 'row off selected' : 'row off'}
                href=${href}
                onClick=${(e) => {
                  e.preventDefault();
                  onNavigate(href);
                }}
              >
                <span class="dot off" aria-hidden="true"></span>
                <span class="body">
                  <span class="title">${m.name}</span>
                  <span class="sub">${offlineMachineSub(m)}</span>
                </span>
                <span class="chev" aria-hidden="true">›</span>
              </a>
            `;
          }
          return html`
            <a
              key=${m.name}
              class=${selected ? 'row selected' : 'row'}
              href=${href}
              onClick=${(e) => {
                e.preventDefault();
                onNavigate(href);
              }}
            >
              <span class="dot" aria-hidden="true"></span>
              <span class="body">
                <span class="title">${m.name}</span>
                <span class="sub">${freeSpaceSub(m)}</span>
              </span>
              <span class="chev" aria-hidden="true">›</span>
            </a>
          `;
        })}
      </div>

      <p class="label">More</p>
      <div class="list">
        ${caps?.hasFav
          ? html`
              <a
                class=${wide && activeView === 'favourites' ? 'row selected' : 'row'}
                href="#/files/@favourites"
                aria-current=${wide && activeView === 'favourites' ? 'page' : undefined}
                onClick=${(e) => {
                  e.preventDefault();
                  onNavigate('#/files/@favourites');
                }}
              >
                <span class="body">
                  <span class="title">Favourites</span>
                  <span class="sub">
                    ${favourites.length === 1 ? '1 file or folder' : `${favourites.length} files and folders`}
                  </span>
                </span>
                <span class="chev" aria-hidden="true">›</span>
              </a>
            `
          : null}
        <a
          class=${wide && activeView === 'recent' ? 'row selected' : 'row'}
          href="#/files/@recent"
          aria-current=${wide && activeView === 'recent' ? 'page' : undefined}
          onClick=${(e) => {
            e.preventDefault();
            onNavigate('#/files/@recent');
          }}
        >
          <span class="body">
            <span class="title">Recent</span>
            <span class="sub">
              ${recentFiles.length > 0
                ? `${recentFiles.length} opened on this ${deviceNoun}`
                : `Opened on this ${deviceNoun}`}
            </span>
          </span>
          <span class="chev" aria-hidden="true">›</span>
        </a>
      </div>
    </div>
  `;
}

// ── Folder Browser (used both in Files tab and inside Machine -> Files) ──────────────────────

export function FolderBrowser({
  machine,
  machineObj,
  path,
  link,
  caps,
  capsStore = null,
  capsEntry = null,
  missingFeatures = [],
  folderChanges = null,
  onPruneSelection = null,
  onRegisterRefresh = null,
  allowedSet,
  technical,
  deviceNoun,
  favourites,
  thumbCache,
  sortPref,
  kindFilter = '',
  onClearKindFilter = null,
  showHidden,
  selecting,
  selectedPaths,
  viewModeOverride,
  onViewModeResolved,
  onEntriesChange,
  onTotalChange = null,
  onNavigateFolder,
  onOpenFile,
  onOpenItemSheet = null,
  onToggleSelect,
  onStartSelectWith,
  onToggleStar,
  onSendTo,
  onEnsureAwake,
  wakePendingSet = null,
  onRefreshFleet,
  onNewFolder,
  onUpload,
  onGoHome,
  onOpenSearch,
  onOpenSort,
  onToggleView,
  onOpenMore,
  embedded = false,
  wide = false,
  hideWideChrome = false,
  reloadToken = 0,
  returnTarget = null,
  onReturnHandled = null,
}) {
  const [resolvedPath, setResolvedPath] = useState(path || '~');
  const [rawEntries, setRawEntries] = useState([]);
  const [totalCount, setTotalCount] = useState(null);
  const [totalExact, setTotalExact] = useState(true);
  const [hasMorePages, setHasMorePages] = useState(false);
  const [oldTruncated, setOldTruncated] = useState(false);
  const [serverSorted, setServerSorted] = useState(false);
  const [serverFilteredKind, setServerFilteredKind] = useState(false);
  const [loading, setLoading] = useState(true);
  const [loadingMore, setLoadingMore] = useState(false);
  const [renderLimit, setRenderLimit] = useState(RENDER_STEP);
  const [error, setError] = useState(null);
  const [justAddedPaths, setJustAddedPaths] = useState(() => new Set());

  const pagerRef = useRef(null);
  const crumbsRef = useRef(null);
  const entryLongPress = useItemLongPress(
    useCallback(
      (entry) => {
        if (!entry) return;
        if (onOpenItemSheet) onOpenItemSheet(machine, entry, { canSelect: Boolean(onStartSelectWith) });
        else if (onStartSelectWith) onStartSelectWith(entry.path);
      },
      [machine, onOpenItemSheet, onStartSelectWith],
    ),
    { disabled: selecting },
  );
  const justAddedTimerRef = useRef(null);
  const abortRef = useRef(null);
  const offsetRef = useRef(0);
  const lastLoadRef = useRef(null);
  const loadedAtRef = useRef(null);
  const rawEntriesRef = useRef(rawEntries);
  rawEntriesRef.current = rawEntries;
  const resolvedPathRef = useRef(resolvedPath);
  resolvedPathRef.current = resolvedPath;
  const coalescedRef = useRef(null);

  const favPathSet = useMemo(() => {
    const s = new Set();
    for (const f of favourites || []) {
      if (f.machine === machine) s.add(f.path);
    }
    return s;
  }, [favourites, machine]);

  const reachable = machineObj ? machineObj.reachable : true;
  const isWaking = Boolean(machine && wakePendingSet?.has(machine));
  const canWake = allowedSet ? allowedSet.has('power.wake') : false;

  const propsRef = useRef(null);
  propsRef.current = {
    machine,
    path: path || '~',
    reachable,
    showHidden,
    sortPref,
    kindFilter: kindFilter || '',
    caps,
    capsStore,
    link,
    allowedSet,
    folderChanges,
    onPruneSelection,
    onRefreshFleet,
    reloadToken,
  };

  useEffect(() => () => {
    if (justAddedTimerRef.current) {
      clearTimeout(justAddedTimerRef.current);
      justAddedTimerRef.current = null;
    }
  }, []);

  // Poll fleet status while waking this offline machine so the folder reopens at the same path.
  useEffect(() => {
    if ((reachable && error?.code !== 'unreachable') || !isWaking || typeof onRefreshFleet !== 'function') {
      return undefined;
    }
    const timer = window.setInterval(() => {
      onRefreshFleet({ background: true, forceOffline: true });
    }, 600);
    return () => window.clearInterval(timer);
  }, [reachable, error?.code, isWaking, onRefreshFleet]);

  const runDirectoryFetch = useCallback(async ({ quiet = false, highlightNew = false, signal = null } = {}) => {
    const p = propsRef.current;
    if (!p.machine || !p.reachable) return;
    const reqPath = p.path || '~';
    const queryKey = JSON.stringify({
      showHidden: Boolean(p.showHidden),
      sort: p.sortPref.sort,
      order: p.sortPref.order,
      kind: p.kindFilter,
    });

    let ctrl = null;
    let activeSignal = signal;
    if (!activeSignal) {
      if (abortRef.current) abortRef.current.abort();
      ctrl = new AbortController();
      abortRef.current = ctrl;
      activeSignal = ctrl.signal;
    }

    if (!quiet) {
      setLoading(true);
      setError(null);
      setRenderLimit(RENDER_STEP);
      setOldTruncated(false);
      offsetRef.current = 0;
    }

    try {
      if (p.capsStore && !p.capsStore.isKnown(p.machine)) {
        await p.capsStore.ensure(p.machine, { signal: activeSignal });
      }
      if (activeSignal.aborted) return;

      const liveCaps = p.capsStore
        ? p.capsStore.fileCapabilities(p.machine, { fallbackAllowedSet: p.allowedSet })
        : p.caps;

      const reqArgs = {
        path: reqPath,
        depth: 1,
        hidden: p.showHidden,
        limit: liveCaps?.hasListOffset ? PAGE_SIZE : 500,
      };
      if (liveCaps?.hasListSort) {
        reqArgs.sort = p.sortPref.sort;
        reqArgs.order = p.sortPref.order;
      }
      if (liveCaps?.hasListCategories && p.kindFilter) {
        reqArgs.categories = [p.kindFilter];
      }
      if (liveCaps?.hasListOffset) {
        reqArgs.offset = 0;
      }

      let res;
      let dropped = [];
      if (p.capsStore) {
        const outcome = await p.capsStore.call('fs.list', reqArgs, {
          on: p.machine,
          optional: ['sort', 'order', 'categories', 'offset'],
          signal: activeSignal,
          onDrop(sentArgs, droppedList) {
            if (droppedList.includes('offset')) {
              sentArgs.limit = 500;
            }
          },
        });
        res = outcome.value;
        dropped = outcome.dropped;
      } else {
        res = await p.link.call('fs.list', reqArgs, { on: p.machine, signal: activeSignal });
      }
      if (activeSignal.aborted) return;

      const usedServerSort = Boolean(
        liveCaps?.hasListSort && !dropped.includes('sort') && !dropped.includes('order'),
      );
      const usedServerCategories = Boolean(
        liveCaps?.hasListCategories && p.kindFilter && !dropped.includes('categories'),
      );
      const usedServerOffset = Boolean(liveCaps?.hasListOffset && !dropped.includes('offset'));

      const actualPath = res?.path || reqPath;
      const list = Array.isArray(res?.entries) ? res.entries : [];
      const nowMs = Date.now();

      if (p.folderChanges) {
        if (reqPath === '~' && actualPath && actualPath !== '~') {
          p.folderChanges.registerHome(p.machine, actualPath);
        }
        p.folderChanges.markLoaded(p.machine, actualPath, nowMs);
        if (actualPath !== reqPath) {
          p.folderChanges.markLoaded(p.machine, reqPath, nowMs);
        }
      }

      if (quiet) {
        const prevPaths = new Set((rawEntriesRef.current || []).map((e) => e.path));
        if (highlightNew && prevPaths.size > 0) {
          const added = new Set();
          for (const entry of list) {
            if (entry?.path && !prevPaths.has(entry.path)) {
              added.add(entry.path);
            }
          }
          if (added.size > 0) {
            setJustAddedPaths(added);
            if (justAddedTimerRef.current) clearTimeout(justAddedTimerRef.current);
            justAddedTimerRef.current = window.setTimeout(() => {
              justAddedTimerRef.current = null;
              setJustAddedPaths(new Set());
            }, 1500);
          }
        }
        if (typeof p.onPruneSelection === 'function') {
          p.onPruneSelection(new Set(list.map((e) => e.path)));
        }
        setError(null);
      }

      const nextTotalCount = typeof res?.total === 'number' ? res.total : list.length;
      const nextTotalExact = res?.total_exact !== false;
      const nextHasMorePages = Boolean(res?.truncated && usedServerOffset);
      const nextOldTruncated = Boolean(res?.truncated && !usedServerOffset);

      lastLoadRef.current = {
        machine: p.machine,
        path: reqPath,
        showHidden: p.showHidden,
        reloadToken: p.reloadToken,
        sort: p.sortPref.sort,
        order: p.sortPref.order,
        kind: p.kindFilter,
        usedServerSort,
        usedServerCategories,
      };
      loadedAtRef.current = nowMs;

      setResolvedPath(actualPath);
      setRawEntries(list);
      setTotalCount(nextTotalCount);
      setTotalExact(nextTotalExact);
      setServerSorted(usedServerSort);
      setServerFilteredKind(usedServerCategories);
      setHasMorePages(nextHasMorePages);
      setOldTruncated(nextOldTruncated);
      offsetRef.current = list.length;
      setLoading(false);

      if (p.folderChanges) {
        const cacheSnap = {
          queryKey,
          loadedAt: nowMs,
          resolvedPath: actualPath,
          rawEntries: list,
          totalCount: nextTotalCount,
          totalExact: nextTotalExact,
          hasMorePages: nextHasMorePages,
          oldTruncated: nextOldTruncated,
          serverSorted: usedServerSort,
          serverFilteredKind: usedServerCategories,
        };
        p.folderChanges.setCached(p.machine, actualPath, cacheSnap);
        if (actualPath !== reqPath) {
          p.folderChanges.setCached(p.machine, reqPath, cacheSnap);
        }
      }
    } catch (err) {
      if (activeSignal.aborted) return;
      if (!quiet) {
        lastLoadRef.current = null;
        setError(err);
        setLoading(false);
      } else {
        setError(err);
      }
    }
  }, []);

  // Load initial page of directory (restoring cached snapshot when available and refetching
  // quietly if stale) and set up a non-overlapping coalesced refecher for live announcements.
  useEffect(() => {
    const reqPath = path || '~';
    if (coalescedRef.current) {
      coalescedRef.current.cancel();
      coalescedRef.current = null;
    }

    if (!machine || !reachable) {
      lastLoadRef.current = null;
      loadedAtRef.current = null;
      setLoading(false);
      setRawEntries([]);
      setOldTruncated(false);
      return undefined;
    }

    const prev = lastLoadRef.current;
    const sameDir =
      prev &&
      prev.machine === machine &&
      prev.path === reqPath &&
      prev.showHidden === showHidden &&
      prev.reloadToken === reloadToken;
    const queryKey = JSON.stringify({
      showHidden: Boolean(showHidden),
      sort: sortPref.sort,
      order: sortPref.order,
      kind: kindFilter || '',
    });

    const runner = createCoalescedRefetch((opts) =>
      runDirectoryFetch({ quiet: true, highlightNew: opts?.highlightNew !== false }),
    );
    coalescedRef.current = runner;

    if (sameDir) {
      const sortChanged = prev.sort !== sortPref.sort || prev.order !== sortPref.order;
      const kindChanged = prev.kind !== (kindFilter || '');
      const needsServerForSort = sortChanged && prev.usedServerSort;
      const needsServerForKind = kindChanged && prev.usedServerCategories;
      if (!needsServerForSort && !needsServerForKind) {
        return () => {
          runner.cancel();
        };
      }
    }

    // Switching directories: check cache first so back navigation paints instantly, then
    // quietly refetch if anything changed while off-screen or data is older than 10s.
    const switchingDir = !prev || prev.machine !== machine || prev.path !== reqPath;
    if (switchingDir) {
      setJustAddedPaths(new Set());
      const cached = folderChanges ? folderChanges.getCached(machine, reqPath) : null;
      if (cached && cached.queryKey === queryKey) {
        setResolvedPath(cached.resolvedPath || reqPath);
        setRawEntries(cached.rawEntries || []);
        setTotalCount(cached.totalCount);
        setTotalExact(cached.totalExact);
        setServerSorted(cached.serverSorted);
        setServerFilteredKind(cached.serverFilteredKind);
        setHasMorePages(cached.hasMorePages);
        setOldTruncated(cached.oldTruncated);
        const cachedCount = (cached.rawEntries || []).length;
        setRenderLimit(cachedCount > PAGE_SIZE ? cachedCount : RENDER_STEP);
        setError(null);
        setLoading(false);
        offsetRef.current = (cached.rawEntries || []).length;
        loadedAtRef.current = cached.loadedAt;
        lastLoadRef.current = {
          machine,
          path: reqPath,
          showHidden,
          reloadToken,
          sort: sortPref.sort,
          order: sortPref.order,
          kind: kindFilter || '',
          usedServerSort: cached.serverSorted,
          usedServerCategories: cached.serverFilteredKind,
        };
        const stale = folderChanges.isStale(machine, cached.resolvedPath || reqPath, {
          loadedAt: cached.loadedAt,
          maxAgeMs: STALE_AFTER_MS,
        });
        if (stale) {
          runner.trigger({ highlightNew: false });
        }
        return () => {
          runner.cancel();
        };
      }
    }

    if (abortRef.current) abortRef.current.abort();
    const ctrl = new AbortController();
    abortRef.current = ctrl;

    const quietReload = Boolean(prev && prev.machine === machine && prev.path === reqPath && rawEntriesRef.current.length > 0);
    runDirectoryFetch({ quiet: quietReload, highlightNew: quietReload, signal: ctrl.signal });

    return () => {
      ctrl.abort();
      runner.cancel();
    };
  }, [machine, path, reachable, showHidden, sortPref.sort, sortPref.order, kindFilter, reloadToken, folderChanges, runDirectoryFetch]);

  // Subscribe to live folder change announcements for this folder (both requested and resolved paths).
  useEffect(() => {
    if (!folderChanges || !machine || !reachable) return undefined;
    const reqPath = path || '~';
    const onChanged = () => {
      coalescedRef.current?.trigger({ highlightNew: true });
    };
    const unsubs = [folderChanges.subscribe(machine, reqPath, onChanged)];
    if (resolvedPath && resolvedPath !== reqPath) {
      unsubs.push(folderChanges.subscribe(machine, resolvedPath, onChanged));
    }
    return () => {
      for (const u of unsubs) u();
    };
  }, [folderChanges, machine, path, resolvedPath, reachable]);

  // Refetch quietly for external changes when returning to foreground/focus if older than 10s.
  useEffect(() => {
    if (!machine || !reachable) return undefined;
    const checkStaleOnVisible = () => {
      if (document.visibilityState !== 'visible') return;
      const loadedAt = loadedAtRef.current;
      if (typeof loadedAt !== 'number') return;
      const targetPath = resolvedPathRef.current || path || '~';
      const stale = folderChanges
        ? folderChanges.isStale(machine, targetPath, { loadedAt, maxAgeMs: STALE_AFTER_MS })
        : Date.now() - loadedAt > STALE_AFTER_MS;
      if (stale) {
        coalescedRef.current?.trigger({ highlightNew: false });
      }
    };
    document.addEventListener('visibilitychange', checkStaleOnVisible);
    window.addEventListener('focus', checkStaleOnVisible);
    return () => {
      document.removeEventListener('visibilitychange', checkStaleOnVisible);
      window.removeEventListener('focus', checkStaleOnVisible);
    };
  }, [folderChanges, machine, path, reachable]);

  // Register this folder's quiet refetch with the shell's pull-to-refresh container.
  useEffect(() => {
    if (typeof onRegisterRefresh !== 'function') return undefined;
    return onRegisterRefresh(async () => {
      if (!propsRef.current.reachable) {
        await propsRef.current.onRefreshFleet?.({ forceOffline: true });
        return;
      }
      if (coalescedRef.current) {
        await coalescedRef.current.trigger({ highlightNew: true });
      } else {
        await runDirectoryFetch({ quiet: true, highlightNew: true });
      }
    });
  }, [onRegisterRefresh, runDirectoryFetch, machine, path, reachable]);

  const loadNextPage = useCallback(() => {
    if (loadingMore || !hasMorePages || !caps?.hasListOffset) return;
    setLoadingMore(true);
    const reqArgs = {
      path: resolvedPath,
      depth: 1,
      hidden: showHidden,
      limit: NEXT_PAGE_SIZE,
      offset: offsetRef.current,
    };
    if (caps?.hasListSort) {
      reqArgs.sort = sortPref.sort;
      reqArgs.order = sortPref.order;
    }
    if (caps?.hasListCategories && kindFilter) {
      reqArgs.categories = [kindFilter];
    }
    const nextPromise = capsStore
      ? capsStore
          .call('fs.list', reqArgs, {
            on: machine,
            optional: ['sort', 'order', 'categories'],
            signal: abortRef.current?.signal,
          })
          .then((r) => r.value)
      : link.call('fs.list', reqArgs, { on: machine, signal: abortRef.current?.signal });
    nextPromise
      .then((res) => {
        const more = Array.isArray(res?.entries) ? res.entries : [];
        let mergedList = null;
        setRawEntries((prev) => {
          const seen = new Set(prev.map((e) => e.path));
          const merged = [...prev];
          for (const e of more) {
            if (!seen.has(e.path)) merged.push(e);
          }
          mergedList = merged;
          return merged;
        });
        offsetRef.current += more.length;
        const nextHasMore = Boolean(res?.truncated && more.length > 0);
        setHasMorePages(nextHasMore);
        setRenderLimit((n) =>
          Math.max(n + RENDER_STEP, mergedList ? mergedList.length : 0, offsetRef.current),
        );
        setLoadingMore(false);
        if (folderChanges && mergedList) {
          const prevSnap =
            folderChanges.getCached(machine, resolvedPath) ||
            folderChanges.getCached(machine, path || '~');
          if (prevSnap) {
            const updatedSnap = {
              ...prevSnap,
              rawEntries: mergedList,
              hasMorePages: nextHasMore,
            };
            folderChanges.setCached(machine, resolvedPath, updatedSnap);
            const reqPath = path || '~';
            if (resolvedPath !== reqPath) {
              folderChanges.setCached(machine, reqPath, updatedSnap);
            }
          }
        }
      })
      .catch(() => {
        setLoadingMore(false);
      });
  }, [loadingMore, hasMorePages, caps?.hasListOffset, caps?.hasListSort, caps?.hasListCategories, resolvedPath, path, showHidden, sortPref.sort, sortPref.order, kindFilter, capsStore, folderChanges, link, machine]);

  // Client-side filter & sort guarantees consistent behavior on older nodes too.
  const entries = useMemo(() => {
    const applyCats = Boolean(kindFilter) && !serverFilteredKind;
    const filtered = filterEntries(rawEntries, {
      hidden: showHidden,
      categories: applyCats ? [kindFilter] : [],
      keepDirs: true,
    });
    return serverSorted ? filtered : sortEntries(filtered, sortPref);
  }, [rawEntries, showHidden, kindFilter, serverFilteredKind, serverSorted, sortPref]);

  useEffect(() => {
    if (onEntriesChange) onEntriesChange(entries, resolvedPath);
  }, [entries, resolvedPath]);

  useEffect(() => {
    if (typeof onTotalChange === 'function') {
      onTotalChange({
        totalCount: reachable && !error ? totalCount : null,
        totalExact,
        resolvedPath,
      });
    }
  }, [onTotalChange, totalCount, totalExact, resolvedPath, reachable, error]);

  const savedMode = readFolderViewMode(safeStorage(), machine, resolvedPath);
  const effectiveMode = viewModeOverride || savedMode || (shouldDefaultToGrid(rawEntries) ? 'grid' : 'list');

  useEffect(() => {
    if (onViewModeResolved) onViewModeResolved(effectiveMode, resolvedPath);
  }, [effectiveMode, resolvedPath]);

  const visibleEntries = useMemo(() => entries.slice(0, renderLimit), [entries, renderLimit]);
  const hasMoreToRender = visibleEntries.length < entries.length;

  const activeReturnTarget = useMemo(() => {
    if (!returnTarget || !returnTarget.path) return null;
    if (returnTarget.machine && returnTarget.machine !== machine) return null;
    return returnTarget;
  }, [returnTarget, machine]);

  // If returning to an entry that is already loaded in `entries` beyond `renderLimit`, reveal it
  // immediately so the virtual list can scroll straight to it.
  useEffect(() => {
    if (!activeReturnTarget?.path || entries.length === 0) return;
    const idx = entries.findIndex((e) => e?.path === activeReturnTarget.path);
    if (idx >= renderLimit) {
      setRenderLimit((n) => Math.max(n, idx + 1));
    }
  }, [activeReturnTarget, entries, renderLimit]);

  const handleNearEnd = useCallback(() => {
    if (hasMoreToRender) {
      setRenderLimit((n) => Math.max(n + RENDER_STEP, entries.length));
    } else if (hasMorePages) {
      loadNextPage();
    }
  }, [hasMoreToRender, hasMorePages, entries.length, loadNextPage]);

  // Auto-advance incremental slice or fetch next offset page when sentinel scrolls into view.
  useEffect(() => {
    const el = pagerRef.current;
    if (!el || typeof IntersectionObserver === 'undefined') return undefined;
    const scroller = findScrollParent(el);
    const root =
      scroller && scroller !== document.documentElement && scroller !== document.body
        ? scroller
        : null;
    const obs = new IntersectionObserver(
      (hits) => {
        if (!hits.some((h) => h.isIntersecting)) return;
        if (hasMoreToRender) {
          setRenderLimit((n) => n + RENDER_STEP);
        } else if (hasMorePages) {
          loadNextPage();
        }
      },
      { root, rootMargin: '360px 0px' },
    );
    obs.observe(el);
    return () => obs.disconnect();
  }, [hasMoreToRender, hasMorePages, loadNextPage]);

  useEffect(() => {
    if (crumbsRef.current) {
      crumbsRef.current.scrollLeft = crumbsRef.current.scrollWidth;
    }
  }, [resolvedPath]);

  const { getUrl: getThumbUrl, observeEl: observeThumb } = useBatchedThumbs({
    machine,
    entries: visibleEntries,
    link,
    capsStore,
    enabled: Boolean(caps?.known && caps?.hasThumb && reachable),
    allowAudio: Boolean(caps?.hasAudioThumb),
    thumbCache,
  });

  if (!reachable || error?.code === 'unreachable') {
    return html`
      <div>
        <div class="notice">
          <strong>${machine} is off.</strong>
          <span>
            ${isWaking
              ? `Waking ${machine}… This folder will reopen here when it answers.`
              : offlineMachineSub(machineObj || { name: machine })}
          </span>
        </div>
        ${machineObj?.canWake && canWake && onEnsureAwake
          ? html`
              <div class="actions">
                <button
                  type="button"
                  class="btn primary"
                  disabled=${isWaking}
                  onClick=${() =>
                    onEnsureAwake(machineObj, {
                      kind: 'files',
                      label: 'open its files',
                    })}
                >
                  ${isWaking ? `Waking ${machine}…` : `Wake ${machine}`}
                </button>
              </div>
            `
          : null}
      </div>
    `;
  }

  const homePath = folderChanges?.getHome?.(machine) || null;
  const crumbs = splitFileCrumbs(machine, resolvedPath, { technical, homePath });

  function handleEntryTap(e, entry) {
    e.preventDefault();
    if (entryLongPress.consumeFired()) {
      return;
    }
    if (selecting) {
      if (onToggleSelect) onToggleSelect(entry.path);
      return;
    }
    if (entry.kind === 'dir') {
      onNavigateFolder(machine, entry.path);
    } else {
      onOpenFile(machine, entry, entries);
    }
  }

  function renderListRow(entry, idx, list) {
    const picked = selecting && selectedPaths.has(entry.path);
    const starred = caps?.hasFav && favPathSet.has(entry.path);
    const thumbUrl = getThumbUrl(entry);
    const beforeName = idx > 0 ? list[idx - 1].name : null;
    const afterName = idx + 1 < list.length ? list[idx + 1].name : null;
    const href =
      entry.kind === 'dir'
        ? routeHref({ section: 'files', view: 'folder', machine, path: entry.path })
        : routeHref({
            section: 'files',
            view: 'folder',
            machine,
            path: resolvedPath,
            viewer: { machine, path: entry.path },
          });

    const rowCls = ['row', picked ? 'picked' : '', justAddedPaths.has(entry.path) ? 'just-added' : '']
      .filter(Boolean)
      .join(' ');
    const rowEl = html`
      <a
        class=${rowCls}
        href=${href}
        aria-label=${entry.name}
        title=${entry.name}
        aria-selected=${selecting ? (picked ? 'true' : 'false') : undefined}
        onPointerDown=${(e) => entryLongPress.start(e, entry)}
        onPointerMove=${entryLongPress.move}
        onPointerUp=${entryLongPress.cancel}
        onPointerCancel=${entryLongPress.cancel}
        onContextMenu=${(e) => entryLongPress.contextMenu(e, entry)}
        onClick=${(e) => handleEntryTap(e, entry)}
      >
        <${EntryThumb} entry=${entry} thumbUrl=${thumbUrl} observeEl=${observeThumb} />
        <span class="body">
          <${FileName}
            className=${technical ? 'title mono fn-title' : 'title fn-title'}
            name=${entry.name}
            before=${beforeName}
            after=${afterName}
            lines=${2}
            titleOnHover=${false}
          />
          <span class="sub">${entrySubtitle(entry, technical)}</span>
        </span>
        ${selecting
          ? html`<span class="check" aria-hidden="true"></span>`
          : html`
              ${starred ? html`<span class="star" aria-label="Favourite">★</span>` : null}
              ${entry.kind === 'dir' ? html`<span class="chev" aria-hidden="true">›</span>` : null}
            `}
      </a>
    `;

    if (selecting) {
      return html`<div key=${entry.path}>${rowEl}</div>`;
    }

    const swipeActions = [];
    if (caps?.hasFavWrite && onToggleStar) {
      swipeActions.push({
        label: starred ? 'Unstar' : 'Star',
        tone: 'neutral',
        onAction: () => onToggleStar(machine, entry),
      });
    }
    if ((caps?.hasTransfer || caps?.hasDownload) && onSendTo) {
      swipeActions.push({
        label: 'Send to',
        tone: 'accent',
        onAction: () => onSendTo(machine, [entry]),
      });
    }

    if (swipeActions.length === 0) {
      return html`<div key=${entry.path}>${rowEl}</div>`;
    }

    return html`
      <${SwipeRow} key=${entry.path} actions=${swipeActions}>
        ${rowEl}
      <//>
    `;
  }

  const listResetKey = `${machine}\0${resolvedPath}\0${sortPref.sort}\0${sortPref.order}\0${kindFilter || ''}\0${showHidden ? '1' : '0'}`;

  function renderGridCell(entry, idx, list) {
    const picked = selecting && selectedPaths.has(entry.path);
    const starred = caps?.hasFav && favPathSet.has(entry.path);
    const thumbUrl = getThumbUrl(entry);
    const isDir = entry.kind === 'dir';
    const beforeName = idx > 0 ? list[idx - 1].name : null;
    const afterName = idx + 1 < list.length ? list[idx + 1].name : null;
    const href = isDir
      ? routeHref({ section: 'files', view: 'folder', machine, path: entry.path })
      : routeHref({
          section: 'files',
          view: 'folder',
          machine,
          path: resolvedPath,
          viewer: { machine, path: entry.path },
        });
    const cls = [
      'cell',
      isDir ? 'folder' : '',
      picked ? 'picked' : '',
      justAddedPaths.has(entry.path) ? 'just-added' : '',
    ]
      .filter(Boolean)
      .join(' ');
    return html`
      <a
        key=${entry.path}
        class=${cls}
        href=${href}
        aria-label=${entry.name}
        title=${entry.name}
        ref=${!isDir ? observeThumb : null}
        data-thumb-path=${!isDir ? entry.path : undefined}
        onPointerDown=${(e) => entryLongPress.start(e, entry)}
        onPointerMove=${entryLongPress.move}
        onPointerUp=${entryLongPress.cancel}
        onPointerCancel=${entryLongPress.cancel}
        onContextMenu=${(e) => entryLongPress.contextMenu(e, entry)}
        onClick=${(e) => handleEntryTap(e, entry)}
      >
        ${isDir
          ? html`
              <${Icon} name="files" />
              <${FileName}
                className="cell-name"
                name=${entry.name}
                before=${beforeName}
                after=${afterName}
                lines=${3}
                titleOnHover=${false}
              />
            `
          : thumbUrl
            ? html`<img src=${thumbUrl} alt=${entry.name} loading="lazy" decoding="async" />`
            : html`
                <span class="cell-icon">
                  <${Icon} name=${entryIconName(entry)} />
                  <${FileName}
                    className="cell-name"
                    name=${entry.name}
                    before=${beforeName}
                    after=${afterName}
                    lines=${3}
                    titleOnHover=${false}
                  />
                </span>
              `}
        ${entry.duration_s != null
          ? html`<span class="len">${formatElapsed(entry.duration_s)}</span>`
          : null}
        ${starred && !selecting ? html`<span class="cell-star" aria-label="Favourite">★</span>` : null}
        ${selecting ? html`<span class="cell-check" aria-hidden="true"></span>` : null}
      </a>
    `;
  }

  function renderGrid() {
    const gridItems = [];
    const otherItems = [];
    for (const entry of visibleEntries) {
      const cat = entryCategory(entry);
      if (entry.kind === 'dir' || cat === 'image' || cat === 'video') {
        gridItems.push(entry);
      } else {
        otherItems.push(entry);
      }
    }

    return html`
      <div>
        ${gridItems.length > 0
          ? html`
              <${VirtualFileGrid}
                items=${gridItems}
                renderCell=${renderGridCell}
                resetKey=${listResetKey}
                onNearEnd=${otherItems.length === 0 ? handleNearEnd : null}
                returnTarget=${activeReturnTarget}
                onReturnHandled=${onReturnHandled}
                lines=${3}
              />
            `
          : null}

        ${otherItems.length > 0
          ? html`
              <p class="label">Other files</p>
              <${VirtualFileList}
                items=${otherItems}
                renderRow=${renderListRow}
                resetKey=${`${listResetKey}\0other`}
                onNearEnd=${handleNearEnd}
                returnTarget=${activeReturnTarget}
                onReturnHandled=${onReturnHandled}
                lines=${2}
              />
            `
          : null}
      </div>
    `;
  }

  const inlineTools = wide && !selecting && !hideWideChrome;
  const atMachineFilesRoot =
    !path || path === '~' || (homePath && resolvedPath === homePath) || isMachineFilesRoot(resolvedPath);
  const wideFolderHeading = folderDisplayName(resolvedPath, { technical, homePath });

  return html`
    <div data-no-swipe="true">
      ${wide && !embedded && !hideWideChrome
        ? html`
            <div class="files-wide-head">
              <h2 aria-label=${wideFolderHeading} title=${wideFolderHeading}>
                <${FileNameSingle} name=${wideFolderHeading} lines=${1} titleOnHover=${false} />
              </h2>
              ${totalCount != null && totalCount > 0
                ? html`<span class="dim">${formatFolderTotal(totalCount, totalExact)}</span>`
                : null}
            </div>
          `
        : null}

      ${inlineTools
        ? html`
            <div class="files-wide-tools">
              <input
                class="input plain"
                type="search"
                placeholder=${`Search in ${folderDisplayName(resolvedPath, { technical, homePath })}`}
                aria-label=${`Search in ${folderDisplayName(resolvedPath, { technical, homePath })}`}
                onKeyDown=${(e) => {
                  if (e.key === 'Enter') {
                    e.preventDefault();
                    if (onOpenSearch) onOpenSearch(e.target.value, machine, resolvedPath);
                  }
                }}
              />
              <button
                type="button"
                class="btn icon"
                aria-label=${kindFilter
                  ? `Sort: ${sortDescription(sortPref)}, showing ${KIND_FILTER_LABELS[kindFilter] || kindFilter}`
                  : `Sort: ${sortDescription(sortPref)}`}
                aria-pressed=${kindFilter ? 'true' : 'false'}
                onClick=${onOpenSort}
              >
                <${Icon} name="sort" />
              </button>
              <button
                type="button"
                class="btn icon"
                aria-label=${effectiveMode === 'grid' ? 'Show as list' : 'Show as grid'}
                aria-pressed=${effectiveMode === 'grid' ? 'true' : 'false'}
                onClick=${onToggleView}
              >
                <${Icon} name="grid" />
              </button>
              <button type="button" class="btn icon" aria-label="More" onClick=${onOpenMore}>
                <${Icon} name="more" />
              </button>
            </div>
          `
        : null}

      ${!hideWideChrome
        ? html`
            <nav ref=${crumbsRef} class="where" aria-label="Where this folder is">
              ${crumbs.flatMap((c, i) => {
                const isLast = i === crumbs.length - 1;
                const out = [];
                if (i > 0) out.push(html`<i key=${`sep-${i}`} aria-hidden="true">›</i>`);
                out.push(html`
                  <a
                    key=${`${c.label}-${i}`}
                    class=${isLast ? 'current' : undefined}
                    aria-current=${isLast ? 'page' : undefined}
                    href=${routeHref({ section: 'files', view: 'folder', machine, path: c.path })}
                    onClick=${(e) => {
                      e.preventDefault();
                      onNavigateFolder(machine, c.path);
                    }}
                  >
                    ${c.label}
                  </a>
                `);
                return out;
              })}
            </nav>
          `
        : null}

      ${atMachineFilesRoot && missingFeatures?.length > 0
        ? html`
            <${OlderMachineNote}
              machine=${machine}
              version=${capsEntry?.version || machineObj?.version || null}
              build=${capsEntry?.build || machineObj?.build || null}
              missing=${missingFeatures}
              technical=${technical}
            />
          `
        : null}

      ${kindFilter
        ? html`
            <div class="folder-filter-bar" role="status">
              <span>Showing ${KIND_FILTER_LABELS[kindFilter] || kindFilter}</span>
              ${onClearKindFilter
                ? html`
                    <button type="button" class="btn small quiet" onClick=${onClearKindFilter}>
                      Show all
                    </button>
                  `
                : null}
            </div>
          `
        : null}

      ${error?.code === 'denied'
        ? html`
            <div class="empty">
              <p>You're not allowed to open this folder.</p>
              ${onGoHome
                ? html`
                    <div class="empty-actions">
                      <button type="button" class="btn small" onClick=${onGoHome}>Back to Files</button>
                    </div>
                  `
                : null}
            </div>
          `
        : error
          ? html`<${Notice} error=${error} technical=${technical} deviceNoun=${deviceNoun} />`
          : null}

      ${error?.code === 'denied'
        ? null
        : loading && rawEntries.length === 0
          ? html`<div class="empty">Loading folder…</div>`
          : entries.length === 0 && !error
            ? kindFilter
              ? html`
                  <div class="empty">
                    <p>No ${(KIND_FILTER_LABELS[kindFilter] || kindFilter).toLowerCase()} in this folder.</p>
                    ${onClearKindFilter
                      ? html`
                          <div class="empty-actions">
                            <button type="button" class="btn small" onClick=${onClearKindFilter}>
                              Show all
                            </button>
                          </div>
                        `
                      : null}
                  </div>
                `
              : html`
                <div class="empty">
                  <p>This folder is empty.</p>
                  ${(caps?.hasMkdir && onNewFolder) || (caps?.hasUpload && onUpload)
                    ? html`
                        <div class="empty-actions">
                          ${caps?.hasMkdir && onNewFolder
                            ? html`
                                <button
                                  type="button"
                                  class="btn small"
                                  onClick=${() => onNewFolder(machine, resolvedPath)}
                                >
                                  New folder
                                </button>
                              `
                            : null}
                          ${caps?.hasUpload && onUpload
                            ? html`
                                <button
                                  type="button"
                                  class="btn small"
                                  onClick=${() => onUpload(machine, resolvedPath)}
                                >
                                  Add from this ${deviceNoun}
                                </button>
                              `
                            : null}
                        </div>
                      `
                    : null}
                </div>
              `
            : effectiveMode === 'grid'
              ? renderGrid()
              : html`
                  <${VirtualFileList}
                    items=${visibleEntries}
                    renderRow=${renderListRow}
                    resetKey=${listResetKey}
                    onNearEnd=${handleNearEnd}
                    returnTarget=${activeReturnTarget}
                    onReturnHandled=${onReturnHandled}
                    lines=${2}
                  />
                `}

      ${oldTruncated
        ? html`
            <p class="folder-limit-note">
              Showing the first ${rawEntries.length.toLocaleString()}. Update Legion on ${machine} to see everything.
            </p>
          `
        : null}

      ${hasMoreToRender || hasMorePages
        ? html`
            <div ref=${pagerRef} class="folder-pager">
              <span>${formatShowingCount(visibleEntries.length, totalCount, totalExact)}</span>
              <button
                type="button"
                class="btn small"
                disabled=${loadingMore}
                onClick=${() => {
                  if (hasMoreToRender) setRenderLimit((n) => n + RENDER_STEP);
                  else loadNextPage();
                }}
              >
                ${loadingMore ? 'Loading more…' : 'Show more'}
              </button>
            </div>
          `
        : null}
    </div>
  `;
}

// ── Search View ──────────────────────────────────────────────────────────────────────────────

export function FilesSearchView({
  route,
  machines,
  favourites,
  caps,
  capsStore = null,
  link,
  technical,
  thumbCache,
  wide = false,
  onRegisterRefresh = null,
  onNavigateFolder,
  onOpenFile,
  onOpenItemSheet = null,
  onResultCountChange,
  onClearFilters,
  onUpdateQuery = null,
  onOpenFilters = null,
}) {
  const scopeMachine = route.machine || null;
  const scopePath = route.path || null;
  const q = route.query?.q || '';
  const kinds = route.query?.kinds || [];
  const size = route.query?.size || '';
  const changed = route.query?.changed || '';
  const kindsKey = kinds.join(',');
  const searchLongPress = useItemLongPress(
    useCallback(
      (hit) => {
        if (!onOpenItemSheet || !hit) return;
        onOpenItemSheet(hit.machine, hit.entry);
      },
      [onOpenItemSheet],
    ),
  );

  const [resultsByMachine, setResultsByMachine] = useState({});
  const [truncatedByMachine, setTruncatedByMachine] = useState({});
  const [searching, setSearching] = useState(false);
  const [error, setError] = useState(null);
  const [searchRev, setSearchRev] = useState(0);
  const abortRef = useRef(null);
  const refreshResolversRef = useRef([]);

  const settleRefreshWaiters = useCallback(() => {
    const waiters = refreshResolversRef.current;
    refreshResolversRef.current = [];
    for (const resolve of waiters) resolve();
  }, []);

  useEffect(() => {
    if (typeof onRegisterRefresh !== 'function') return undefined;
    return onRegisterRefresh(
      () =>
        new Promise((resolve) => {
          refreshResolversRef.current.push(resolve);
          setSearchRev((n) => n + 1);
        }),
    );
  }, [onRegisterRefresh]);

  // Batch-load thumbnails for picture, video, and audio search hits (up to 32 per machine call)
  // with retry backoff.
  const allowSearchAudio = Boolean(capsStore || caps?.hasAudioThumb);
  const searchThumbsByMachine = useMemo(() => {
    const map = new Map();
    for (const [mName, items] of Object.entries(resultsByMachine)) {
      if (!Array.isArray(items) || items.length === 0) continue;
      const eligible = [];
      for (const entry of items) {
        if (!isThumbEligible(entry, { allowAudio: allowSearchAudio })) continue;
        eligible.push({
          path: entry.path,
          mtime: entry.mtime || '',
          isAudio: entryCategory(entry) === 'audio',
        });
      }
      if (eligible.length > 0) map.set(mName, eligible);
    }
    return map;
  }, [resultsByMachine, allowSearchAudio]);

  useMultiMachineThumbs({
    itemsByMachine: searchThumbsByMachine,
    enabled: Boolean(capsStore || caps?.hasThumb),
    link,
    capsStore,
    thumbCache,
  });

  const favKeySet = useMemo(
    () => new Set((favourites || []).map((f) => `${f.machine}\0${f.path}`)),
    [favourites],
  );

  const onlineMachines = useMemo(() => machines.filter((m) => m.reachable), [machines]);
  const offMachines = useMemo(() => machines.filter((m) => !m.reachable), [machines]);
  const onlineKey = useMemo(() => onlineMachines.map((m) => m.name).join(','), [onlineMachines]);

  useEffect(() => {
    if (abortRef.current) abortRef.current.abort();
    const ctrl = new AbortController();
    abortRef.current = ctrl;

    const hasCriteria = Boolean(q.trim() || kinds.length > 0 || size || changed);
    if (!hasCriteria) {
      setResultsByMachine({});
      setTruncatedByMachine({});
      setSearching(false);
      if (onResultCountChange) onResultCountChange({ count: 0, machineCount: 0, searching: false });
      settleRefreshWaiters();
      return () => ctrl.abort();
    }

    setSearching(true);
    setError(null);
    if (searchRev === 0) {
      setResultsByMachine({});
      setTruncatedByMachine({});
    }

    const { min_size, max_size } = sizePresetBounds(size);
    const modified_after = changedPresetCutoff(changed);

    const buildArgsForMachine = (basePath, mCaps) => {
      if (mCaps?.hasFindEnhanced) {
        const args = { path: basePath, limit: 100 };
        if (q.trim()) args.name = q.trim();
        if (kinds.length > 0) {
          const fileCats = kinds.filter((k) => k !== 'folder');
          if (kinds.length === 1 && kinds[0] === 'folder') {
            args.kind = 'dir';
          } else if (fileCats.length > 0) {
            args.categories = fileCats;
          }
        }
        if (mCaps.hasFindFilters) {
          if (min_size != null) args.min_size = min_size;
          if (max_size != null) args.max_size = max_size;
          if (modified_after) args.modified_after = modified_after;
        }
        return {
          args,
          optional: [
            'name',
            'categories',
            'kind',
            'min_size',
            'max_size',
            'modified_after',
            'sort',
            'order',
            'hidden',
            'budget_ms',
          ],
        };
      }
      // Older node degrade: basic glob/kind + client-side filtering.
      const args = { path: basePath, limit: 300 };
      const glob = buildOldFindGlob(q);
      if (glob) args.glob = glob;
      if (kinds.length === 1 && kinds[0] === 'folder') args.kind = 'dir';
      return { args, optional: ['glob', 'kind'] };
    };

    const filterClient = (rawRes) => {
      const normalized = normalizeFindEntries(rawRes?.matches || rawRes?.entries || []);
      return filterEntries(normalized, {
        hidden: false,
        query: q.trim(),
        categories: kinds,
        minSize: min_size,
        maxSize: max_size,
        modifiedAfter: modified_after,
      });
    };

    const searchOneMachine = async (mName, basePath) => {
      if (capsStore) {
        if (!capsStore.isKnown(mName)) {
          await capsStore.ensure(mName, { signal: ctrl.signal });
        }
        if (ctrl.signal.aborted) return null;
        const mCaps = capsStore.fileCapabilities(mName);
        const { args, optional } = buildArgsForMachine(basePath, mCaps);
        const outcome = await capsStore.call('fs.find', args, {
          on: mName,
          optional,
          signal: ctrl.signal,
        });
        const entries = filterClient(outcome.value);
        return { entries, truncated: Boolean(outcome.value?.truncated) };
      }
      const { args } = buildArgsForMachine(basePath, caps);
      const res = await link.call('fs.find', args, { on: mName, signal: ctrl.signal });
      const entries = filterClient(res);
      return { entries, truncated: Boolean(res?.truncated) };
    };

    if (scopeMachine) {
      searchOneMachine(scopeMachine, scopePath || '~')
        .then((hit) => {
          if (ctrl.signal.aborted || !hit) return;
          setResultsByMachine({ [scopeMachine]: hit.entries });
          setTruncatedByMachine({ [scopeMachine]: hit.truncated });
          setSearching(false);
          if (onResultCountChange) {
            onResultCountChange({ count: hit.entries.length, machineCount: 1, searching: false });
          }
          settleRefreshWaiters();
        })
        .catch((err) => {
          if (ctrl.signal.aborted) return;
          setError(err);
          setSearching(false);
          if (onResultCountChange) onResultCountChange({ count: 0, machineCount: 0, searching: false });
          settleRefreshWaiters();
        });
    } else if (capsStore && onlineMachines.length > 0) {
      // Fan out per online machine using each machine's own capabilities so mixed-version fleets
      // never send unsupported arguments to older machines.
      (async () => {
        const nextMap = {};
        const nextTrunc = {};
        let firstErr = null;
        let okCount = 0;

        await Promise.all(
          onlineMachines.map(async (m) => {
            try {
              const hit = await searchOneMachine(m.name, '~');
              if (ctrl.signal.aborted || !hit) return;
              okCount += 1;
              nextMap[m.name] = hit.entries;
              nextTrunc[m.name] = hit.truncated;
              setResultsByMachine((prev) => ({ ...prev, [m.name]: hit.entries }));
              setTruncatedByMachine((prev) => ({ ...prev, [m.name]: hit.truncated }));
            } catch (err) {
              if (ctrl.signal.aborted) return;
              if (!firstErr) firstErr = err;
              nextMap[m.name] = [];
              setResultsByMachine((prev) => ({ ...prev, [m.name]: [] }));
            }
          }),
        );

        if (ctrl.signal.aborted) return;
        if (okCount === 0 && firstErr) {
          setError(firstErr);
        }
        setSearching(false);
        const total = Object.values(nextMap).reduce((s, arr) => s + arr.length, 0);
        if (onResultCountChange) {
          onResultCountChange({ count: total, machineCount: okCount, searching: false });
        }
        settleRefreshWaiters();
      })();
    } else {
      const { args } = buildArgsForMachine('~', caps);
      link
        .call('fs.find', args, {
          on: 'all',
          signal: ctrl.signal,
          onEvent(evt) {
            if (ctrl.signal.aborted) return;
            if (evt?.kind === 'find_machine' && evt.machine && evt.data) {
              const entries = filterClient(evt.data);
              setResultsByMachine((prev) => ({ ...prev, [evt.machine]: entries }));
              setTruncatedByMachine((prev) => ({ ...prev, [evt.machine]: Boolean(evt.data.truncated) }));
            }
          },
        })
        .then((res) => {
          if (ctrl.signal.aborted) return;
          const nextMap = {};
          const nextTrunc = {};
          const rawResults = res?.results || {};
          for (const [mName, outcome] of Object.entries(rawResults)) {
            if (outcome?.ok) {
              nextMap[mName] = filterClient(outcome.ok);
              nextTrunc[mName] = Boolean(outcome.ok.truncated);
            }
          }
          setResultsByMachine(nextMap);
          setTruncatedByMachine(nextTrunc);
          setSearching(false);
          const total = Object.values(nextMap).reduce((s, arr) => s + arr.length, 0);
          if (onResultCountChange) {
            onResultCountChange({ count: total, machineCount: Object.keys(nextMap).length, searching: false });
          }
          settleRefreshWaiters();
        })
        .catch((err) => {
          if (ctrl.signal.aborted) return;
          setError(err);
          setSearching(false);
          if (onResultCountChange) onResultCountChange({ count: 0, machineCount: 0, searching: false });
          settleRefreshWaiters();
        });
    }

    return () => {
      ctrl.abort();
      settleRefreshWaiters();
    };
  }, [q, kindsKey, size, changed, scopeMachine, scopePath, onlineKey, capsStore, link, searchRev, settleRefreshWaiters]);

  const machineOrder = scopeMachine
    ? [scopeMachine]
    : onlineMachines.map((m) => m.name);

  const hasCriteria = Boolean(q.trim() || kinds.length > 0 || size || changed);
  const activeKinds = new Set(kinds);
  const hasMoreFilters = Boolean(size || changed);
  const showMoreFilters = caps ? Boolean(caps.known && caps.hasFindFilters) : true;
  const wideFilterBar =
    wide && onUpdateQuery
      ? html`
          <div class="filters" role="group" aria-label="Kind" data-no-swipe="true">
            ${SEARCH_KIND_CHIPS.map(
              (chip) => html`
                <button
                  key=${chip.id}
                  type="button"
                  aria-pressed=${activeKinds.has(chip.id) ? 'true' : 'false'}
                  onClick=${() => {
                    const next = new Set(activeKinds);
                    if (next.has(chip.id)) next.delete(chip.id);
                    else next.add(chip.id);
                    onUpdateQuery({ kinds: Array.from(next) });
                  }}
                >
                  ${chip.label}
                </button>
              `,
            )}
            ${showMoreFilters && onOpenFilters
              ? html`
                  <button
                    type="button"
                    aria-pressed=${hasMoreFilters ? 'true' : 'false'}
                    onClick=${onOpenFilters}
                  >
                    More filters
                  </button>
                `
              : null}
          </div>
        `
      : null;

  if (!hasCriteria) {
    return html`
      <div>
        ${wideFilterBar}
        <div class="empty">
          ${scopeMachine
            ? `Type a name or pick a kind ${wide ? 'above' : 'below'} to search in ${folderDisplayName(scopePath, { technical })}.`
            : `Type a name or pick a kind ${wide ? 'above' : 'below'} to search across your machines.`}
        </div>
      </div>
    `;
  }

  const totalMatches = machineOrder.reduce(
    (sum, mName) => sum + (Array.isArray(resultsByMachine[mName]) ? resultsByMachine[mName].length : 0),
    0,
  );
  const anyPending = machineOrder.some((mName) => resultsByMachine[mName] === undefined);

  const hasActiveFilters = kinds.length > 0 || Boolean(size) || Boolean(changed);

  return html`
    <div>
      ${wideFilterBar}
      ${error ? html`<${Notice} error=${error} />` : null}
      ${searching && Object.keys(resultsByMachine).length === 0
        ? html`<div class="empty">Searching…</div>`
        : !searching && !anyPending && totalMatches === 0
          ? html`
              <div class="empty">
                <p>Nothing found. Try a shorter word or fewer filters.</p>
                ${hasActiveFilters && onClearFilters
                  ? html`
                      <div class="empty-actions">
                        <button type="button" class="btn small" onClick=${onClearFilters}>
                          Clear filters
                        </button>
                      </div>
                    `
                  : null}
              </div>
            `
          : machineOrder.map((mName) => {
              const items = resultsByMachine[mName];
              if (items === undefined) {
                return html`
                  <div key=${mName}>
                    <p class="label">${mName}</p>
                    <div class="empty">Searching…</div>
                  </div>
                `;
              }
              if (items.length === 0) return null;
              return html`
                <div key=${mName}>
                  <p class="label">${mName}</p>
                  <${FileNameList} className="list" lines=${2} dim=${false}>
                    ${items.map((entry, idx) => {
                      const parent = parentFilePath(entry.path) || '~';
                      const sub = [
                        formatPathForDisplay(parent, { technical }),
                        entry.kind !== 'dir' && entry.size != null ? formatBytes(entry.size) : null,
                      ]
                        .filter(Boolean)
                        .join(' · ');
                      const machineHasFav = capsStore ? capsStore.hasTool(mName, 'fav.list') : caps?.hasFav;
                      const starred = machineHasFav && favKeySet.has(`${mName}\0${entry.path}`);
                      const thumbHit = thumbCache.get(thumbCacheKey(mName, entry.path, entry.mtime || '', 128));
                      const thumbUrl = thumbHit && !thumbHit.failed ? thumbHit.url : null;
                      const prevName = idx > 0 ? items[idx - 1].name : null;
                      const nextName = idx + 1 < items.length ? items[idx + 1].name : null;
                      const hitPayload = { machine: mName, entry };
                      return html`
                        <a
                          key=${entry.path}
                          class="row"
                          aria-label=${entry.name}
                          title=${entry.name}
                          href=${entry.kind === 'dir'
                            ? routeHref({ section: 'files', view: 'folder', machine: mName, path: entry.path })
                            : routeHref({ ...route, viewer: { machine: mName, path: entry.path } })}
                          onPointerDown=${(e) => searchLongPress.start(e, hitPayload)}
                          onPointerMove=${searchLongPress.move}
                          onPointerUp=${searchLongPress.cancel}
                          onPointerCancel=${searchLongPress.cancel}
                          onContextMenu=${(e) => searchLongPress.contextMenu(e, hitPayload)}
                          onClick=${(e) => {
                            e.preventDefault();
                            if (searchLongPress.consumeFired()) return;
                            if (entry.kind === 'dir') onNavigateFolder(mName, entry.path);
                            else onOpenFile(mName, entry, items);
                          }}
                        >
                          <${EntryThumb} entry=${entry} thumbUrl=${thumbUrl} />
                          <span class="body">
                            <${FileName}
                              className=${technical ? 'title mono fn-title' : 'title fn-title'}
                              name=${entry.name}
                              before=${prevName}
                              after=${nextName}
                              lines=${2}
                              dim=${false}
                              highlight=${q}
                              titleOnHover=${false}
                            />
                            <span class="sub">${sub}</span>
                          </span>
                          ${starred ? html`<span class="star" aria-label="Favourite">★</span>` : null}
                        </a>
                      `;
                    })}
                  <//>
                  ${truncatedByMachine[mName]
                    ? html`<p class="search-note">Showing the first ${items.length} matches on ${mName}. Narrow your search to see more.</p>`
                    : null}
                </div>
              `;
            })}

      ${!scopeMachine && offMachines.length > 0
        ? html`
            <p class="search-note">
              ${offMachines.length === 1
                ? `${offMachines[0].name} is off and was not searched.`
                : `${joinMemberNames(offMachines.map((m) => ({ machine: m.name })))} are off and were not searched.`}
            </p>
          `
        : null}
    </div>
  `;
}

// ── Favourites & Recent Views ────────────────────────────────────────────────────────────────

function FavouritesView({
  favourites,
  technical,
  onNavigateFolder,
  onOpenFile,
  onOpenItemSheet = null,
  onTogglePin,
  onRemoveFav,
  caps,
}) {
  const favLongPress = useItemLongPress(
    useCallback(
      (item) => {
        if (!onOpenItemSheet || !item) return;
        onOpenItemSheet(item.machine, {
          name: item.label || baseFileName(item.path),
          path: item.path,
          kind: item.kind || 'file',
          category: item.category,
        });
      },
      [onOpenItemSheet],
    ),
  );
  if (!favourites || favourites.length === 0) {
    return html`<div class="empty">Star files or folders to find them quickly from any device.</div>`;
  }
  return html`
    <${FileNameList} className="list" lines=${2}>
      ${favourites.map((item, idx) => {
        const label = item.label || baseFileName(item.path);
        const prevLabel = idx > 0 ? favourites[idx - 1].label || baseFileName(favourites[idx - 1].path) : null;
        const nextLabel =
          idx + 1 < favourites.length ? favourites[idx + 1].label || baseFileName(favourites[idx + 1].path) : null;
        const sub = [item.machine, formatPathForDisplay(item.path, { technical }), item.pinned ? 'Pinned' : null]
          .filter(Boolean)
          .join(' · ');
        const href =
          item.kind === 'dir'
            ? routeHref({ section: 'files', view: 'folder', machine: item.machine, path: item.path })
            : routeHref({ section: 'files', view: 'favourites', viewer: { machine: item.machine, path: item.path } });
        const rowEl = html`
          <a
            class="row"
            href=${href}
            aria-label=${label}
            title=${label}
            onPointerDown=${(e) => favLongPress.start(e, item)}
            onPointerMove=${favLongPress.move}
            onPointerUp=${favLongPress.cancel}
            onPointerCancel=${favLongPress.cancel}
            onContextMenu=${(e) => favLongPress.contextMenu(e, item)}
            onClick=${(e) => {
              e.preventDefault();
              if (favLongPress.consumeFired()) return;
              if (item.kind === 'dir') onNavigateFolder(item.machine, item.path);
              else
                onOpenFile(item.machine, {
                  name: label,
                  path: item.path,
                  kind: 'file',
                  category: item.category,
                });
            }}
          >
            <span class=${item.kind === 'dir' ? 'thumb folder' : 'thumb'} aria-hidden="true">
              <${Icon} name=${item.kind === 'dir' ? 'files' : entryIconName(item)} />
            </span>
            <span class="body">
              <${FileName}
                className="title fn-title"
                name=${label}
                before=${prevLabel}
                after=${nextLabel}
                lines=${2}
                titleOnHover=${false}
              />
              <span class="sub">${sub}</span>
            </span>
            <span class="star" aria-label="Favourite">★</span>
          </a>
        `;
        if (!caps?.hasFavWrite) return html`<div key=${item.id}>${rowEl}</div>`;
        return html`
          <${SwipeRow}
            key=${item.id}
            actions=${[
              {
                label: item.pinned ? 'Unpin' : 'Pin',
                tone: 'neutral',
                onAction: () => onTogglePin && onTogglePin(item),
              },
              {
                label: 'Remove',
                tone: 'danger',
                onAction: () => onRemoveFav && onRemoveFav(item),
              },
            ]}
          >
            ${rowEl}
          <//>
        `;
      })}
    <//>
  `;
}

function RecentView({ recentFiles, deviceNoun, technical, onOpenFile, onOpenItemSheet = null }) {
  const recentLongPress = useItemLongPress(
    useCallback(
      (item) => {
        if (!onOpenItemSheet || !item) return;
        onOpenItemSheet(item.machine, {
          name: item.name || baseFileName(item.path),
          path: item.path,
          kind: item.kind || 'file',
          category: item.category,
          size: item.size,
          mtime: item.mtime,
        });
      },
      [onOpenItemSheet],
    ),
  );
  if (!recentFiles || recentFiles.length === 0) {
    return html`<div class="empty">Files you open on this ${deviceNoun} appear here.</div>`;
  }
  return html`
    <${FileNameList} className="list" lines=${2}>
      ${recentFiles.map((item, idx) => {
        const label = item.name || baseFileName(item.path);
        const prevLabel = idx > 0 ? recentFiles[idx - 1].name || baseFileName(recentFiles[idx - 1].path) : null;
        const nextLabel =
          idx + 1 < recentFiles.length ? recentFiles[idx + 1].name || baseFileName(recentFiles[idx + 1].path) : null;
        const sub = [
          item.machine,
          item.size != null ? formatBytes(item.size) : null,
          item.openedAt ? formatRelativeTime(item.openedAt) : null,
        ]
          .filter(Boolean)
          .join(' · ');
        return html`
          <a
            key=${`${item.machine}:${item.path}`}
            class="row"
            href=${routeHref({ section: 'files', view: 'recent', viewer: { machine: item.machine, path: item.path } })}
            aria-label=${label}
            title=${label}
            onPointerDown=${(e) => recentLongPress.start(e, item)}
            onPointerMove=${recentLongPress.move}
            onPointerUp=${recentLongPress.cancel}
            onPointerCancel=${recentLongPress.cancel}
            onContextMenu=${(e) => recentLongPress.contextMenu(e, item)}
            onClick=${(e) => {
              e.preventDefault();
              if (recentLongPress.consumeFired()) return;
              onOpenFile(item.machine, item);
            }}
          >
            <span class="thumb" aria-hidden="true">
              <${Icon} name=${entryIconName(item)} />
            </span>
            <span class="body">
              <${FileName}
                className=${technical ? 'title mono fn-title' : 'title fn-title'}
                name=${label}
                before=${prevLabel}
                after=${nextLabel}
                lines=${2}
                titleOnHover=${false}
              />
              <span class="sub">${sub}</span>
            </span>
          </a>
        `;
      })}
    <//>
  `;
}

// ── Sync Folder Detail View ──────────────────────────────────────────────────────────────────

function SyncFolderView({
  syncId,
  machines,
  link,
  technical,
  onRegisterRefresh = null,
  onNavigateFolder,
  onRequestConfirm,
  onSyncChanged,
  onOpenOverlay,
  onCloseSync,
}) {
  const [detail, setDetail] = useState(null);
  const [error, setError] = useState(null);
  const [busyPause, setBusyPause] = useState(false);

  const load = useCallback(() => {
    setError(null);
    return link
      .call('sync.status', { id: syncId })
      .then((res) => setDetail(res))
      .catch((err) => setError(err));
  }, [link, syncId]);

  useEffect(() => {
    load();
  }, [load]);

  useEffect(() => {
    if (typeof onRegisterRefresh !== 'function') return undefined;
    return onRegisterRefresh(() => load());
  }, [onRegisterRefresh, load]);

  if (error) return html`<${Notice} error=${error} />`;
  if (!detail) return html`<div class="empty">Loading synced folder…</div>`;

  const onlineMap = new Map((machines || []).map((m) => [m.name, m]));
  const firstOnlineMember = (detail.members || []).find((mem) => onlineMap.get(mem.machine)?.reachable);

  async function togglePause(checked) {
    setBusyPause(true);
    try {
      await link.call(checked ? 'sync.pause' : 'sync.resume', { id: detail.id });
      load();
      if (onSyncChanged) onSyncChanged();
    } catch (err) {
      setError(err);
    } finally {
      setBusyPause(false);
    }
  }

  function confirmStopSync() {
    if (!onRequestConfirm) return;
    onRequestConfirm({
      title: `Stop keeping ${detail.name} in sync?`,
      summary: 'The files stay on every machine, and future changes will no longer be copied between them.',
      confirmLabel: 'Stop syncing',
      danger: true,
      canDryRun: false,
      async onConfirm() {
        await link.call('sync.delete', { id: detail.id });
        if (onSyncChanged) onSyncChanged();
        if (onCloseSync) onCloseSync();
      },
    });
  }

  const conflictsCount = detail.conflicts_count ?? detail.conflicts?.length ?? 0;
  const binCount = detail.bin_count ?? detail.bin?.length ?? 0;

  return html`
    <div>
      ${firstOnlineMember
        ? html`
            <div class="list">
              <a
                class="row"
                href=${routeHref({
                  section: 'files',
                  view: 'folder',
                  machine: firstOnlineMember.machine,
                  path: firstOnlineMember.path,
                })}
                onClick=${(e) => {
                  e.preventDefault();
                  onNavigateFolder(firstOnlineMember.machine, firstOnlineMember.path);
                }}
              >
                <span class="thumb folder" aria-hidden="true"><${Icon} name="files" /></span>
                <span class="body">
                  <span class="title">Open the folder</span>
                  <span class="sub">
                    ${`${(detail.file_count || 0).toLocaleString()} files · ${formatBytes(detail.total_bytes || 0)}`}
                  </span>
                </span>
                <span class="chev" aria-hidden="true">›</span>
              </a>
            </div>
          `
        : null}

      <p class="label">Copies</p>
      <div class="list">
        ${(detail.members || []).map((mem) => {
          const m = onlineMap.get(mem.machine);
          const isOff = m ? !m.reachable : mem.state === 'waiting';
          const isBusy = !isOff && mem.state === 'syncing';
          const dotCls = isOff ? 'dot off' : isBusy ? 'dot busy' : 'dot';
          const showPath = mem.machine === detail.members?.[0]?.machine || technical;
          const sentence =
            mem.sentence ||
            (isOff ? 'Off' : isBusy ? 'Syncing…' : detail.paused ? 'Paused' : 'Up to date');
          const subText = showPath && !isBusy && !isOff
            ? `${sentence} · ${formatPathForDisplay(mem.path, { technical })}`
            : sentence;
          return html`
            <a
              key=${mem.machine}
              class=${isOff ? 'row off' : 'row'}
              href=${routeHref({ section: 'files', view: 'folder', machine: mem.machine, path: mem.path })}
              onClick=${(e) => {
                e.preventDefault();
                onNavigateFolder(mem.machine, mem.path);
              }}
            >
              <span class=${dotCls} aria-hidden="true"></span>
              <span class="body">
                <span class="title">${mem.machine}</span>
                <span class="sub">
                  ${subText}
                  ${isBusy && typeof mem.progress_pct === 'number'
                    ? html`<${ProgressBar} value=${mem.progress_pct} />`
                    : null}
                </span>
              </span>
              <span class="chev" aria-hidden="true">›</span>
            </a>
          `;
        })}
      </div>

      <div class="sheet-btn-stack">
        <button
          type="button"
          class="btn"
          onClick=${() => onOpenOverlay && onOpenOverlay({ kind: 'sync-add', syncFolder: detail, onChanged: () => { load(); if (onSyncChanged) onSyncChanged(); } })}
        >
          Add a machine
        </button>
      </div>

      ${conflictsCount > 0 || binCount > 0
        ? html`
            <p class="label">To look at</p>
            <div class="list">
              ${conflictsCount > 0
                ? html`
                    <button
                      type="button"
                      class="row"
                      onClick=${() =>
                        onOpenOverlay &&
                        onOpenOverlay({
                          kind: 'sync-conflicts',
                          syncFolder: detail,
                          onChanged: () => {
                            load();
                            if (onSyncChanged) onSyncChanged();
                          },
                        })}
                    >
                      <span class="body">
                        <span class="title">
                          ${conflictsCount === 1 ? '1 file has two versions' : `${conflictsCount} files have two versions`}
                        </span>
                        <span class="sub wrap">
                          ${detail.conflicts?.[0]?.summary || 'Changed on two machines while apart. Both are kept.'}
                        </span>
                      </span>
                      <span class="chev" aria-hidden="true">›</span>
                    </button>
                  `
                : null}
              ${binCount > 0
                ? html`
                    <button
                      type="button"
                      class="row"
                      onClick=${() =>
                        onOpenOverlay &&
                        onOpenOverlay({
                          kind: 'sync-bin',
                          syncFolder: detail,
                          onChanged: () => {
                            load();
                            if (onSyncChanged) onSyncChanged();
                          },
                        })}
                    >
                      <span class="body">
                        <span class="title">Bin</span>
                        <span class="sub">
                          ${binCount === 1 ? '1 deleted file · kept for 30 days' : `${binCount} deleted files · kept for 30 days`}
                        </span>
                      </span>
                      <span class="chev" aria-hidden="true">›</span>
                    </button>
                  `
                : null}
            </div>
          `
        : null}

      <p class="label">This folder</p>
      <div class="list">
        <label class="row">
          <span class="body"><span class="title">Pause syncing</span></span>
          <span class="switch row-switch">
            <input
              type="checkbox"
              disabled=${busyPause}
              checked=${Boolean(detail.paused)}
              onChange=${(e) => togglePause(e.target.checked)}
            />
          </span>
        </label>
        <button type="button" class="row" onClick=${confirmStopSync}>
          <span class="body">
            <span class="title">Stop keeping it in sync</span>
            <span class="sub wrap">The files stay on every machine</span>
          </span>
          <span class="chev" aria-hidden="true">›</span>
        </button>
      </div>
    </div>
  `;
}

// ── Top-level FilesView orchestrating screens, viewer, and sheets ────────────────────────────

export function FilesView({
  route,
  wide,
  snapshot,
  link,
  capsStore = null,
  folderChanges = null,
  onRegisterRefresh = null,
  onRegisterPopState = null,
  nodeInfo,
  tools,
  allowedSet,
  settings,
  deviceNoun,
  thumbCache,
  filesState,
  filesActions,
  wakePendingSet = null,
  onRefreshFleet,
  onEnsureAwake,
  onRequestConfirm,
  onToast,
}) {
  const machines = snapshot?.machines || [];
  const technical = Boolean(settings?.technical);
  const localNode = nodeInfo?.node || machines.find((m) => m.reachable)?.name || null;

  // Determine active machine/folder for wide split detail when on `#/files`.
  const defaultWideMachine = useMemo(() => {
    if (link?.mockFlags?.has('emptyright')) return null;
    return machines.find((m) => m.reachable)?.name || machines[0]?.name || null;
  }, [machines, link]);
  const activeFolderMachine =
    route.view === 'folder' ? route.machine : wide && route.view === 'home' ? defaultWideMachine : null;
  const activeFolderPath = route.view === 'folder' ? route.path || '~' : '~';
  const activeMachineObj = activeFolderMachine
    ? machines.find((m) => m.name === activeFolderMachine) || null
    : null;
  const viewerMachine = route.viewer?.machine || null;
  const searchMachine = route.view === 'search' ? route.machine || null : null;

  const { caps: localCaps } = useMachineCaps(capsStore, localNode, {
    fallbackTools: tools,
    fallbackAllowedSet: allowedSet,
    link,
    enabled: Boolean(localNode),
  });
  const {
    caps: folderCaps,
    entry: folderCapsEntry,
    missing: folderMissing,
  } = useMachineCaps(capsStore, activeFolderMachine, {
    fallbackTools: tools,
    fallbackAllowedSet: allowedSet,
    link,
    enabled: Boolean(activeFolderMachine && (!activeMachineObj || activeMachineObj.reachable)),
  });
  const { caps: viewerCaps } = useMachineCaps(capsStore, viewerMachine, {
    fallbackTools: tools,
    fallbackAllowedSet: allowedSet,
    link,
    enabled: Boolean(viewerMachine),
  });
  const { caps: scopedSearchCaps } = useMachineCaps(capsStore, searchMachine, {
    fallbackTools: tools,
    fallbackAllowedSet: allowedSet,
    link,
    enabled: Boolean(searchMachine),
  });

  // Effective capabilities for the currently active view/dock.
  const activeSearchCaps = useMemo(() => {
    if (searchMachine) return scopedSearchCaps;
    if (!capsStore) return localCaps;
    const online = machines.filter((m) => m.reachable);
    const knownOnline = online.filter((m) => capsStore.isKnown(m.name));
    if (knownOnline.length === 0) return localCaps;
    const anyFindFilters = knownOnline.some((m) => capsStore.fileCapabilities(m.name).hasFindFilters);
    const anyFindEnhanced = knownOnline.some((m) => capsStore.fileCapabilities(m.name).hasFindEnhanced);
    return {
      ...localCaps,
      known: true,
      hasFindEnhanced: anyFindEnhanced,
      hasFindFilters: anyFindFilters,
    };
  }, [searchMachine, scopedSearchCaps, capsStore, localCaps, machines]);

  const dockCaps =
    route.view === 'search'
      ? activeSearchCaps
      : activeFolderMachine
        ? folderCaps
        : localCaps;

  const [favourites, setFavourites] = useState([]);
  const [syncFolders, setSyncFolders] = useState([]);
  const [recentFiles, setRecentFiles] = useState(() => readRecentFiles(safeStorage()));
  const [folderEntries, setFolderEntries] = useState([]);
  const [resolvedFolderPath, setResolvedFolderPath] = useState(route.path || '~');
  const [folderSummary, setFolderSummary] = useState({ totalCount: null, totalExact: true });
  const [wideSearchSummary, setWideSearchSummary] = useState({ count: 0, machineCount: 0, searching: false });
  const [wideSearchText, setWideSearchText] = useState(route.query?.q || '');
  const wideCrumbsRef = useRef(null);
  const [reloadToken, setReloadToken] = useState(0);
  const [viewerEntryCache, setViewerEntryCache] = useState(null);
  const migratedFavNodeRef = useRef(null);
  const [returnTarget, setReturnTarget] = useState(null);
  const returnTokenRef = useRef(0);
  const viewerTrackRef = useRef({ machine: null, openedPath: null, currentPath: null });
  const prevFolderRef = useRef({ machine: null, path: null });

  // Track viewer open/sibling-swipe/close so closing the viewer scrolls back to the file we ended
  // on (even when swiping far down a virtualized folder list).
  useEffect(() => {
    const prev = viewerTrackRef.current;
    if (route.viewer) {
      if (!prev.currentPath || prev.machine !== route.viewer.machine) {
        viewerTrackRef.current = {
          machine: route.viewer.machine,
          openedPath: route.viewer.path,
          currentPath: route.viewer.path,
        };
      } else {
        viewerTrackRef.current = {
          ...prev,
          currentPath: route.viewer.path,
        };
      }
      return;
    }
    if (prev.currentPath) {
      const closed = prev;
      viewerTrackRef.current = { machine: null, openedPath: null, currentPath: null };
      if (closed.machine && closed.machine === activeFolderMachine) {
        const swipedToOther = closed.currentPath !== closed.openedPath;
        returnTokenRef.current += 1;
        setReturnTarget({
          machine: closed.machine,
          path: closed.currentPath,
          onlyIfOffscreen: !swipedToOther,
          token: returnTokenRef.current,
        });
      }
    }
  }, [route.viewer?.machine, route.viewer?.path, activeFolderMachine]);

  // Returning from a subfolder (`dir/Sub` -> `dir`) scrolls `dir/Sub` into view if off-screen.
  useEffect(() => {
    const prev = prevFolderRef.current;
    const curMachine = activeFolderMachine || null;
    const curPath = activeFolderPath || null;
    prevFolderRef.current = { machine: curMachine, path: curPath };
    if (
      prev.machine &&
      curMachine === prev.machine &&
      prev.path &&
      curPath &&
      prev.path !== curPath
    ) {
      const parentOfPrev = parentFilePath(prev.path);
      if (
        parentOfPrev &&
        (parentOfPrev === curPath || (curPath === '~' && parentOfPrev === resolvedFolderPath))
      ) {
        returnTokenRef.current += 1;
        setReturnTarget({
          machine: curMachine,
          path: prev.path,
          onlyIfOffscreen: true,
          token: returnTokenRef.current,
        });
      }
    }
  }, [activeFolderMachine, activeFolderPath, resolvedFolderPath]);

  useEffect(() => {
    setResolvedFolderPath(activeFolderPath || '~');
    setFolderSummary({ totalCount: null, totalExact: true });
  }, [activeFolderMachine, activeFolderPath]);

  useEffect(() => {
    setWideSearchText(route.query?.q || '');
  }, [route.query?.q, route.view]);

  useEffect(() => {
    if (wideCrumbsRef.current) {
      wideCrumbsRef.current.scrollLeft = wideCrumbsRef.current.scrollWidth;
    }
  }, [resolvedFolderPath, activeFolderMachine]);

  useEffect(() => {
    setRecentFiles(readRecentFiles(safeStorage()));
  }, [localNode]);

  const loadFavourites = useCallback(() => {
    if (!localCaps.hasFav || !localNode) {
      setFavourites([]);
      return Promise.resolve();
    }
    const req = capsStore
      ? capsStore.call('fav.list', {}, { on: localNode }).then((r) => r.value)
      : link.call('fav.list', {}, { on: localNode });
    return req
      .then(async (res) => {
        const rawEntries = Array.isArray(res?.entries) ? res.entries : [];
        const normalized = isRealMachineName(localNode)
          ? normalizeFavouritesMachine(rawEntries, localNode)
          : rawEntries;
        setFavourites(normalized);

        if (
          isRealMachineName(localNode) &&
          localCaps.hasFavWrite &&
          migratedFavNodeRef.current !== localNode &&
          rawEntries.some((e) => e?.machine === 'local' || e?.machine === PENDING_LOCAL_MACHINE_NAME)
        ) {
          migratedFavNodeRef.current = localNode;
          const outcome = await migrateLocalFavouritesOnNode({
            entries: rawEntries,
            realNodeName: localNode,
            link,
            localNode,
            canWrite: true,
          });
          if (outcome.migrated) {
            const refreshed = await (capsStore
              ? capsStore.call('fav.list', {}, { on: localNode }).then((r) => r.value)
              : link.call('fav.list', {}, { on: localNode })
            ).catch(() => null);
            if (Array.isArray(refreshed?.entries)) {
              setFavourites(normalizeFavouritesMachine(refreshed.entries, localNode));
            }
          }
        }
      })
      .catch(() => setFavourites([]));
  }, [localCaps.hasFav, localCaps.hasFavWrite, capsStore, link, localNode]);

  const loadSyncFolders = useCallback(() => {
    if (!localCaps.hasSync || !localNode) {
      setSyncFolders([]);
      return Promise.resolve();
    }
    const req = capsStore
      ? capsStore.call('sync.list', {}, { on: localNode }).then((r) => r.value)
      : link.call('sync.list', {}, { on: localNode });
    return req
      .then((res) => setSyncFolders(Array.isArray(res?.folders) ? res.folders : []))
      .catch(() => setSyncFolders([]));
  }, [localCaps.hasSync, capsStore, link, localNode]);

  useEffect(() => {
    loadFavourites();
    loadSyncFolders();
  }, [loadFavourites, loadSyncFolders]);

  const refreshFilesHome = useCallback(async () => {
    setRecentFiles(readRecentFiles(safeStorage()));
    await Promise.all([
      loadFavourites(),
      loadSyncFolders(),
      onRefreshFleet ? onRefreshFleet({ forceOffline: true }) : Promise.resolve(),
    ]);
  }, [loadFavourites, loadSyncFolders, onRefreshFleet]);

  useEffect(() => {
    if (typeof onRegisterRefresh !== 'function' || wide) return undefined;
    if (route.view === 'home' || route.view === 'favourites' || route.view === 'recent') {
      return onRegisterRefresh(refreshFilesHome);
    }
    return undefined;
  }, [onRegisterRefresh, wide, route.view, refreshFilesHome]);

  // Share caps & current folder metadata with shell header/dock.
  useEffect(() => {
    filesActions.setMeta({
      caps: dockCaps,
      resolvedPath: resolvedFolderPath,
      entries: folderEntries,
      syncFolders,
    });
  }, [dockCaps, resolvedFolderPath, folderEntries, syncFolders]);

  const navigateHref = useCallback(
    (href) => {
      filesActions.pushHref(href);
    },
    [filesActions],
  );

  const navigateFolder = useCallback(
    (machine, dirPath) => {
      navigateHref(routeHref({ section: 'files', view: 'folder', machine, path: dirPath }));
    },
    [navigateHref],
  );

  const openFile = useCallback(
    (machine, entry, siblings = null) => {
      const nextRecent = recordRecentFile(safeStorage(), {
        machine,
        path: entry.path,
        name: entry.name || baseFileName(entry.path),
        kind: entry.kind || 'file',
        category: entry.category || entryCategory(entry),
        size: entry.size,
        mtime: entry.mtime,
      });
      setRecentFiles(nextRecent);
      setViewerEntryCache({ machine, entry, siblings: siblings || folderEntries });
      navigateHref(routeHref({ ...route, viewer: { machine, path: entry.path } }));
    },
    [route, folderEntries, navigateHref],
  );

  const machineCapsFor = useCallback(
    (machine) => {
      if (capsStore && machine) {
        return capsStore.fileCapabilities(machine, { fallbackAllowedSet: allowedSet });
      }
      return folderCaps;
    },
    [capsStore, allowedSet, folderCaps],
  );

  const toggleStar = useCallback(
    async (machine, entry) => {
      const targetCaps = machineCapsFor(machine);
      if (!localCaps.hasFavWrite || (machine && !targetCaps.hasFavWrite)) return;
      const existing = favourites.find((f) => f.machine === machine && f.path === entry.path);
      try {
        if (existing) {
          await link.call('fav.remove', { id: existing.id }, { on: localNode });
          if (onToast) onToast(`Removed ${entry.name || baseFileName(entry.path)} from Favourites.`);
        } else {
          await link.call(
            'fav.add',
            { machine, path: entry.path, label: entry.name || baseFileName(entry.path), pinned: false },
            { on: localNode },
          );
          if (onToast) onToast(`Starred ${entry.name || baseFileName(entry.path)}.`);
        }
        loadFavourites();
      } catch (err) {
        if (onToast) onToast(plainErrorMessage(err, 'Could not update favourite.', { technical, deviceNoun }));
      }
    },
    [localCaps.hasFavWrite, machineCapsFor, favourites, link, localNode, loadFavourites, onToast, technical, deviceNoun],
  );

  const togglePin = useCallback(
    async (machine, entry) => {
      const targetCaps = machineCapsFor(machine);
      if (!localCaps.hasFavWrite || (machine && !targetCaps.hasFavWrite)) return;
      const existing = favourites.find((f) => f.machine === machine && f.path === entry.path);
      try {
        if (existing) {
          const nextPin = !existing.pinned;
          await link.call('fav.set', { id: existing.id, pinned: nextPin }, { on: localNode });
          if (onToast) onToast(nextPin ? `Pinned ${existing.label} to Files.` : `Unpinned ${existing.label}.`);
        } else {
          const label = entry.name || folderDisplayName(entry.path);
          await link.call('fav.add', { machine, path: entry.path, label, pinned: true }, { on: localNode });
          if (onToast) onToast(`Pinned ${label} to Files.`);
        }
        loadFavourites();
      } catch (err) {
        if (onToast) onToast(plainErrorMessage(err, 'Could not update pin.', { technical, deviceNoun }));
      }
    },
    [localCaps.hasFavWrite, machineCapsFor, favourites, link, localNode, loadFavourites, onToast, technical, deviceNoun],
  );

  const startDownload = useCallback(
    async (machine, items) => {
      const targetCaps = machineCapsFor(machine);
      if (!targetCaps.hasDownload || typeof link?.download !== 'function') return;
      const list = Array.isArray(items) ? items.filter((i) => i.kind !== 'dir') : [items];
      if (list.length === 0) return;
      let firstError = null;
      const results = await Promise.all(
        list.map((item) =>
          link
            .download({
              machine,
              path: item.path,
              name: item.name || baseFileName(item.path),
              size: item.size,
              mime: item.mime,
            })
            .catch((err) => {
              if (!firstError) firstError = err;
              return null;
            }),
        ),
      );
      const started = results.filter(Boolean);
      const transferIds = started.map((r) => r?.id).filter(Boolean);
      if (onToast) {
        if (started.length === 0 && firstError) {
          onToast(plainErrorMessage(firstError, 'Could not start download.', { technical, deviceNoun }), { tone: 'error' });
          return;
        }
        onToast(
          list.length === 1
            ? `Downloading ${list[0].name || baseFileName(list[0].path)}. Watch progress in Activity.`
            : `Downloading ${list.length} items. Watch progress in Activity.`,
          { actionLabel: 'Activity', actionHref: '#/activity', transferIds },
        );
      }
    },
    [machineCapsFor, link, onToast, technical, deviceNoun],
  );

  const startUpload = useCallback(
    (machine, dir) => {
      const targetCaps = machineCapsFor(machine);
      if (!targetCaps.hasUpload || typeof link?.upload !== 'function') return;
      link
        .upload({ machine, dir })
        .then((res) => {
          if (!res) return;
          const transferIds =
            Array.isArray(res.ids) && res.ids.length > 0
              ? res.ids.filter(Boolean)
              : res.id
                ? [res.id]
                : [];
          if (onToast) {
            onToast(`Uploading to ${machine}. Watch progress in Activity.`, {
              actionLabel: 'Activity',
              actionHref: '#/activity',
              transferIds,
            });
          }
        })
        .catch((err) => {
          if (onToast && err?.code !== 'cancelled') {
            onToast(
              plainErrorMessage(err, `Could not upload to ${machine}.`, { technical, deviceNoun }),
              { tone: 'error' },
            );
          }
        });
    },
    [machineCapsFor, link, onToast, technical, deviceNoun],
  );

  const startOpenExternally = useCallback(
    (machine, entry) => {
      const targetCaps = machineCapsFor(machine);
      if (!targetCaps.hasOpenExternally || typeof link?.openExternally !== 'function') {
        return Promise.resolve(null);
      }
      return link.openExternally({
        machine,
        path: entry.path,
        name: entry.name || baseFileName(entry.path),
        size: entry.size,
        mime: entry.mime,
      });
    },
    [machineCapsFor, link],
  );

  const requestDelete = useCallback(
    (machine, items) => {
      const targetCaps = machineCapsFor(machine);
      if (!targetCaps.hasRemove || !onRequestConfirm || !items?.length) return;
      const single = items.length === 1 ? items[0] : null;
      const label = single ? single.name || baseFileName(single.path) : `${items.length} items`;
      onRequestConfirm({
        title: `Delete ${label}?`,
        summary: single
          ? `Remove ${label} from ${machine}. This cannot be undone.`
          : `Remove ${items.length} items from ${machine}. This cannot be undone.`,
        tool: 'fs.remove',
        targetName: machine,
        args: single ? { path: single.path, recursive: single.kind === 'dir' } : {},
        effect: 'destroy',
        confirmLabel: 'Delete',
        danger: true,
        canDryRun: Boolean(single),
        async onDryRun() {
          if (capsStore) {
            const res = await capsStore.call(
              'fs.remove',
              { path: single.path, recursive: single.kind === 'dir', dry_run: true },
              { on: machine, optional: ['recursive'] },
            );
            return res.value;
          }
          return link.call('fs.remove', { path: single.path, recursive: single.kind === 'dir', dry_run: true }, { on: machine });
        },
        async onConfirm() {
          const touchedParents = new Set();
          for (let idx = 0; idx < items.length; idx += 1) {
            const item = items[idx];
            if (capsStore) {
              await capsStore.call(
                'fs.remove',
                { path: item.path, recursive: item.kind === 'dir' },
                { on: machine, optional: ['recursive'] },
              );
            } else {
              await link.call('fs.remove', { path: item.path, recursive: item.kind === 'dir' }, { on: machine });
            }
            const parent = parentFilePath(item.path) || '~';
            touchedParents.add(parent);
            if (folderChanges && idx < items.length - 1) {
              folderChanges.changed(machine, parent, { throttle: true, reason: 'delete' });
            }
          }
          filesActions.endSelect();
          if (route.viewer && items.some((i) => i.path === route.viewer.path)) {
            filesActions.closeViewer();
          }
          if (folderChanges) {
            for (const parent of touchedParents) {
              folderChanges.changed(machine, parent, { throttle: false, reason: 'delete' });
            }
          } else {
            setReloadToken((n) => n + 1);
          }
          loadFavourites();
          if (onToast) onToast(`Deleted ${label}.`);
        },
      });
    },
    [machineCapsFor, onRequestConfirm, capsStore, folderChanges, link, filesActions, route.viewer, loadFavourites, onToast],
  );

  // Wire bulk selection actions triggered from the shell dock.
  useEffect(() => {
    filesActions.registerHandlers({
      onBulkSend(machine, items) {
        filesActions.openSheet({ kind: 'send', sourceMachine: machine, items });
      },
      onBulkDownload(machine, items) {
        startDownload(machine, items);
        filesActions.endSelect();
      },
      async onBulkStar(machine, items) {
        for (const item of items) {
          await toggleStar(machine, item);
        }
        filesActions.endSelect();
      },
      onBulkDelete(machine, items) {
        requestDelete(machine, items);
      },
    });
  }, [filesActions, startDownload, toggleStar, requestDelete]);

  const openItemSheet = useCallback(
    (machine, entry, { canSelect = false } = {}) => {
      if (!machine || !entry) return;
      filesActions.openSheet({ kind: 'item', machine, entry, canSelect });
    },
    [filesActions],
  );

  const handleReturnHandled = useCallback((token) => {
    setReturnTarget((cur) => (cur && cur.token === token ? null : cur));
  }, []);

  // Clear any stale return target when toggling view mode, sort, or filter so switching views
  // never unexpectedly re-scrolls to a previously viewed file.
  useEffect(() => {
    setReturnTarget(null);
  }, [filesState.viewModeOverride, filesState.sortPref.sort, filesState.sortPref.order, filesState.kindFilter]);

  const folderViewEl = activeFolderMachine
    ? html`
        <${FolderBrowser}
          machine=${activeFolderMachine}
          machineObj=${activeMachineObj}
          path=${activeFolderPath}
          link=${link}
          caps=${folderCaps}
          capsStore=${capsStore}
          capsEntry=${folderCapsEntry}
          missingFeatures=${folderMissing}
          folderChanges=${folderChanges}
          onPruneSelection=${filesActions.pruneSelection}
          onRegisterRefresh=${wide ? null : onRegisterRefresh}
          allowedSet=${allowedSet}
          technical=${technical}
          deviceNoun=${deviceNoun}
          favourites=${favourites}
          thumbCache=${thumbCache}
          sortPref=${filesState.sortPref}
          kindFilter=${filesState.kindFilter || ''}
          onClearKindFilter=${() => filesActions.setKindFilter?.('')}
          showHidden=${filesState.showHidden}
          selecting=${filesState.selecting}
          selectedPaths=${filesState.selectedPaths}
          viewModeOverride=${filesState.viewModeOverride}
          onViewModeResolved=${(m, rPath) => filesActions.setResolvedViewMode(m, rPath)}
          onEntriesChange=${(list, rPath) => {
            setFolderEntries(list);
            setResolvedFolderPath(rPath);
          }}
          onTotalChange=${({ totalCount, totalExact, resolvedPath: rPath }) => {
            setFolderSummary((prev) =>
              prev.totalCount === totalCount && prev.totalExact === totalExact
                ? prev
                : { totalCount, totalExact },
            );
            if (rPath) setResolvedFolderPath(rPath);
          }}
          onNavigateFolder=${navigateFolder}
          onOpenFile=${openFile}
          onOpenItemSheet=${openItemSheet}
          onToggleSelect=${filesActions.toggleSelect}
          onStartSelectWith=${filesActions.startSelect}
          onToggleStar=${toggleStar}
          onSendTo=${(m, items) => filesActions.openSheet({ kind: 'send', sourceMachine: m, items })}
          onEnsureAwake=${onEnsureAwake}
          wakePendingSet=${wakePendingSet}
          onRefreshFleet=${onRefreshFleet}
          onNewFolder=${(m, p) => filesActions.openSheet({ kind: 'mkdir', machine: m, parentPath: p })}
          onUpload=${startUpload}
          onGoHome=${() => navigateHref('#/files')}
          onOpenSearch=${(q, m, p) =>
            navigateHref(
              routeHref({
                section: 'files',
                view: 'search',
                machine: m,
                path: p,
                query: { q: q || '', kinds: [], size: '', changed: '' },
              }),
            )}
          onOpenSort=${() => filesActions.openSheet({ kind: 'sort' })}
          onToggleView=${() => {
            const next = filesState.resolvedViewMode === 'grid' ? 'list' : 'grid';
            writeFolderViewMode(safeStorage(), activeFolderMachine, resolvedFolderPath, next);
            filesActions.setViewModeOverride(next);
          }}
          onOpenMore=${() => filesActions.openSheet({ kind: 'more', machine: activeFolderMachine, path: resolvedFolderPath })}
          wide=${wide}
          hideWideChrome=${wide}
          reloadToken=${reloadToken}
          returnTarget=${returnTarget}
          onReturnHandled=${handleReturnHandled}
        />
      `
    : null;

  const updateWideSearchRoute = useCallback(
    (patch) => {
      const nextQuery = {
        q: route.query?.q || '',
        kinds: route.query?.kinds || [],
        size: route.query?.size || '',
        changed: route.query?.changed || '',
        ...patch,
      };
      const href = routeHref({ ...route, query: nextQuery });
      window.history.replaceState(window.history.state, '', href);
      window.dispatchEvent(new HashChangeEvent('hashchange'));
    },
    [route],
  );

  function renderMainContent() {
    if (route.view === 'search') {
      return html`
        <${FilesSearchView}
          route=${route}
          machines=${machines}
          favourites=${favourites}
          caps=${activeSearchCaps}
          capsStore=${capsStore}
          link=${link}
          technical=${technical}
          thumbCache=${thumbCache}
          wide=${wide}
          onRegisterRefresh=${wide ? null : onRegisterRefresh}
          onNavigateFolder=${navigateFolder}
          onOpenFile=${openFile}
          onOpenItemSheet=${openItemSheet}
          onResultCountChange=${(summary) => {
            setWideSearchSummary(summary);
            filesActions.setSearchSummary(summary);
          }}
          onClearFilters=${() => {
            updateWideSearchRoute({ kinds: [], size: '', changed: '' });
          }}
          onUpdateQuery=${wide ? updateWideSearchRoute : null}
          onOpenFilters=${wide ? () => filesActions.openSheet({ kind: 'filters' }) : null}
        />
      `;
    }
    if (route.view === 'favourites') {
      return html`
        <${FavouritesView}
          favourites=${favourites}
          technical=${technical}
          caps=${localCaps}
          onNavigateFolder=${navigateFolder}
          onOpenFile=${openFile}
          onOpenItemSheet=${openItemSheet}
          onTogglePin=${(item) => togglePin(item.machine, { path: item.path, name: item.label })}
          onRemoveFav=${(item) => toggleStar(item.machine, { path: item.path, name: item.label })}
        />
      `;
    }
    if (route.view === 'recent') {
      return html`
        <${RecentView}
          recentFiles=${recentFiles}
          deviceNoun=${deviceNoun}
          technical=${technical}
          onOpenFile=${openFile}
          onOpenItemSheet=${openItemSheet}
        />
      `;
    }
    if (route.view === 'sync' && route.syncId) {
      return html`
        <${SyncFolderView}
          syncId=${route.syncId}
          machines=${machines}
          link=${link}
          technical=${technical}
          onRegisterRefresh=${wide ? null : onRegisterRefresh}
          onNavigateFolder=${navigateFolder}
          onRequestConfirm=${onRequestConfirm}
          onSyncChanged=${loadSyncFolders}
          onOpenOverlay=${filesActions.openSheet}
          onCloseSync=${() => navigateHref('#/files')}
        />
      `;
    }
    if (route.view === 'folder') {
      return folderViewEl;
    }
    return html`
      <${FilesHome}
        machines=${machines}
        favourites=${favourites}
        syncFolders=${syncFolders}
        recentFiles=${recentFiles}
        caps=${localCaps}
        capsStore=${capsStore}
        deviceNoun=${deviceNoun}
        link=${link}
        thumbCache=${thumbCache}
        wide=${false}
        onNavigate=${navigateHref}
        onOpenFile=${openFile}
        onOpenItemSheet=${openItemSheet}
        onStartSearch=${(q) =>
          navigateHref(routeHref({ section: 'files', view: 'search', query: { q: q || '', kinds: [], size: '', changed: '' } }))}
      />
    `;
  }

  // Viewer resolution (works both from click and after reload/deep-link).
  const viewer = route.viewer;
  const viewerEntry = useMemo(() => {
    if (!viewer) return null;
    const fromFolder = folderEntries.find((e) => e.path === viewer.path);
    if (fromFolder) return fromFolder;
    if (viewerEntryCache && viewerEntryCache.machine === viewer.machine && viewerEntryCache.entry?.path === viewer.path) {
      return viewerEntryCache.entry;
    }
    return { name: baseFileName(viewer.path), path: viewer.path, kind: 'file' };
  }, [viewer, folderEntries, viewerEntryCache]);

  const viewerSiblings = useMemo(() => {
    if (!viewer) return [];
    if (folderEntries.some((e) => e.path === viewer.path)) return folderEntries;
    if (viewerEntryCache && viewerEntryCache.machine === viewer.machine) return viewerEntryCache.siblings || [];
    return [];
  }, [viewer, folderEntries, viewerEntryCache]);

  const viewerFav = viewer
    ? favourites.find((f) => f.machine === viewer.machine && f.path === viewer.path)
    : null;

  const currentFolderFav =
    activeFolderMachine && resolvedFolderPath
      ? favourites.find((f) => f.machine === activeFolderMachine && f.path === resolvedFolderPath)
      : null;

  const sheet = filesState.sheet;

  function renderWideSplit() {
    const onlineCount = machines.filter((m) => m.reachable).length;
    const hasPinned = Boolean(localCaps.hasFav && favourites.some((f) => f.pinned));
    const hasSync = Boolean(localCaps.hasSync && syncFolders.length > 0);
    const firstSection = hasPinned ? 'pinned' : hasSync ? 'sync' : 'machines';
    const leftLabel = hasPinned ? 'Pinned' : hasSync ? 'Kept in sync' : 'Machines';
    const leftLabelFact = firstSection === 'machines' && machines.length > 0 ? `${onlineCount} on` : null;

    const leftTools = html`
      <input
        class="input plain"
        type="search"
        placeholder="Search all machines"
        aria-label="Search all machines"
        onKeyDown=${(e) => {
          if (e.key === 'Enter') {
            e.preventDefault();
            navigateHref(
              routeHref({
                section: 'files',
                view: 'search',
                query: { q: e.target.value, kinds: [], size: '', changed: '' },
              }),
            );
          }
        }}
      />
    `;

    const leftBody = html`
      <${FilesHome}
        machines=${machines}
        favourites=${favourites}
        syncFolders=${syncFolders}
        recentFiles=${recentFiles}
        caps=${localCaps}
        capsStore=${capsStore}
        deviceNoun=${deviceNoun}
        link=${link}
        thumbCache=${thumbCache}
        wide=${true}
        hideSearch=${true}
        hideFirstLabel=${true}
        activeView=${route.view}
        activeMachine=${activeFolderMachine}
        activeSyncId=${route.view === 'sync' ? route.syncId : null}
        onNavigate=${navigateHref}
        onOpenFile=${openFile}
        onOpenItemSheet=${openItemSheet}
        onStartSearch=${(q) =>
          navigateHref(
            routeHref({ section: 'files', view: 'search', query: { q: q || '', kinds: [], size: '', changed: '' } }),
          )}
      />
    `;

    let rightTitle = null;
    let rightFact = null;
    let rightTools = null;
    let rightLabel = null;
    let rightEmpty = null;
    let rightBody = null;

    const showingFolder = (route.view === 'folder' || route.view === 'home') && Boolean(activeFolderMachine);
    if (route.view === 'home' && !activeFolderMachine) {
      rightEmpty = 'Choose a place on the left.';
    } else if (showingFolder) {
      const activeHomePath = folderChanges?.getHome?.(activeFolderMachine) || null;
      const folderName = folderDisplayName(resolvedFolderPath, { technical, homePath: activeHomePath });
      const isReachable = activeMachineObj ? activeMachineObj.reachable : true;
      const crumbs = splitFileCrumbs(activeFolderMachine, resolvedFolderPath, {
        technical,
        homePath: activeHomePath,
      });
      rightTitle = folderName;
      rightFact =
        isReachable && folderSummary.totalCount != null && folderSummary.totalCount > 0
          ? formatFolderTotal(folderSummary.totalCount, folderSummary.totalExact)
          : null;
      if (!filesState.selecting && isReachable) {
        rightTools = html`
          <div class="files-wide-tools">
            <input
              class="input plain"
              type="search"
              placeholder=${`Search in ${folderName}`}
              aria-label=${`Search in ${folderName}`}
              onKeyDown=${(e) => {
                if (e.key === 'Enter') {
                  e.preventDefault();
                  navigateHref(
                    routeHref({
                      section: 'files',
                      view: 'search',
                      machine: activeFolderMachine,
                      path: resolvedFolderPath,
                      query: { q: e.target.value, kinds: [], size: '', changed: '' },
                    }),
                  );
                }
              }}
            />
            <button
              type="button"
              class="btn icon"
              aria-label=${filesState.kindFilter
                ? `Sort: ${sortDescription(filesState.sortPref)}, showing ${KIND_FILTER_LABELS[filesState.kindFilter] || filesState.kindFilter}`
                : `Sort: ${sortDescription(filesState.sortPref)}`}
              aria-pressed=${filesState.kindFilter ? 'true' : 'false'}
              onClick=${() => filesActions.openSheet({ kind: 'sort' })}
            >
              <${Icon} name="sort" />
            </button>
            <button
              type="button"
              class="btn icon"
              aria-label=${filesState.resolvedViewMode === 'grid' ? 'Show as list' : 'Show as grid'}
              aria-pressed=${filesState.resolvedViewMode === 'grid' ? 'true' : 'false'}
              onClick=${() => {
                const next = filesState.resolvedViewMode === 'grid' ? 'list' : 'grid';
                writeFolderViewMode(safeStorage(), activeFolderMachine, resolvedFolderPath, next);
                filesActions.setViewModeOverride(next);
              }}
            >
              <${Icon} name="grid" />
            </button>
            <button
              type="button"
              class="btn icon"
              aria-label="More"
              onClick=${() =>
                filesActions.openSheet({
                  kind: 'more',
                  machine: activeFolderMachine,
                  path: resolvedFolderPath,
                })}
            >
              <${Icon} name="more" />
            </button>
          </div>
        `;
      }
      rightLabel = html`
        <nav ref=${wideCrumbsRef} class="where" aria-label="Where this folder is">
          ${crumbs.flatMap((c, i) => {
            const isLast = i === crumbs.length - 1;
            const out = [];
            if (i > 0) out.push(html`<i key=${`sep-${i}`} aria-hidden="true">›</i>`);
            out.push(html`
              <a
                key=${`${c.label}-${i}`}
                class=${isLast ? 'current' : undefined}
                aria-current=${isLast ? 'page' : undefined}
                href=${routeHref({ section: 'files', view: 'folder', machine: activeFolderMachine, path: c.path })}
                onClick=${(e) => {
                  e.preventDefault();
                  navigateFolder(activeFolderMachine, c.path);
                }}
              >
                ${c.label}
              </a>
            `);
            return out;
          })}
        </nav>
      `;
      rightBody = folderViewEl;
    } else if (route.view === 'search') {
      const placeholder = route.machine
        ? `Search in ${folderDisplayName(route.path, { technical })}`
        : 'Search all machines';
      rightTitle = 'Search';
      rightFact = wideSearchSummary.searching
        ? 'Searching…'
        : wideSearchSummary.count > 0
          ? `${wideSearchSummary.count} found`
          : null;
      rightTools = html`
        <div class="files-wide-tools">
          <input
            class="input plain"
            type="search"
            value=${wideSearchText}
            placeholder=${placeholder}
            aria-label=${placeholder}
            onInput=${(e) => {
              const val = e.target.value;
              setWideSearchText(val);
              updateWideSearchRoute({ q: val });
            }}
          />
        </div>
      `;
      rightLabel = route.machine
        ? html`
            <nav class="where" aria-label="Search location">
              <a
                href=${routeHref({ section: 'files', view: 'folder', machine: route.machine, path: route.path || '~' })}
                onClick=${(e) => {
                  e.preventDefault();
                  navigateFolder(route.machine, route.path || '~');
                }}
              >
                ${route.machine}
              </a>
              <i aria-hidden="true">›</i>
              <span class="current">${folderDisplayName(route.path, { technical })}</span>
            </nav>
          `
        : 'All machines';
      rightBody = renderMainContent();
    } else if (route.view === 'favourites') {
      rightTitle = 'Favourites';
      rightFact =
        favourites.length > 0
          ? `${favourites.length} ${favourites.length === 1 ? 'item' : 'items'}`
          : null;
      rightLabel = 'Starred';
      rightBody = renderMainContent();
    } else if (route.view === 'recent') {
      rightTitle = 'Recent';
      rightFact =
        recentFiles.length > 0
          ? `${recentFiles.length} ${recentFiles.length === 1 ? 'item' : 'items'}`
          : null;
      rightLabel = `Opened on this ${deviceNoun}`;
      rightBody = renderMainContent();
    } else if (route.view === 'sync' && route.syncId) {
      const sf = (syncFolders || []).find((f) => f.id === route.syncId);
      const memberCount = sf?.members?.length || 0;
      rightTitle = sf?.name || 'Synced folder';
      rightFact = memberCount > 0 ? `${memberCount} machines` : null;
      rightLabel = 'Kept in sync';
      rightBody = renderMainContent();
    } else {
      rightEmpty = 'Choose a place on the left.';
    }

    return html`
      <${SplitView}
        leftTitle="Files"
        leftTools=${leftTools}
        leftLabel=${leftLabel}
        leftLabelFact=${leftLabelFact}
        leftBody=${leftBody}
        onLeftRefresh=${refreshFilesHome}
        rightTitle=${rightTitle}
        rightFact=${rightFact}
        rightTools=${rightTools}
        rightLabel=${rightLabel}
        rightEmpty=${rightEmpty}
        rightBody=${rightBody}
      />
    `;
  }

  return html`
    <div>
      ${wide ? renderWideSplit() : renderMainContent()}

      ${viewer
        ? html`
            <${FilesViewer}
              machine=${viewer.machine}
              path=${viewer.path}
              entry=${viewerEntry}
              siblings=${viewerSiblings}
              link=${link}
              caps=${viewerCaps}
              capsStore=${capsStore}
              thumbCache=${thumbCache}
              onRegisterPopState=${onRegisterPopState}
              deviceNoun=${deviceNoun}
              technical=${technical}
              isStarred=${Boolean(viewerFav)}
              isPinned=${Boolean(viewerFav?.pinned)}
              onClose=${filesActions.closeViewer}
              onNavigateSibling=${(nextEntry) => {
                // Replace viewer URL so swiping through 20 photos doesn't require 20 backs.
                const href = routeHref({ ...route, viewer: { machine: viewer.machine, path: nextEntry.path } });
                window.history.replaceState(window.history.state, '', href);
                window.dispatchEvent(new HashChangeEvent('hashchange'));
              }}
              onToggleStar=${(item) => toggleStar(viewer.machine, item)}
              onTogglePin=${(item) => togglePin(viewer.machine, item)}
              onSendTo=${(items) => filesActions.openSheet({ kind: 'send', sourceMachine: viewer.machine, items })}
              onDownload=${(item) => startDownload(viewer.machine, [item])}
              onOpenExternally=${(item) => startOpenExternally(viewer.machine, item)}
              onRename=${(item) => filesActions.openSheet({ kind: 'rename', machine: viewer.machine, entry: item })}
              onDelete=${(items) => requestDelete(viewer.machine, items)}
              onToast=${onToast}
            />
          `
        : null}

      ${sheet?.kind === 'send'
        ? html`
            <${SendToSheet}
              sourceMachine=${sheet.sourceMachine}
              items=${sheet.items}
              machines=${machines}
              favourites=${favourites}
              caps=${machineCapsFor(sheet.sourceMachine)}
              capsStore=${capsStore}
              folderChanges=${folderChanges}
              allowedSet=${allowedSet}
              wakePendingSet=${wakePendingSet}
              deviceNoun=${deviceNoun}
              technical=${technical}
              link=${link}
              onEnsureAwake=${onEnsureAwake}
              onRefreshFleet=${onRefreshFleet}
              onLocalDone=${() => {
                if (!folderChanges) setReloadToken((n) => n + 1);
              }}
              onStarted=${(info) => {
                filesActions.finishBulkAction();
                if (onToast) {
                  onToast(
                    info.message,
                    info.local
                      ? {}
                      : {
                          actionLabel: 'Activity',
                          actionHref: '#/activity',
                          transferIds: info.transferIds,
                          transferIdsPromise: info.transferIdsPromise,
                          transferJobsPromise: info.transferJobsPromise,
                        },
                  );
                }
              }}
              onClose=${filesActions.closeSheet}
            />
          `
        : null}

      ${sheet?.kind === 'sort'
        ? html`
            <${SortSheet}
              sortPref=${filesState.sortPref}
              kindFilter=${filesState.kindFilter || ''}
              onChange=${filesActions.setSortPref}
              onChangeKind=${filesActions.setKindFilter}
              onClose=${filesActions.closeSheet}
            />
          `
        : null}

      ${sheet?.kind === 'item'
        ? (() => {
            const itemFav = favourites.find(
              (f) => f.machine === sheet.machine && f.path === sheet.entry?.path,
            );
            return html`
              <${ItemSheet}
                machine=${sheet.machine}
                entry=${sheet.entry}
                caps=${machineCapsFor(sheet.machine)}
                deviceNoun=${deviceNoun}
                technical=${technical}
                isStarred=${Boolean(itemFav)}
                isPinned=${Boolean(itemFav?.pinned)}
                onStartSelect=${sheet.canSelect ? (p) => filesActions.startSelect(p) : null}
                onSendTo=${(items) =>
                  filesActions.openSheet({ kind: 'send', sourceMachine: sheet.machine, items })}
                onDownload=${(item) => startDownload(sheet.machine, [item])}
                onToggleStar=${(item) => toggleStar(sheet.machine, item)}
                onTogglePin=${(item) => togglePin(sheet.machine, item)}
                onRename=${(item) =>
                  filesActions.openSheet({ kind: 'rename', machine: sheet.machine, entry: item })}
                onDelete=${(items) => requestDelete(sheet.machine, items)}
                onClose=${filesActions.closeSheet}
              />
            `;
          })()
        : null}

      ${sheet?.kind === 'more'
        ? html`
            <${FolderMoreSheet}
              machine=${sheet.machine || activeFolderMachine}
              path=${sheet.path || resolvedFolderPath}
              caps=${machineCapsFor(sheet.machine || activeFolderMachine)}
              deviceNoun=${deviceNoun}
              showHidden=${filesState.showHidden}
              isStarred=${Boolean(currentFolderFav)}
              isPinned=${Boolean(currentFolderFav?.pinned)}
              onStartSelect=${() => filesActions.startSelect(null)}
              onNewFolder=${() =>
                filesActions.openSheet({
                  kind: 'mkdir',
                  machine: sheet.machine || activeFolderMachine,
                  parentPath: sheet.path || resolvedFolderPath,
                })}
              onUpload=${() => startUpload(sheet.machine || activeFolderMachine, sheet.path || resolvedFolderPath)}
              onToggleStarFolder=${() =>
                toggleStar(sheet.machine || activeFolderMachine, {
                  path: sheet.path || resolvedFolderPath,
                  name: folderDisplayName(sheet.path || resolvedFolderPath),
                  kind: 'dir',
                })}
              onTogglePinFolder=${() =>
                togglePin(sheet.machine || activeFolderMachine, {
                  path: sheet.path || resolvedFolderPath,
                  name: folderDisplayName(sheet.path || resolvedFolderPath),
                  kind: 'dir',
                })}
              onToggleHidden=${filesActions.setShowHidden}
              onClose=${filesActions.closeSheet}
            />
          `
        : null}

      ${sheet?.kind === 'mkdir'
        ? html`
            <${NewFolderSheet}
              machine=${sheet.machine}
              parentPath=${sheet.parentPath}
              link=${link}
              capsStore=${capsStore}
              folderChanges=${folderChanges}
              onCreated=${() => {
                if (!folderChanges) setReloadToken((n) => n + 1);
                if (onToast) onToast('Folder created.');
              }}
              onClose=${filesActions.closeSheet}
            />
          `
        : null}

      ${sheet?.kind === 'rename'
        ? html`
            <${RenameSheet}
              machine=${sheet.machine}
              entry=${sheet.entry}
              link=${link}
              capsStore=${capsStore}
              folderChanges=${folderChanges}
              onRenamed=${(nextPath) => {
                if (!folderChanges) setReloadToken((n) => n + 1);
                loadFavourites();
                if (route.viewer && route.viewer.path === sheet.entry.path) {
                  const href = routeHref({ ...route, viewer: { machine: sheet.machine, path: nextPath } });
                  window.history.replaceState(window.history.state, '', href);
                  window.dispatchEvent(new HashChangeEvent('hashchange'));
                }
                if (onToast) onToast('Renamed.');
              }}
              onClose=${filesActions.closeSheet}
            />
          `
        : null}

      ${sheet?.kind === 'filters'
        ? html`
            <${SearchFiltersSheet}
              size=${route.query?.size || ''}
              changed=${route.query?.changed || ''}
              onApply=${({ size, changed }) => {
                const href = routeHref({
                  ...route,
                  query: { ...(route.query || {}), size, changed },
                });
                window.history.replaceState(window.history.state, '', href);
                window.dispatchEvent(new HashChangeEvent('hashchange'));
              }}
              onClose=${filesActions.closeSheet}
            />
          `
        : null}

      ${sheet?.kind === 'sync-conflicts'
        ? html`
            <${SyncConflictsSheet}
              syncFolder=${sheet.syncFolder}
              link=${link}
              folderChanges=${folderChanges}
              onChanged=${sheet.onChanged}
              onClose=${filesActions.closeSheet}
            />
          `
        : null}

      ${sheet?.kind === 'sync-bin'
        ? html`
            <${SyncBinSheet}
              syncFolder=${sheet.syncFolder}
              link=${link}
              folderChanges=${folderChanges}
              onChanged=${sheet.onChanged}
              onClose=${filesActions.closeSheet}
            />
          `
        : null}

      ${sheet?.kind === 'sync-add'
        ? html`
            <${SyncAddMachineSheet}
              syncFolder=${sheet.syncFolder}
              machines=${machines}
              link=${link}
              onChanged=${sheet.onChanged}
              onClose=${filesActions.closeSheet}
            />
          `
        : null}
    </div>
  `;
}

// ── Machine's own Files part (opens the exact same Files browser at that machine) ────────────

export function MachineFilesBrowser({
  machine,
  nodeInfo,
  tools,
  allowedSet,
  link,
  capsStore = null,
  folderChanges = null,
  onRegisterRefresh = null,
  onRefreshFleet = null,
  settings,
  deviceNoun,
  thumbCache,
  wide = false,
  filesState = null,
  filesActions = null,
  onEnsureAwake,
}) {
  const { caps, entry: capsEntry, missing: missingFeatures } = useMachineCaps(capsStore, machine?.name, {
    fallbackTools: tools,
    fallbackAllowedSet: allowedSet,
    link,
    enabled: Boolean(machine?.name && machine?.reachable !== false),
  });
  const technical = Boolean(settings?.technical);
  const [localSortPref, setLocalSortPref] = useState({ sort: 'name', order: 'asc' });
  const [localKindFilter, setLocalKindFilter] = useState('');
  const [localShowHidden, setLocalShowHidden] = useState(false);
  const [localViewMode, setLocalViewMode] = useState(null);
  const [localResolvedMode, setLocalResolvedMode] = useState('list');
  const [resolvedPath, setResolvedPath] = useState('~');
  const [folderEntries, setFolderEntries] = useState([]);
  const [localSheet, setLocalSheet] = useState(null);
  const [reloadToken, setReloadToken] = useState(0);

  const sortPref = filesState?.sortPref || localSortPref;
  const setSortPref = filesActions?.setSortPref || setLocalSortPref;
  const kindFilter = filesState ? filesState.kindFilter || '' : localKindFilter;
  const setKindFilter = filesActions?.setKindFilter || setLocalKindFilter;
  const showHidden = filesState ? Boolean(filesState.showHidden) : localShowHidden;
  const setShowHidden = filesActions?.setShowHidden || setLocalShowHidden;
  const viewModeOverride = filesState ? filesState.viewModeOverride : localViewMode;
  const setViewModeOverride = filesActions?.setViewModeOverride || setLocalViewMode;
  const resolvedMode = filesState ? filesState.resolvedViewMode : localResolvedMode;
  const setResolvedMode = filesActions?.setResolvedViewMode || setLocalResolvedMode;
  const activeSheet = filesState ? filesState.sheet : localSheet;
  const openSheet = filesActions?.openSheet || setLocalSheet;
  const closeSheet = filesActions?.closeSheet || (() => setLocalSheet(null));

  useEffect(() => {
    if (filesActions?.setMeta) {
      filesActions.setMeta({
        caps,
        resolvedPath,
        entries: folderEntries,
      });
    }
  }, [filesActions, caps, resolvedPath, folderEntries]);

  return html`
    <div>
      <${FolderBrowser}
        machine=${machine.name}
        machineObj=${machine}
        path="~"
        link=${link}
        caps=${caps}
        capsStore=${capsStore}
        capsEntry=${capsEntry}
        missingFeatures=${missingFeatures}
        folderChanges=${folderChanges}
        onRegisterRefresh=${wide ? null : onRegisterRefresh}
        onRefreshFleet=${onRefreshFleet}
        allowedSet=${allowedSet}
        technical=${technical}
        deviceNoun=${deviceNoun}
        favourites=${[]}
        thumbCache=${thumbCache}
        sortPref=${sortPref}
        kindFilter=${kindFilter}
        onClearKindFilter=${() => setKindFilter('')}
        showHidden=${showHidden}
        selecting=${false}
        selectedPaths=${new Set()}
        viewModeOverride=${viewModeOverride}
        onViewModeResolved=${(m, p) => {
          setResolvedMode(m);
          setResolvedPath(p);
        }}
        onEntriesChange=${(list, p) => {
          setFolderEntries(list);
          setResolvedPath(p);
        }}
        onNavigateFolder=${(m, p) => {
          window.location.hash = routeHref({ section: 'files', view: 'folder', machine: m, path: p });
        }}
        onOpenFile=${(m, entry) => {
          const dir = parentFilePath(entry.path) || '~';
          window.location.hash = routeHref({
            section: 'files',
            view: 'folder',
            machine: m,
            path: dir,
            viewer: { machine: m, path: entry.path },
          });
        }}
        onOpenItemSheet=${(m, entry) => openSheet({ kind: 'item', machine: m, entry })}
        onEnsureAwake=${onEnsureAwake}
        onOpenSearch=${(q, m, p) => {
          window.location.hash = routeHref({
            section: 'files',
            view: 'search',
            machine: m,
            path: p,
            query: { q: q || '', kinds: [], size: '', changed: '' },
          });
        }}
        onOpenSort=${() => openSheet({ kind: 'sort' })}
        onToggleView=${() => {
          const next = resolvedMode === 'grid' ? 'list' : 'grid';
          writeFolderViewMode(safeStorage(), machine.name, resolvedPath, next);
          setViewModeOverride(next);
        }}
        onOpenMore=${() => openSheet({ kind: 'more', machine: machine.name, path: resolvedPath })}
        embedded=${true}
        wide=${wide}
        reloadToken=${reloadToken}
      />
      ${activeSheet?.kind === 'sort'
        ? html`
            <${SortSheet}
              sortPref=${sortPref}
              kindFilter=${kindFilter}
              onChange=${setSortPref}
              onChangeKind=${setKindFilter}
              onClose=${closeSheet}
            />
          `
        : null}
      ${activeSheet?.kind === 'item'
        ? html`
            <${ItemSheet}
              machine=${activeSheet.machine || machine.name}
              entry=${activeSheet.entry}
              caps=${caps}
              deviceNoun=${deviceNoun}
              technical=${technical}
              onClose=${closeSheet}
            />
          `
        : null}
      ${activeSheet?.kind === 'more'
        ? html`
            <${FolderMoreSheet}
              machine=${activeSheet.machine || machine.name}
              path=${activeSheet.path || resolvedPath}
              caps=${caps}
              deviceNoun=${deviceNoun}
              showHidden=${showHidden}
              isStarred=${false}
              isPinned=${false}
              onNewFolder=${() => openSheet({ kind: 'mkdir', machine: machine.name, parentPath: resolvedPath })}
              onToggleHidden=${setShowHidden}
              onClose=${closeSheet}
            />
          `
        : null}
      ${activeSheet?.kind === 'mkdir'
        ? html`
            <${NewFolderSheet}
              machine=${activeSheet.machine || machine.name}
              parentPath=${activeSheet.parentPath || resolvedPath}
              link=${link}
              capsStore=${capsStore}
              folderChanges=${folderChanges}
              onCreated=${() => {
                if (!folderChanges) setReloadToken((n) => n + 1);
              }}
              onClose=${closeSheet}
            />
          `
        : null}
    </div>
  `;
}

// ── Files Dock (bottom thumb-reach search, folder tools, selection bar, search chips) ────────

const SEARCH_KIND_CHIPS = [
  { id: 'image', label: 'Pictures' },
  { id: 'video', label: 'Videos' },
  { id: 'audio', label: 'Music' },
  { id: 'document', label: 'Documents' },
  { id: 'folder', label: 'Folders' },
];

export function FilesDock({ route, filesState, filesActions, onNavigateHref }) {
  const { caps, selecting, selectedPaths, entries, resolvedPath, sortPref, kindFilter = '', resolvedViewMode } = filesState;
  const [searchInput, setSearchInput] = useState(route.query?.q || '');

  useEffect(() => {
    setSearchInput(route.query?.q || '');
  }, [route.query?.q, route.view]);

  if (selecting) {
    const sum = selectionSummary(entries, selectedPaths);
    const machine = route.machine;
    const disabled = sum.count === 0;
    return html`
      <div class="dock">
        <div class="bar4">
          ${caps?.hasTransfer || caps?.hasDownload
            ? html`
                <button
                  type="button"
                  disabled=${disabled}
                  onClick=${() => filesActions.bulkSend(machine, sum.items)}
                >
                  <${Icon} name="send" />Send to
                </button>
              `
            : null}
          ${caps?.hasDownload
            ? html`
                <button
                  type="button"
                  disabled=${disabled || sum.fileCount === 0}
                  onClick=${() => filesActions.bulkDownload(machine, sum.items)}
                >
                  <${Icon} name="download" />Download
                </button>
              `
            : null}
          ${caps?.hasFavWrite
            ? html`
                <button
                  type="button"
                  disabled=${disabled}
                  onClick=${() => filesActions.bulkStar(machine, sum.items)}
                >
                  <${Icon} name="star" />Star
                </button>
              `
            : null}
          ${caps?.hasRemove
            ? html`
                <button
                  type="button"
                  class="danger"
                  disabled=${disabled}
                  onClick=${() => filesActions.bulkDelete(machine, sum.items)}
                >
                  <${Icon} name="bin" />Delete
                </button>
              `
            : null}
        </div>
      </div>
    `;
  }

  if (route.view === 'home') {
    return html`
      <div class="dock">
        <input
          class="input plain"
          type="search"
          placeholder="Search all machines"
          aria-label="Search all machines"
          onKeyDown=${(e) => {
            if (e.key === 'Enter') {
              e.preventDefault();
              onNavigateHref(
                routeHref({
                  section: 'files',
                  view: 'search',
                  query: { q: e.target.value, kinds: [], size: '', changed: '' },
                }),
              );
            }
          }}
          onInput=${(e) => {
            if (e.target.value.trim()) {
              onNavigateHref(
                routeHref({
                  section: 'files',
                  view: 'search',
                  query: { q: e.target.value, kinds: [], size: '', changed: '' },
                }),
              );
            }
          }}
        />
      </div>
    `;
  }

  if (route.view === 'folder') {
    const label = folderDisplayName(resolvedPath || route.path);
    return html`
      <div class="dock tools">
        <input
          class="input plain"
          type="search"
          placeholder=${`Search in ${label}`}
          aria-label=${`Search in ${label}`}
          onKeyDown=${(e) => {
            if (e.key === 'Enter') {
              e.preventDefault();
              onNavigateHref(
                routeHref({
                  section: 'files',
                  view: 'search',
                  machine: route.machine,
                  path: resolvedPath || route.path,
                  query: { q: e.target.value, kinds: [], size: '', changed: '' },
                }),
              );
            }
          }}
          onInput=${(e) => {
            if (e.target.value.trim()) {
              onNavigateHref(
                routeHref({
                  section: 'files',
                  view: 'search',
                  machine: route.machine,
                  path: resolvedPath || route.path,
                  query: { q: e.target.value, kinds: [], size: '', changed: '' },
                }),
              );
            }
          }}
        />
        <button
          type="button"
          class="btn icon"
          aria-label=${kindFilter
            ? `Sort: ${sortDescription(sortPref)}, showing ${KIND_FILTER_LABELS[kindFilter] || kindFilter}`
            : `Sort: ${sortDescription(sortPref)}`}
          aria-pressed=${kindFilter ? 'true' : 'false'}
          onClick=${() => filesActions.openSheet({ kind: 'sort' })}
        >
          <${Icon} name="sort" />
        </button>
        <button
          type="button"
          class="btn icon"
          aria-label=${resolvedViewMode === 'grid' ? 'Show as list' : 'Show as grid'}
          aria-pressed=${resolvedViewMode === 'grid' ? 'true' : 'false'}
          onClick=${() => {
            const next = resolvedViewMode === 'grid' ? 'list' : 'grid';
            writeFolderViewMode(safeStorage(), route.machine, resolvedPath || route.path, next);
            filesActions.setViewModeOverride(next);
          }}
        >
          <${Icon} name="grid" />
        </button>
        <button
          type="button"
          class="btn icon"
          aria-label="More"
          onClick=${() =>
            filesActions.openSheet({ kind: 'more', machine: route.machine, path: resolvedPath || route.path })}
        >
          <${Icon} name="more" />
        </button>
      </div>
    `;
  }

  if (route.view === 'search') {
    const activeKinds = new Set(route.query?.kinds || []);
    const hasMoreFilters = Boolean(route.query?.size || route.query?.changed);
    const showMoreFilters = caps ? Boolean(caps.known && caps.hasFindFilters) : true;

    const updateSearchRoute = (patch) => {
      const nextQuery = {
        q: route.query?.q || '',
        kinds: route.query?.kinds || [],
        size: route.query?.size || '',
        changed: route.query?.changed || '',
        ...patch,
      };
      const href = routeHref({ ...route, query: nextQuery });
      window.history.replaceState(window.history.state, '', href);
      window.dispatchEvent(new HashChangeEvent('hashchange'));
    };

    const toggleKind = (id) => {
      const next = new Set(activeKinds);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      updateSearchRoute({ kinds: Array.from(next) });
    };

    const placeholder = route.machine
      ? `Search in ${folderDisplayName(route.path)}`
      : 'Search all machines';

    return html`
      <div class="dock">
        <div class="filters" role="group" aria-label="Kind" data-no-swipe="true">
          ${SEARCH_KIND_CHIPS.map(
            (chip) => html`
              <button
                key=${chip.id}
                type="button"
                aria-pressed=${activeKinds.has(chip.id) ? 'true' : 'false'}
                onClick=${() => toggleKind(chip.id)}
              >
                ${chip.label}
              </button>
            `,
          )}
          ${showMoreFilters
            ? html`
                <button
                  type="button"
                  aria-pressed=${hasMoreFilters ? 'true' : 'false'}
                  onClick=${() => filesActions.openSheet({ kind: 'filters' })}
                >
                  More filters
                </button>
              `
            : null}
        </div>
        <input
          class="input plain"
          type="search"
          value=${searchInput}
          placeholder=${placeholder}
          aria-label=${placeholder}
          onInput=${(e) => {
            const val = e.target.value;
            setSearchInput(val);
            updateSearchRoute({ q: val });
          }}
        />
      </div>
    `;
  }

  return null;
}
