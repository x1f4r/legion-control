// Files tab views: Home (Pinned, Kept in sync, Machines, More), Folder (list & grid with batched
// thumbnails, 5 000-entry paging, swipe actions, long-press selection), Search, Favourites, Recent,
// and Kept-in-sync folder detail.

import { html, useCallback, useEffect, useMemo, useRef, useState } from '../vendor/preact-htm.js';
import { Icon } from '../icons.js';
import {
  baseFileName,
  changedPresetCutoff,
  createThumbQueue,
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
import { plainErrorMessage } from '../lib/errors.js';
import { createCoalescedRefetch, STALE_AFTER_MS } from '../lib/folder-changes.js';
import { formatBytes, formatElapsed, formatRelativeTime, primaryDisk } from '../lib/format.js';
import { routeHref } from '../lib/route.js';
import { thumbCacheKey } from '../lib/thumb-cache.js';
import { Notice, OlderMachineNote, ProgressBar, SwipeRow } from './common.js';
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
import { ScrollContainer } from './scroll-container.js';

const PAGE_SIZE = 200;
const RENDER_STEP = 120;
const LONG_PRESS_MS = 450;

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

// ── Batched lazy thumbnail loader hooks ──────────────────────────────────────────────────────

function useBatchedThumbs({ machine, entries, link, capsStore = null, enabled, includeAudio = false, thumbCache }) {
  const [, setTick] = useState(0);
  const queueRef = useRef(null);
  const observerRef = useRef(null);
  const observedElsRef = useRef(new Set());
  const entryMapRef = useRef(new Map());

  useEffect(() => {
    const map = new Map();
    for (const e of entries || []) {
      if (e?.path) map.set(e.path, e);
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
      if (!enabled || !machine || !entry || !isThumbEligible(entry, { includeAudio })) return;
      queueRef.current?.enqueue({ path: entry.path, mtime: entry.mtime || '' });
    },
    [enabled, includeAudio, machine],
  );

  // Immediately enqueue the first visible batch so initial paint fills thumbnails promptly.
  useEffect(() => {
    if (!enabled || !machine || !Array.isArray(entries)) return;
    let count = 0;
    for (const e of entries) {
      if (count >= 24) break;
      if (isThumbEligible(e, { includeAudio })) {
        requestPath(e);
        count += 1;
      }
    }
  }, [enabled, includeAudio, machine, entries, link, capsStore, thumbCache, requestPath]);

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
          observedElsRef.current.delete(item.target);
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
    observedElsRef.current.add(el);
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
  capsStore = null,
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
  const pinnedByMachine = useMemo(() => {
    const map = new Map();
    for (const p of pinned) {
      const includeAudio = capsStore
        ? Boolean(capsStore.fileCapabilities(p.machine).hasAudioCovers)
        : Boolean(caps?.hasAudioCovers);
      if (!isThumbEligible(p, { includeAudio })) continue;
      if (!map.has(p.machine)) map.set(p.machine, []);
      map.get(p.machine).push({ path: p.path, mtime: '' });
    }
    return map;
  }, [pinned, capsStore, caps?.hasAudioCovers]);

  // Batch-load thumbnails for pinned pictures/videos/audio covers (up to 32 per machine call) with retry backoff.
  const getPinnedThumbUrl = useMultiMachineThumbs({
    itemsByMachine: pinnedByMachine,
    enabled: Boolean((capsStore || caps?.hasThumb) && pinnedByMachine.size > 0),
    link,
    capsStore,
    thumbCache,
  });

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
                const thumbUrl = isThumbEligible(item) ? getPinnedThumbUrl(item.machine, item.path, '') : null;
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
  onNavigateFolder,
  onOpenFile,
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
  reloadToken = 0,
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
  const longPressTimerRef = useRef(null);
  const longPressFiredRef = useRef(false);
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
        setRenderLimit(RENDER_STEP);
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
      limit: PAGE_SIZE,
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
  }, [loadingMore, hasMorePages, caps?.hasListOffset, caps?.hasListSort, caps?.hasListCategories, resolvedPath, showHidden, sortPref.sort, sortPref.order, kindFilter, capsStore, link, machine]);

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

  const savedMode = readFolderViewMode(safeStorage(), machine, resolvedPath);
  const effectiveMode = viewModeOverride || savedMode || (shouldDefaultToGrid(rawEntries) ? 'grid' : 'list');

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
    capsStore,
    enabled: Boolean(caps?.known && caps?.hasThumb && reachable),
    includeAudio: Boolean(caps?.hasAudioCovers),
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

    const rowCls = ['row', picked ? 'picked' : '', justAddedPaths.has(entry.path) ? 'just-added' : '']
      .filter(Boolean)
      .join(' ');
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

  const inlineTools = wide && !selecting;
  const atMachineFilesRoot = !path || path === '~' || isMachineFilesRoot(resolvedPath);

  return html`
    <div data-no-swipe="true">
      ${wide && !embedded
        ? html`
            <div class="files-wide-head">
              <h2>${folderDisplayName(resolvedPath, { technical })}</h2>
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
              : html`<div class="list">${visibleEntries.map(renderListRow)}</div>`}

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
  onRegisterRefresh = null,
  onNavigateFolder,
  onOpenFile,
  onResultCountChange,
  onClearFilters,
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

  // Batch-load thumbnails for picture/video/audio search hits (up to 32 per machine call) with retry backoff.
  const searchThumbsByMachine = useMemo(() => {
    const map = new Map();
    for (const [mName, items] of Object.entries(resultsByMachine)) {
      if (!Array.isArray(items) || items.length === 0) continue;
      const includeAudio = capsStore
        ? Boolean(capsStore.fileCapabilities(mName).hasAudioCovers)
        : Boolean(caps?.hasAudioCovers);
      const eligible = [];
      for (const entry of items) {
        if (!isThumbEligible(entry, { includeAudio })) continue;
        eligible.push({ path: entry.path, mtime: entry.mtime || '' });
      }
      if (eligible.length > 0) map.set(mName, eligible);
    }
    return map;
  }, [resultsByMachine, capsStore, caps?.hasAudioCovers]);

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

  if (!hasCriteria) {
    return html`
      <div class="empty">
        ${scopeMachine
          ? `Type a name or pick a kind below to search in ${folderDisplayName(scopePath, { technical })}.`
          : 'Type a name or pick a kind below to search across your machines.'}
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
                  <div class="list">
                    ${items.map((entry) => {
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
  const [reloadToken, setReloadToken] = useState(0);
  const [viewerEntryCache, setViewerEntryCache] = useState(null);

  const loadFavourites = useCallback(() => {
    if (!localCaps.hasFav || !localNode) {
      setFavourites([]);
      return Promise.resolve();
    }
    const req = capsStore
      ? capsStore.call('fav.list', {}, { on: localNode }).then((r) => r.value)
      : link.call('fav.list', {}, { on: localNode });
    return req
      .then((res) => setFavourites(Array.isArray(res?.entries) ? res.entries : []))
      .catch(() => setFavourites([]));
  }, [localCaps.hasFav, capsStore, link, localNode]);

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
            onToast(plainErrorMessage(err, `Could not upload to ${machine}.`, { technical, deviceNoun }), { tone: 'error' });
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
          onNavigateFolder=${navigateFolder}
          onOpenFile=${openFile}
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
          caps=${activeSearchCaps}
          capsStore=${capsStore}
          link=${link}
          technical=${technical}
          thumbCache=${thumbCache}
          onRegisterRefresh=${wide ? null : onRegisterRefresh}
          onNavigateFolder=${navigateFolder}
          onOpenFile=${openFile}
          onResultCountChange=${filesActions.setSearchSummary}
          onClearFilters=${() => {
            const href = routeHref({
              ...route,
              query: { q: route.query?.q || '', kinds: [], size: '', changed: '' },
            });
            window.history.replaceState(window.history.state, '', href);
            window.dispatchEvent(new HashChangeEvent('hashchange'));
          }}
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
              <${ScrollContainer} as="section" onRefresh=${refreshFilesHome}>
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
                  activeMachine=${activeFolderMachine}
                  activeSyncId=${route.view === 'sync' ? route.syncId : null}
                  onNavigate=${navigateHref}
                  onOpenFile=${openFile}
                  onStartSearch=${(q) =>
                    navigateHref(
                      routeHref({ section: 'files', view: 'search', query: { q: q || '', kinds: [], size: '', changed: '' } }),
                    )}
                />
              <//>
              <${ScrollContainer} as="section">
                ${route.view === 'home' ? folderViewEl || html`<div class="empty">Choose a machine to browse its files.</div>` : renderMainContent()}
              <//>
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

export { selectAllEntries, selectionSummary, toggleSelection };
