// Files tab views: Home (Pinned, Kept in sync, Machines, More), Folder (list & grid with batched
// thumbnails, 5 000-entry paging, swipe actions, long-press selection), Search, Favourites, Recent,
// and Kept-in-sync folder detail.

import { html, useCallback, useEffect, useMemo, useRef, useState } from '../vendor/preact-htm.js';
import { Icon } from '../icons.js';
import {
  baseFileName,
  changedPresetCutoff,
  detectFileCapabilities,
  entryCategory,
  entryIconName,
  filterEntries,
  folderDisplayName,
  formatPathForDisplay,
  isThumbEligible,
  parentFilePath,
  plainCategoryLabel,
  selectAllEntries,
  selectionSummary,
  shouldDefaultToGrid,
  sizePresetBounds,
  sortDescription,
  sortEntries,
  splitFileCrumbs,
  toggleSelection,
} from '../lib/files.js';
import {
  readFolderViewMode,
  readRecentFiles,
  recordRecentFile,
  writeFolderViewMode,
} from '../lib/files-store.js';
import { formatBytes, formatElapsed, formatRelativeTime, primaryDisk } from '../lib/format.js';
import { routeHref } from '../lib/route.js';
import { thumbCacheKey } from '../lib/thumb-cache.js';
import { Notice, ProgressBar, SwipeRow } from './common.js';
import {
  FolderMoreSheet,
  NewFolderSheet,
  RenameSheet,
  SearchFiltersSheet,
  SendToSheet,
  SortSheet,
  SyncAddMachineSheet,
  SyncBinSheet,
  SyncConflictsSheet,
} from './files-sheets.js';
import { FilesViewer } from './files-viewer.js';

const PAGE_SIZE = 200;
const RENDER_STEP = 120;
const THUMB_BATCH_MAX = 32;
const LONG_PRESS_MS = 450;

function safeStorage() {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

function freeSpaceSub(machine) {
  const disk = primaryDisk(machine?.sys?.disks);
  if (disk?.disk?.available != null) {
    return `${formatBytes(disk.disk.available)} free`;
  }
  return 'On';
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

// ── Batched lazy thumbnail loader hook ───────────────────────────────────────────────────────

function useBatchedThumbs({ machine, entries, link, enabled, thumbCache }) {
  const [, setTick] = useState(0);
  const queueRef = useRef(new Set());
  const inFlightRef = useRef(new Set());
  const timerRef = useRef(null);
  const observerRef = useRef(null);
  const entryMapRef = useRef(new Map());

  useEffect(() => {
    const map = new Map();
    for (const e of entries || []) {
      if (e?.path) map.set(e.path, e);
    }
    entryMapRef.current = map;
  }, [entries]);

  const flush = useCallback(() => {
    timerRef.current = null;
    if (!enabled || !machine || queueRef.current.size === 0) return;
    const batch = [];
    for (const p of queueRef.current) {
      if (batch.length >= THUMB_BATCH_MAX) break;
      batch.push(p);
    }
    for (const p of batch) {
      queueRef.current.delete(p);
      inFlightRef.current.add(p);
    }
    if (batch.length === 0) return;

    link
      .call('fs.thumb', { paths: batch, size: 128, fit: 'cover' }, { on: machine })
      .then((res) => {
        const map = res?.thumbnails || {};
        for (const p of batch) {
          inFlightRef.current.delete(p);
          const entry = entryMapRef.current.get(p);
          const key = thumbCacheKey(machine, p, entry?.mtime || '', 128);
          const item = map[p];
          if (item?.content && item?.mime) {
            thumbCache.set(key, { url: `data:${item.mime};base64,${item.content}` });
          } else {
            thumbCache.set(key, { failed: true });
          }
        }
        setTick((n) => n + 1);
        if (queueRef.current.size > 0 && !timerRef.current) {
          timerRef.current = window.setTimeout(flush, 30);
        }
      })
      .catch(() => {
        for (const p of batch) {
          inFlightRef.current.delete(p);
          const entry = entryMapRef.current.get(p);
          const key = thumbCacheKey(machine, p, entry?.mtime || '', 128);
          thumbCache.set(key, { failed: true });
        }
      });
  }, [enabled, machine, link, thumbCache]);

  const requestPath = useCallback(
    (entry) => {
      if (!enabled || !machine || !entry || !isThumbEligible(entry)) return;
      const key = thumbCacheKey(machine, entry.path, entry.mtime || '', 128);
      if (thumbCache.get(key) || inFlightRef.current.has(entry.path)) return;
      queueRef.current.add(entry.path);
      if (!timerRef.current) {
        timerRef.current = window.setTimeout(flush, 24);
      }
    },
    [enabled, machine, thumbCache, flush],
  );

  // Immediately enqueue the first visible batch so initial paint fills thumbnails promptly.
  useEffect(() => {
    if (!enabled || !machine || !Array.isArray(entries)) return;
    let count = 0;
    for (const e of entries) {
      if (count >= 24) break;
      if (isThumbEligible(e)) {
        requestPath(e);
        count += 1;
      }
    }
    return () => {
      if (timerRef.current) {
        clearTimeout(timerRef.current);
        timerRef.current = null;
      }
    };
  }, [enabled, machine, entries, requestPath]);

  useEffect(() => {
    if (!enabled || typeof IntersectionObserver === 'undefined') return undefined;
    const obs = new IntersectionObserver(
      (items) => {
        for (const item of items) {
          if (!item.isIntersecting) continue;
          const p = item.target?.dataset?.thumbPath;
          if (!p) continue;
          const entry = entryMapRef.current.get(p);
          if (entry) requestPath(entry);
          obs.unobserve(item.target);
        }
      },
      { rootMargin: '200px 0px' },
    );
    observerRef.current = obs;
    return () => {
      obs.disconnect();
      observerRef.current = null;
    };
  }, [enabled, requestPath]);

  const observeEl = useCallback((el) => {
    if (el && observerRef.current) {
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

// ── Thumbnail box (fixed size, zero layout shift) ────────────────────────────────────────────

function EntryThumb({ entry, thumbUrl, observeEl }) {
  if (entry.kind === 'dir') {
    return html`
      <span class="thumb folder" aria-hidden="true">
        <${Icon} name="files" />
      </span>
    `;
  }
  return html`
    <span class="thumb" ref=${observeEl} data-thumb-path=${entry.path} aria-hidden="true">
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
  deviceNoun,
  link,
  thumbCache,
  wide,
  activeMachine,
  activeSyncId,
  onNavigate,
  onOpenFile,
  onStartSearch,
}) {
  const pinned = useMemo(() => (favourites || []).filter((f) => f.pinned), [favourites]);
  const [tileThumbs, setTileThumbs] = useState({});

  // Load thumbnails for pinned files (e.g. Passport.pdf or pinned photos).
  useEffect(() => {
    if (!caps?.hasThumb || pinned.length === 0) return;
    let alive = true;
    const byMachine = new Map();
    for (const p of pinned) {
      if (p.kind === 'dir') continue;
      const key = thumbCacheKey(p.machine, p.path, '', 128);
      const hit = thumbCache.get(key);
      if (hit && !hit.failed) {
        setTileThumbs((prev) => (prev[p.id] ? prev : { ...prev, [p.id]: hit.url }));
        continue;
      }
      if (!byMachine.has(p.machine)) byMachine.set(p.machine, []);
      byMachine.get(p.machine).push(p);
    }
    for (const [mName, items] of byMachine.entries()) {
      link
        .call('fs.thumb', { paths: items.map((i) => i.path), size: 128, fit: 'cover' }, { on: mName })
        .then((res) => {
          if (!alive) return;
          const map = res?.thumbnails || {};
          const patch = {};
          for (const item of items) {
            const t = map[item.path];
            if (t?.content && t?.mime) {
              const url = `data:${t.mime};base64,${t.content}`;
              thumbCache.set(thumbCacheKey(mName, item.path, '', 128), { url });
              patch[item.id] = url;
            }
          }
          if (Object.keys(patch).length > 0) {
            setTileThumbs((prev) => ({ ...prev, ...patch }));
          }
        })
        .catch(() => {});
    }
    return () => {
      alive = false;
    };
  }, [caps?.hasThumb, pinned, link, thumbCache]);

  return html`
    <div>
      ${wide
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

      ${caps?.hasFav && pinned.length > 0
        ? html`
            <p class="label">Pinned</p>
            <div class="tiles">
              ${pinned.map((item) => {
                const href =
                  item.kind === 'dir'
                    ? routeHref({ section: 'files', view: 'folder', machine: item.machine, path: item.path })
                    : routeHref({ section: 'files', view: 'home', viewer: { machine: item.machine, path: item.path } });
                const thumbUrl = tileThumbs[item.id];
                return html`
                  <a
                    key=${item.id}
                    class="tile"
                    href=${href}
                    onClick=${(e) => {
                      e.preventDefault();
                      if (item.kind === 'dir') {
                        onNavigate(href);
                      } else {
                        onOpenFile(item.machine, {
                          name: item.label || baseFileName(item.path),
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
                    <strong>${item.label || baseFileName(item.path)}</strong>
                    <span>${item.machine}</span>
                  </a>
                `;
              })}
            </div>
          `
        : null}

      ${caps?.hasSync && syncFolders.length > 0
        ? html`
            <p class="label">Kept in sync</p>
            <div class="list">
              ${syncFolders.map((sf) => {
                const href = routeHref({ section: 'files', view: 'sync', syncId: sf.id });
                const selected = wide && activeSyncId === sf.id;
                return html`
                  <a
                    key=${sf.id}
                    class=${selected ? 'row selected' : 'row'}
                    href=${href}
                    onClick=${(e) => {
                      e.preventDefault();
                      onNavigate(href);
                    }}
                  >
                    <span class="thumb folder" aria-hidden="true"><${Icon} name="sync" /></span>
                    <span class="body">
                      <span class="title">${sf.name}</span>
                      <span class="sub">${sf.summary || `${joinMemberNames(sf.members)} · Up to date`}</span>
                    </span>
                    <span class="chev" aria-hidden="true">›</span>
                  </a>
                `;
              })}
            </div>
          `
        : null}

      <p class="label">Machines</p>
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
              <span class=${m.busy ? 'dot busy' : 'dot'} aria-hidden="true"></span>
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
                class="row"
                href="#/files/@favourites"
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
          class="row"
          href="#/files/@recent"
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
  allowedSet,
  technical,
  deviceNoun,
  favourites,
  thumbCache,
  sortPref,
  showHidden,
  selecting,
  selectedPaths,
  viewModeOverride,
  onViewModeResolved,
  onEntriesChange,
  onNavigateFolder,
  onOpenFile,
  onToggleSelect,
  onStartSelectWith,
  onToggleStar,
  onSendTo,
  onWakeMachine,
  onOpenSearch,
  onOpenSort,
  onToggleView,
  onOpenMore,
  embedded = false,
  wide = false,
  reloadToken = 0,
}) {
  const [resolvedPath, setResolvedPath] = useState(path || '~');
  const [rawEntries, setRawEntries] = useState([]);
  const [totalCount, setTotalCount] = useState(null);
  const [hasMorePages, setHasMorePages] = useState(false);
  const [loading, setLoading] = useState(true);
  const [loadingMore, setLoadingMore] = useState(false);
  const [renderLimit, setRenderLimit] = useState(RENDER_STEP);
  const [error, setError] = useState(null);

  const pagerRef = useRef(null);
  const crumbsRef = useRef(null);
  const longPressTimerRef = useRef(null);
  const longPressFiredRef = useRef(false);
  const abortRef = useRef(null);
  const offsetRef = useRef(0);

  const favPathSet = useMemo(() => {
    const s = new Set();
    for (const f of favourites || []) {
      if (f.machine === machine) s.add(f.path);
    }
    return s;
  }, [favourites, machine]);

  const reachable = machineObj ? machineObj.reachable : true;
  const canWake = allowedSet ? allowedSet.has('power.wake') : false;

  // Load initial page of directory.
  useEffect(() => {
    if (!machine || !reachable) {
      setLoading(false);
      setRawEntries([]);
      return;
    }
    if (abortRef.current) abortRef.current.abort();
    const ctrl = new AbortController();
    abortRef.current = ctrl;

    setLoading(true);
    setError(null);
    setRenderLimit(RENDER_STEP);
    offsetRef.current = 0;

    const reqArgs = {
      path: path || '~',
      depth: 1,
      hidden: showHidden,
      limit: caps?.hasListOffset ? PAGE_SIZE : 5000,
    };
    if (caps?.hasListSort) {
      reqArgs.sort = sortPref.sort;
      reqArgs.order = sortPref.order;
    }
    if (caps?.hasListOffset) {
      reqArgs.offset = 0;
    }

    link
      .call('fs.list', reqArgs, { on: machine, signal: ctrl.signal })
      .then((res) => {
        if (ctrl.signal.aborted) return;
        const actualPath = res?.path || path || '~';
        const list = Array.isArray(res?.entries) ? res.entries : [];
        setResolvedPath(actualPath);
        setRawEntries(list);
        setTotalCount(typeof res?.total === 'number' ? res.total : list.length);
        setHasMorePages(Boolean(res?.truncated && caps?.hasListOffset));
        offsetRef.current = list.length;
        setLoading(false);
      })
      .catch((err) => {
        if (ctrl.signal.aborted) return;
        setError(err);
        setLoading(false);
      });

    return () => {
      ctrl.abort();
    };
  }, [machine, path, reachable, showHidden, sortPref.sort, sortPref.order, caps?.hasListSort, caps?.hasListOffset, reloadToken]);

  const loadNextPage = useCallback(() => {
    if (loadingMore || !hasMorePages || !caps?.hasListOffset) return;
    setLoadingMore(true);
    const reqArgs = {
      path: resolvedPath,
      depth: 1,
      hidden: showHidden,
      limit: PAGE_SIZE,
      offset: offsetRef.current,
    };
    if (caps?.hasListSort) {
      reqArgs.sort = sortPref.sort;
      reqArgs.order = sortPref.order;
    }
    link
      .call('fs.list', reqArgs, { on: machine, signal: abortRef.current?.signal })
      .then((res) => {
        const more = Array.isArray(res?.entries) ? res.entries : [];
        setRawEntries((prev) => {
          const seen = new Set(prev.map((e) => e.path));
          const merged = [...prev];
          for (const e of more) {
            if (!seen.has(e.path)) merged.push(e);
          }
          return merged;
        });
        offsetRef.current += more.length;
        setHasMorePages(Boolean(res?.truncated && more.length > 0));
        setRenderLimit((n) => n + RENDER_STEP);
        setLoadingMore(false);
      })
      .catch(() => {
        setLoadingMore(false);
      });
  }, [loadingMore, hasMorePages, caps?.hasListOffset, caps?.hasListSort, resolvedPath, showHidden, sortPref.sort, sortPref.order, link, machine]);

  // Client-side filter & sort guarantees consistent behavior on older nodes too.
  const entries = useMemo(() => {
    const filtered = filterEntries(rawEntries, { hidden: showHidden });
    return caps?.hasListSort ? filtered : sortEntries(filtered, sortPref);
  }, [rawEntries, showHidden, caps?.hasListSort, sortPref]);

  useEffect(() => {
    if (onEntriesChange) onEntriesChange(entries, resolvedPath);
  }, [entries, resolvedPath]);

  const savedMode = readFolderViewMode(safeStorage(), machine, resolvedPath);
  const effectiveMode = viewModeOverride || savedMode || (shouldDefaultToGrid(entries) ? 'grid' : 'list');

  useEffect(() => {
    if (onViewModeResolved) onViewModeResolved(effectiveMode, resolvedPath);
  }, [effectiveMode, resolvedPath]);

  const visibleEntries = useMemo(() => entries.slice(0, renderLimit), [entries, renderLimit]);
  const hasMoreToRender = visibleEntries.length < entries.length;

  // Auto-advance incremental slice or fetch next offset page when sentinel scrolls into view.
  useEffect(() => {
    const el = pagerRef.current;
    if (!el || typeof IntersectionObserver === 'undefined') return undefined;
    const obs = new IntersectionObserver(
      (hits) => {
        if (!hits.some((h) => h.isIntersecting)) return;
        if (hasMoreToRender) {
          setRenderLimit((n) => n + RENDER_STEP);
        } else if (hasMorePages) {
          loadNextPage();
        }
      },
      { rootMargin: '300px 0px' },
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
    enabled: Boolean(caps?.hasThumb && reachable),
    thumbCache,
  });

  if (!reachable) {
    return html`
      <div>
        <div class="notice">
          <strong>${machine} is off.</strong>
          <span>${offlineMachineSub(machineObj || { name: machine })}</span>
        </div>
        ${machineObj?.canWake && canWake && onWakeMachine
          ? html`
              <div class="actions">
                <button type="button" class="btn primary" onClick=${() => onWakeMachine(machineObj)}>
                  Wake ${machine}
                </button>
              </div>
            `
          : null}
      </div>
    `;
  }

  const crumbs = splitFileCrumbs(machine, resolvedPath, { technical });

  function startLongPress(entry) {
    if (selecting) return;
    longPressFiredRef.current = false;
    if (longPressTimerRef.current) clearTimeout(longPressTimerRef.current);
    longPressTimerRef.current = window.setTimeout(() => {
      longPressFiredRef.current = true;
      longPressTimerRef.current = null;
      if (onStartSelectWith) onStartSelectWith(entry.path);
    }, LONG_PRESS_MS);
  }

  function cancelLongPress() {
    if (longPressTimerRef.current) {
      clearTimeout(longPressTimerRef.current);
      longPressTimerRef.current = null;
    }
  }

  function handleEntryTap(e, entry) {
    e.preventDefault();
    if (longPressFiredRef.current) {
      longPressFiredRef.current = false;
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

  function renderListRow(entry) {
    const picked = selecting && selectedPaths.has(entry.path);
    const starred = caps?.hasFav && favPathSet.has(entry.path);
    const thumbUrl = getThumbUrl(entry);
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

    const rowCls = ['row', picked ? 'picked' : ''].filter(Boolean).join(' ');
    const rowEl = html`
      <a
        class=${rowCls}
        href=${href}
        aria-selected=${selecting ? (picked ? 'true' : 'false') : undefined}
        onPointerDown=${() => startLongPress(entry)}
        onPointerMove=${cancelLongPress}
        onPointerUp=${cancelLongPress}
        onPointerCancel=${cancelLongPress}
        onContextMenu=${(e) => {
          if (!selecting && onStartSelectWith) {
            e.preventDefault();
            onStartSelectWith(entry.path);
          }
        }}
        onClick=${(e) => handleEntryTap(e, entry)}
      >
        <${EntryThumb} entry=${entry} thumbUrl=${thumbUrl} observeEl=${observeThumb} />
        <span class="body">
          <span class=${technical ? 'title mono' : 'title'}>${entry.name}</span>
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
              <div class="grid">
                ${gridItems.map((entry) => {
                  const picked = selecting && selectedPaths.has(entry.path);
                  const starred = caps?.hasFav && favPathSet.has(entry.path);
                  const thumbUrl = getThumbUrl(entry);
                  const isDir = entry.kind === 'dir';
                  const href = isDir
                    ? routeHref({ section: 'files', view: 'folder', machine, path: entry.path })
                    : routeHref({
                        section: 'files',
                        view: 'folder',
                        machine,
                        path: resolvedPath,
                        viewer: { machine, path: entry.path },
                      });
                  const cls = ['cell', isDir ? 'folder' : '', picked ? 'picked' : '']
                    .filter(Boolean)
                    .join(' ');
                  return html`
                    <a
                      key=${entry.path}
                      class=${cls}
                      href=${href}
                      aria-label=${entry.name}
                      ref=${!isDir ? observeThumb : null}
                      data-thumb-path=${!isDir ? entry.path : undefined}
                      onPointerDown=${() => startLongPress(entry)}
                      onPointerMove=${cancelLongPress}
                      onPointerUp=${cancelLongPress}
                      onPointerCancel=${cancelLongPress}
                      onClick=${(e) => handleEntryTap(e, entry)}
                    >
                      ${isDir
                        ? html`
                            <${Icon} name="files" />
                            <span class="cell-name">${entry.name}</span>
                          `
                        : thumbUrl
                          ? html`<img src=${thumbUrl} alt=${entry.name} loading="lazy" decoding="async" />`
                          : html`
                              <span class="cell-icon">
                                <${Icon} name=${entryIconName(entry)} />
                                <span class="cell-name">${entry.name}</span>
                              </span>
                            `}
                      ${entry.duration_s != null
                        ? html`<span class="len">${formatElapsed(entry.duration_s)}</span>`
                        : null}
                      ${starred && !selecting ? html`<span class="cell-star" aria-label="Favourite">★</span>` : null}
                      ${selecting ? html`<span class="cell-check" aria-hidden="true"></span>` : null}
                    </a>
                  `;
                })}
              </div>
            `
          : null}

        ${otherItems.length > 0
          ? html`
              <p class="label">Other files</p>
              <div class="list">${otherItems.map(renderListRow)}</div>
            `
          : null}
      </div>
    `;
  }

  const inlineTools = (wide || embedded) && !selecting;

  return html`
    <div data-no-swipe="true">
      ${wide && !embedded
        ? html`
            <div class="files-wide-head">
              <h2>${folderDisplayName(resolvedPath, { technical })}</h2>
              ${totalCount != null && totalCount > 0
                ? html`<span class="dim">${totalCount.toLocaleString()} items</span>`
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
                placeholder=${`Search in ${folderDisplayName(resolvedPath, { technical })}`}
                aria-label=${`Search in ${folderDisplayName(resolvedPath, { technical })}`}
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
                aria-label=${`Sort: ${sortDescription(sortPref)}`}
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

      <nav ref=${crumbsRef} class="where" aria-label="Where this folder is">
        ${crumbs.map(
          (c, i) => html`
            <span key=${`${c.label}-${i}`}>
              ${i > 0 ? html`<i aria-hidden="true">›</i>` : null}
              <a
                href=${routeHref({ section: 'files', view: 'folder', machine, path: c.path })}
                onClick=${(e) => {
                  e.preventDefault();
                  onNavigateFolder(machine, c.path);
                }}
              >
                ${c.label}
              </a>
            </span>
          `,
        )}
      </nav>

      ${error ? html`<${Notice} error=${error} />` : null}

      ${loading && rawEntries.length === 0
        ? html`<div class="empty">Loading folder…</div>`
        : entries.length === 0
          ? html`<div class="empty">This folder is empty.</div>`
          : effectiveMode === 'grid'
            ? renderGrid()
            : html`<div class="list">${visibleEntries.map(renderListRow)}</div>`}

      ${hasMoreToRender || hasMorePages
        ? html`
            <div ref=${pagerRef} class="folder-pager">
              <span>
                Showing ${visibleEntries.length.toLocaleString()}
                ${totalCount ? ` of ${totalCount.toLocaleString()}` : ''}
              </span>
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
  link,
  technical,
  thumbCache,
  onNavigateFolder,
  onOpenFile,
  onResultCountChange,
}) {
  const scopeMachine = route.machine || null;
  const scopePath = route.path || null;
  const q = route.query?.q || '';
  const kinds = route.query?.kinds || [];
  const size = route.query?.size || '';
  const changed = route.query?.changed || '';
  const kindsKey = kinds.join(',');

  const [resultsByMachine, setResultsByMachine] = useState({});
  const [truncatedByMachine, setTruncatedByMachine] = useState({});
  const [searching, setSearching] = useState(false);
  const [error, setError] = useState(null);
  const abortRef = useRef(null);

  const favKeySet = useMemo(
    () => new Set((favourites || []).map((f) => `${f.machine}\0${f.path}`)),
    [favourites],
  );

  const onlineMachines = useMemo(() => machines.filter((m) => m.reachable), [machines]);
  const offMachines = useMemo(() => machines.filter((m) => !m.reachable), [machines]);

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
      return () => ctrl.abort();
    }

    setSearching(true);
    setError(null);
    setResultsByMachine({});
    setTruncatedByMachine({});

    const { min_size, max_size } = sizePresetBounds(size);
    const modified_after = changedPresetCutoff(changed);

    const buildFindArgs = (basePath) => {
      if (caps?.hasFindEnhanced) {
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
        if (min_size != null) args.min_size = min_size;
        if (max_size != null) args.max_size = max_size;
        if (modified_after) args.modified_after = modified_after;
        return args;
      }
      // Older node degrade: basic glob/kind + client-side filtering.
      const args = { path: basePath, limit: 300 };
      if (q.trim()) args.glob = `*${q.trim()}*`;
      if (kinds.length === 1 && kinds[0] === 'folder') args.kind = 'dir';
      return args;
    };

    const filterClient = (list) =>
      filterEntries(list || [], {
        hidden: false,
        query: q.trim(),
        categories: kinds,
        minSize: min_size,
        maxSize: max_size,
        modifiedAfter: modified_after,
      });

    if (scopeMachine) {
      const args = buildFindArgs(scopePath || '~');
      link
        .call('fs.find', args, { on: scopeMachine, signal: ctrl.signal })
        .then((res) => {
          if (ctrl.signal.aborted) return;
          const entries = filterClient(res?.entries || []);
          setResultsByMachine({ [scopeMachine]: entries });
          setTruncatedByMachine({ [scopeMachine]: Boolean(res?.truncated) });
          setSearching(false);
          if (onResultCountChange) {
            onResultCountChange({ count: entries.length, machineCount: 1, searching: false });
          }
        })
        .catch((err) => {
          if (ctrl.signal.aborted) return;
          setError(err);
          setSearching(false);
          if (onResultCountChange) onResultCountChange({ count: 0, machineCount: 0, searching: false });
        });
    } else {
      const args = buildFindArgs('~');
      link
        .call('fs.find', args, {
          on: 'all',
          signal: ctrl.signal,
          onEvent(evt) {
            if (ctrl.signal.aborted) return;
            if (evt?.kind === 'find_machine' && evt.machine && evt.data) {
              const entries = filterClient(evt.data.entries || []);
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
              nextMap[mName] = filterClient(outcome.ok.entries || []);
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
        })
        .catch((err) => {
          if (ctrl.signal.aborted) return;
          setError(err);
          setSearching(false);
          if (onResultCountChange) onResultCountChange({ count: 0, machineCount: 0, searching: false });
        });
    }

    return () => ctrl.abort();
  }, [q, kindsKey, size, changed, scopeMachine, scopePath, caps?.hasFindEnhanced]);

  const machineOrder = scopeMachine
    ? [scopeMachine]
    : onlineMachines.map((m) => m.name);

  const hasCriteria = Boolean(q.trim() || kinds.length > 0 || size || changed);

  if (!hasCriteria) {
    return html`
      <div class="empty">
        ${scopeMachine
          ? `Type a name or pick a kind below to search in ${folderDisplayName(scopePath, { technical })}.`
          : 'Type a name or pick a kind below to search across your machines.'}
      </div>
    `;
  }

  return html`
    <div>
      ${error ? html`<${Notice} error=${error} />` : null}
      ${searching && Object.keys(resultsByMachine).length === 0
        ? html`<div class="empty">Searching…</div>`
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
            return html`
              <div key=${mName}>
                <p class="label">${mName}</p>
                ${items.length === 0
                  ? html`<div class="empty">Nothing found.</div>`
                  : html`
                      <div class="list">
                        ${items.map((entry) => {
                          const parent = parentFilePath(entry.path) || '~';
                          const sub = [
                            formatPathForDisplay(parent, { technical }),
                            entry.kind !== 'dir' && entry.size != null ? formatBytes(entry.size) : null,
                          ]
                            .filter(Boolean)
                            .join(' · ');
                          const starred = caps?.hasFav && favKeySet.has(`${mName}\0${entry.path}`);
                          const thumbHit = thumbCache.get(thumbCacheKey(mName, entry.path, entry.mtime || '', 128));
                          const thumbUrl = thumbHit && !thumbHit.failed ? thumbHit.url : null;
                          return html`
                            <a
                              key=${entry.path}
                              class="row"
                              href=${entry.kind === 'dir'
                                ? routeHref({ section: 'files', view: 'folder', machine: mName, path: entry.path })
                                : routeHref({ ...route, viewer: { machine: mName, path: entry.path } })}
                              onClick=${(e) => {
                                e.preventDefault();
                                if (entry.kind === 'dir') onNavigateFolder(mName, entry.path);
                                else onOpenFile(mName, entry, items);
                              }}
                            >
                              <${EntryThumb} entry=${entry} thumbUrl=${thumbUrl} />
                              <span class="body">
                                <span class=${technical ? 'title mono' : 'title'}>${entry.name}</span>
                                <span class="sub">${sub}</span>
                              </span>
                              ${starred ? html`<span class="star" aria-label="Favourite">★</span>` : null}
                            </a>
                          `;
                        })}
                      </div>
                    `}
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

function FavouritesView({ favourites, technical, onNavigateFolder, onOpenFile, onTogglePin, onRemoveFav, caps }) {
  if (!favourites || favourites.length === 0) {
    return html`<div class="empty">Star files or folders to find them quickly from any device.</div>`;
  }
  return html`
    <div class="list">
      ${favourites.map((item) => {
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
            onClick=${(e) => {
              e.preventDefault();
              if (item.kind === 'dir') onNavigateFolder(item.machine, item.path);
              else
                onOpenFile(item.machine, {
                  name: item.label || baseFileName(item.path),
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
              <span class="title">${item.label || baseFileName(item.path)}</span>
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
    </div>
  `;
}

function RecentView({ recentFiles, deviceNoun, technical, onOpenFile }) {
  if (!recentFiles || recentFiles.length === 0) {
    return html`<div class="empty">Files you open on this ${deviceNoun} appear here.</div>`;
  }
  return html`
    <div class="list">
      ${recentFiles.map((item) => {
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
            onClick=${(e) => {
              e.preventDefault();
              onOpenFile(item.machine, item);
            }}
          >
            <span class="thumb" aria-hidden="true">
              <${Icon} name=${entryIconName(item)} />
            </span>
            <span class="body">
              <span class=${technical ? 'title mono' : 'title'}>${item.name || baseFileName(item.path)}</span>
              <span class="sub">${sub}</span>
            </span>
          </a>
        `;
      })}
    </div>
  `;
}

// ── Sync Folder Detail View ──────────────────────────────────────────────────────────────────

function SyncFolderView({
  syncId,
  machines,
  link,
  technical,
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
    link
      .call('sync.status', { id: syncId })
      .then((res) => setDetail(res))
      .catch((err) => setError(err));
  }, [link, syncId]);

  useEffect(() => {
    load();
  }, [load]);

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
  nodeInfo,
  tools,
  allowedSet,
  settings,
  deviceNoun,
  thumbCache,
  filesState,
  filesActions,
  onWakeMachine,
  onRequestConfirm,
  onToast,
}) {
  const machines = snapshot?.machines || [];
  const technical = Boolean(settings?.technical);
  const localNode = nodeInfo?.node || machines.find((m) => m.reachable)?.name || null;

  const caps = useMemo(
    () => detectFileCapabilities({ tools, allowedSet, link }),
    [tools, allowedSet, link],
  );

  const [favourites, setFavourites] = useState([]);
  const [syncFolders, setSyncFolders] = useState([]);
  const [recentFiles, setRecentFiles] = useState(() => readRecentFiles(safeStorage()));
  const [folderEntries, setFolderEntries] = useState([]);
  const [resolvedFolderPath, setResolvedFolderPath] = useState(route.path || '~');
  const [reloadToken, setReloadToken] = useState(0);
  const [viewerEntryCache, setViewerEntryCache] = useState(null);

  const loadFavourites = useCallback(() => {
    if (!caps.hasFav || !localNode) {
      setFavourites([]);
      return;
    }
    link
      .call('fav.list', {}, { on: localNode })
      .then((res) => setFavourites(Array.isArray(res?.entries) ? res.entries : []))
      .catch(() => setFavourites([]));
  }, [caps.hasFav, link, localNode]);

  const loadSyncFolders = useCallback(() => {
    if (!caps.hasSync || !localNode) {
      setSyncFolders([]);
      return;
    }
    link
      .call('sync.list', {}, { on: localNode })
      .then((res) => setSyncFolders(Array.isArray(res?.folders) ? res.folders : []))
      .catch(() => setSyncFolders([]));
  }, [caps.hasSync, link, localNode]);

  useEffect(() => {
    loadFavourites();
    loadSyncFolders();
  }, [loadFavourites, loadSyncFolders]);

  // Share caps & current folder metadata with shell header/dock.
  useEffect(() => {
    filesActions.setMeta({
      caps,
      resolvedPath: resolvedFolderPath,
      entries: folderEntries,
      syncFolders,
    });
  }, [caps, resolvedFolderPath, folderEntries, syncFolders]);

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

  const toggleStar = useCallback(
    async (machine, entry) => {
      if (!caps.hasFavWrite) return;
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
        if (onToast) onToast(err?.message || 'Could not update favourite.');
      }
    },
    [caps.hasFavWrite, favourites, link, localNode, loadFavourites, onToast],
  );

  const togglePin = useCallback(
    async (machine, entry) => {
      if (!caps.hasFavWrite) return;
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
        if (onToast) onToast(err?.message || 'Could not update pin.');
      }
    },
    [caps.hasFavWrite, favourites, link, localNode, loadFavourites, onToast],
  );

  const startDownload = useCallback(
    (machine, items) => {
      if (!caps.hasDownload || typeof link?.download !== 'function') return;
      const list = Array.isArray(items) ? items.filter((i) => i.kind !== 'dir') : [items];
      if (list.length === 0) return;
      for (const item of list) {
        link
          .download({
            machine,
            path: item.path,
            name: item.name || baseFileName(item.path),
            size: item.size,
            mime: item.mime,
          })
          .catch(() => {});
      }
      if (onToast) {
        onToast(
          list.length === 1
            ? `Downloading ${list[0].name || baseFileName(list[0].path)} — watch progress in Activity.`
            : `Downloading ${list.length} items — watch progress in Activity.`,
          { actionLabel: 'Activity', actionHref: '#/activity' },
        );
      }
    },
    [caps.hasDownload, link, onToast],
  );

  const startUpload = useCallback(
    (machine, dir) => {
      if (!caps.hasUpload || typeof link?.upload !== 'function') return;
      link
        .upload({ machine, dir })
        .then((res) => {
          if (!res) return;
          setReloadToken((n) => n + 1);
          if (onToast) {
            onToast('Uploading to ' + machine + ' — watch progress in Activity.', {
              actionLabel: 'Activity',
              actionHref: '#/activity',
            });
          }
        })
        .catch(() => {});
    },
    [caps.hasUpload, link, onToast],
  );

  const startOpenExternally = useCallback(
    (machine, entry) => {
      if (!caps.hasOpenExternally || typeof link?.openExternally !== 'function') return;
      link
        .openExternally({
          machine,
          path: entry.path,
          name: entry.name || baseFileName(entry.path),
          mime: entry.mime,
        })
        .then(() => {
          if (onToast) onToast(`Opening ${entry.name || baseFileName(entry.path)} in another app.`);
        })
        .catch((err) => {
          if (onToast) onToast(err?.message || 'Could not open in another app.');
        });
    },
    [caps.hasOpenExternally, link, onToast],
  );

  const requestDelete = useCallback(
    (machine, items) => {
      if (!caps.hasRemove || !onRequestConfirm || !items?.length) return;
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
          return link.call('fs.remove', { path: single.path, recursive: single.kind === 'dir', dry_run: true }, { on: machine });
        },
        async onConfirm() {
          for (const item of items) {
            await link.call('fs.remove', { path: item.path, recursive: item.kind === 'dir' }, { on: machine });
          }
          filesActions.endSelect();
          if (route.viewer && items.some((i) => i.path === route.viewer.path)) {
            filesActions.closeViewer();
          }
          setReloadToken((n) => n + 1);
          loadFavourites();
          if (onToast) onToast(`Deleted ${label}.`);
        },
      });
    },
    [caps.hasRemove, onRequestConfirm, link, filesActions, route.viewer, loadFavourites, onToast],
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

  // Determine active machine/folder for wide split detail when on `#/files`.
  const defaultWideMachine = useMemo(
    () => machines.find((m) => m.reachable)?.name || machines[0]?.name || null,
    [machines],
  );

  const activeFolderMachine =
    route.view === 'folder' ? route.machine : wide && route.view === 'home' ? defaultWideMachine : null;
  const activeFolderPath = route.view === 'folder' ? route.path || '~' : '~';
  const activeMachineObj = activeFolderMachine
    ? machines.find((m) => m.name === activeFolderMachine) || null
    : null;

  const folderViewEl = activeFolderMachine
    ? html`
        <${FolderBrowser}
          machine=${activeFolderMachine}
          machineObj=${activeMachineObj}
          path=${activeFolderPath}
          link=${link}
          caps=${caps}
          allowedSet=${allowedSet}
          technical=${technical}
          deviceNoun=${deviceNoun}
          favourites=${favourites}
          thumbCache=${thumbCache}
          sortPref=${filesState.sortPref}
          showHidden=${filesState.showHidden}
          selecting=${filesState.selecting}
          selectedPaths=${filesState.selectedPaths}
          viewModeOverride=${filesState.viewModeOverride}
          onViewModeResolved=${(m, rPath) => filesActions.setResolvedViewMode(m, rPath)}
          onEntriesChange=${(list, rPath) => {
            setFolderEntries(list);
            setResolvedFolderPath(rPath);
          }}
          onNavigateFolder=${navigateFolder}
          onOpenFile=${openFile}
          onToggleSelect=${filesActions.toggleSelect}
          onStartSelectWith=${filesActions.startSelect}
          onToggleStar=${toggleStar}
          onSendTo=${(m, items) => filesActions.openSheet({ kind: 'send', sourceMachine: m, items })}
          onWakeMachine=${onWakeMachine}
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
          reloadToken=${reloadToken}
        />
      `
    : null;

  function renderMainContent() {
    if (route.view === 'search') {
      return html`
        <${FilesSearchView}
          route=${route}
          machines=${machines}
          favourites=${favourites}
          caps=${caps}
          link=${link}
          technical=${technical}
          thumbCache=${thumbCache}
          onNavigateFolder=${navigateFolder}
          onOpenFile=${openFile}
          onResultCountChange=${filesActions.setSearchSummary}
        />
      `;
    }
    if (route.view === 'favourites') {
      return html`
        <${FavouritesView}
          favourites=${favourites}
          technical=${technical}
          caps=${caps}
          onNavigateFolder=${navigateFolder}
          onOpenFile=${openFile}
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
        caps=${caps}
        deviceNoun=${deviceNoun}
        link=${link}
        thumbCache=${thumbCache}
        wide=${false}
        onNavigate=${navigateHref}
        onOpenFile=${openFile}
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

  return html`
    <div>
      ${wide
        ? html`
            <div class="split">
              <section>
                <${FilesHome}
                  machines=${machines}
                  favourites=${favourites}
                  syncFolders=${syncFolders}
                  recentFiles=${recentFiles}
                  caps=${caps}
                  deviceNoun=${deviceNoun}
                  link=${link}
                  thumbCache=${thumbCache}
                  wide=${true}
                  activeMachine=${activeFolderMachine}
                  activeSyncId=${route.view === 'sync' ? route.syncId : null}
                  onNavigate=${navigateHref}
                  onOpenFile=${openFile}
                  onStartSearch=${(q) =>
                    navigateHref(
                      routeHref({ section: 'files', view: 'search', query: { q: q || '', kinds: [], size: '', changed: '' } }),
                    )}
                />
              </section>
              <section>
                ${route.view === 'home' ? folderViewEl || html`<div class="empty">Choose a machine to browse its files.</div>` : renderMainContent()}
              </section>
            </div>
          `
        : renderMainContent()}

      ${viewer
        ? html`
            <${FilesViewer}
              machine=${viewer.machine}
              path=${viewer.path}
              entry=${viewerEntry}
              siblings=${viewerSiblings}
              link=${link}
              caps=${caps}
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
              caps=${caps}
              allowedSet=${allowedSet}
              deviceNoun=${deviceNoun}
              technical=${technical}
              link=${link}
              onWakeMachine=${onWakeMachine}
              onStarted=${(info) => {
                filesActions.endSelect();
                if (onToast) {
                  onToast(info.message, { actionLabel: 'Activity', actionHref: '#/activity' });
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
              onChange=${filesActions.setSortPref}
              onClose=${filesActions.closeSheet}
            />
          `
        : null}

      ${sheet?.kind === 'more'
        ? html`
            <${FolderMoreSheet}
              machine=${sheet.machine || activeFolderMachine}
              path=${sheet.path || resolvedFolderPath}
              caps=${caps}
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
              onCreated=${() => {
                setReloadToken((n) => n + 1);
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
              onRenamed=${(nextPath) => {
                setReloadToken((n) => n + 1);
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
  settings,
  deviceNoun,
  thumbCache,
  onWakeMachine,
}) {
  const caps = useMemo(
    () => detectFileCapabilities({ tools, allowedSet, link }),
    [tools, allowedSet, link],
  );
  const technical = Boolean(settings?.technical);
  const [sortPref, setSortPref] = useState({ sort: 'name', order: 'asc' });
  const [showHidden, setShowHidden] = useState(false);
  const [viewMode, setViewMode] = useState(null);
  const [resolvedMode, setResolvedMode] = useState('list');
  const [resolvedPath, setResolvedPath] = useState('~');
  const [sheet, setSheet] = useState(null);

  return html`
    <div>
      <${FolderBrowser}
        machine=${machine.name}
        machineObj=${machine}
        path="~"
        link=${link}
        caps=${caps}
        allowedSet=${allowedSet}
        technical=${technical}
        deviceNoun=${deviceNoun}
        favourites=${[]}
        thumbCache=${thumbCache}
        sortPref=${sortPref}
        showHidden=${showHidden}
        selecting=${false}
        selectedPaths=${new Set()}
        viewModeOverride=${viewMode}
        onViewModeResolved=${(m, p) => {
          setResolvedMode(m);
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
        onWakeMachine=${onWakeMachine}
        onOpenSearch=${(q, m, p) => {
          window.location.hash = routeHref({
            section: 'files',
            view: 'search',
            machine: m,
            path: p,
            query: { q: q || '', kinds: [], size: '', changed: '' },
          });
        }}
        onOpenSort=${() => setSheet('sort')}
        onToggleView=${() => setViewMode(resolvedMode === 'grid' ? 'list' : 'grid')}
        onOpenMore=${() => {
          window.location.hash = routeHref({ section: 'files', view: 'folder', machine: machine.name, path: resolvedPath });
        }}
        embedded=${true}
      />
      ${sheet === 'sort'
        ? html`
            <${SortSheet}
              sortPref=${sortPref}
              onChange=${setSortPref}
              onClose=${() => setSheet(null)}
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
  const { caps, selecting, selectedPaths, entries, resolvedPath, sortPref, resolvedViewMode } = filesState;
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
          aria-label=${`Sort: ${sortDescription(sortPref)}`}
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
          <button
            type="button"
            aria-pressed=${hasMoreFilters ? 'true' : 'false'}
            onClick=${() => filesActions.openSheet({ kind: 'filters' })}
          >
            More filters
          </button>
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

export { selectAllEntries, selectionSummary, toggleSelection };
