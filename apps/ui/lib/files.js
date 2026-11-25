// Pure helpers for Files: categories, client-side degrade rules, sorting, selection, paths, and history.

import { routeHref } from './route.js';

export const FILE_CATEGORIES = ['image', 'video', 'audio', 'document', 'text', 'archive', 'other'];

const EXTENSION_CATEGORY = {
  // image
  jpg: 'image',
  jpeg: 'image',
  png: 'image',
  gif: 'image',
  webp: 'image',
  avif: 'image',
  heic: 'image',
  heif: 'image',
  bmp: 'image',
  svg: 'image',
  tif: 'image',
  tiff: 'image',
  ico: 'image',
  jxl: 'image',
  // video
  mp4: 'video',
  m4v: 'video',
  mov: 'video',
  webm: 'video',
  mkv: 'video',
  avi: 'video',
  mpeg: 'video',
  mpg: 'video',
  ogv: 'video',
  '3gp': 'video',
  ts: 'video',
  flv: 'video',
  wmv: 'video',
  // audio
  mp3: 'audio',
  m4a: 'audio',
  aac: 'audio',
  flac: 'audio',
  ogg: 'audio',
  opus: 'audio',
  wav: 'audio',
  wma: 'audio',
  aiff: 'audio',
  aif: 'audio',
  mka: 'audio',
  // document
  pdf: 'document',
  epub: 'document',
  doc: 'document',
  docx: 'document',
  xls: 'document',
  xlsx: 'document',
  ppt: 'document',
  pptx: 'document',
  odt: 'document',
  ods: 'document',
  odp: 'document',
  rtf: 'document',
  pages: 'document',
  numbers: 'document',
  key: 'document',
  // text & source code
  txt: 'text',
  md: 'text',
  markdown: 'text',
  log: 'text',
  csv: 'text',
  tsv: 'text',
  json: 'text',
  jsonl: 'text',
  toml: 'text',
  yaml: 'text',
  yml: 'text',
  xml: 'text',
  html: 'text',
  htm: 'text',
  css: 'text',
  scss: 'text',
  less: 'text',
  js: 'text',
  mjs: 'text',
  cjs: 'text',
  jsx: 'text',
  ts: 'text',
  tsx: 'text',
  rs: 'text',
  py: 'text',
  go: 'text',
  c: 'text',
  h: 'text',
  cc: 'text',
  cpp: 'text',
  hpp: 'text',
  java: 'text',
  kt: 'text',
  kts: 'text',
  swift: 'text',
  rb: 'text',
  php: 'text',
  sh: 'text',
  bash: 'text',
  zsh: 'text',
  fish: 'text',
  ps1: 'text',
  bat: 'text',
  cmd: 'text',
  sql: 'text',
  ini: 'text',
  cfg: 'text',
  conf: 'text',
  env: 'text',
  lock: 'text',
  diff: 'text',
  patch: 'text',
  // archive
  zip: 'archive',
  tar: 'archive',
  gz: 'archive',
  tgz: 'archive',
  bz2: 'archive',
  xz: 'archive',
  zst: 'archive',
  '7z': 'archive',
  rar: 'archive',
  dmg: 'archive',
  iso: 'archive',
  deb: 'archive',
  rpm: 'archive',
};

const EXTENSION_MIME = {
  jpg: 'image/jpeg',
  jpeg: 'image/jpeg',
  png: 'image/png',
  gif: 'image/gif',
  webp: 'image/webp',
  avif: 'image/avif',
  bmp: 'image/bmp',
  svg: 'image/svg+xml',
  tif: 'image/tiff',
  tiff: 'image/tiff',
  mp4: 'video/mp4',
  m4v: 'video/mp4',
  mov: 'video/quicktime',
  webm: 'video/webm',
  mkv: 'video/x-matroska',
  avi: 'video/x-msvideo',
  mp3: 'audio/mpeg',
  m4a: 'audio/mp4',
  aac: 'audio/aac',
  flac: 'audio/flac',
  ogg: 'audio/ogg',
  opus: 'audio/opus',
  wav: 'audio/wav',
  pdf: 'application/pdf',
  epub: 'application/epub+zip',
  txt: 'text/plain',
  md: 'text/markdown',
  json: 'application/json',
  toml: 'text/plain',
  yaml: 'text/plain',
  yml: 'text/plain',
  html: 'text/html',
  css: 'text/css',
  js: 'text/javascript',
  zip: 'application/zip',
  tar: 'application/x-tar',
  gz: 'application/gzip',
  '7z': 'application/x-7z-compressed',
};

const CATEGORY_SET = new Set(FILE_CATEGORIES);

/**
 * Extracts the lowercase file extension from a file name or path (excluding dotfiles without a
 * second dot).
 */
export function extensionFromName(name) {
  const base = baseFileName(name);
  const dot = base.lastIndexOf('.');
  if (dot <= 0 || dot === base.length - 1) return '';
  return base.slice(dot + 1).toLowerCase();
}

/**
 * Maps a filename to one of Legion's seven file categories by extension only (case-insensitive).
 */
export function categoryFromName(name) {
  const ext = extensionFromName(name);
  if (!ext) return 'other';
  return EXTENSION_CATEGORY[ext] || 'other';
}

/**
 * Derives a fallback MIME type from a filename when an older node omits `mime`.
 */
export function mimeFromName(name) {
  const ext = extensionFromName(name);
  if (!ext) return 'application/octet-stream';
  return EXTENSION_MIME[ext] || (EXTENSION_CATEGORY[ext] === 'text' ? 'text/plain' : 'application/octet-stream');
}

/**
 * Resolves the effective category of a listing or search entry, using the server's `category`
 * when present and falling back to the client extension table for older nodes.
 */
export function entryCategory(entry) {
  if (!entry) return 'other';
  if (entry.kind === 'dir') return 'folder';
  if (typeof entry.category === 'string' && CATEGORY_SET.has(entry.category.toLowerCase())) {
    return entry.category.toLowerCase();
  }
  return categoryFromName(entry.name || entry.path || '');
}

/**
 * Plain-language label for an entry's kind ("Folder", "Picture", "Video", "Music", "Document",
 * "Text", "Archive", "File").
 */
export function plainCategoryLabel(entry) {
  const cat = typeof entry === 'string' ? entry : entryCategory(entry);
  switch (cat) {
    case 'folder':
    case 'dir':
      return 'Folder';
    case 'image':
      return 'Picture';
    case 'video':
      return 'Video';
    case 'audio':
      return 'Music';
    case 'document':
      return 'Document';
    case 'text':
      return 'Text';
    case 'archive':
      return 'Archive';
    default:
      return 'File';
  }
}

/**
 * Icon symbol name in `icons.js` for a file or folder entry.
 */
export function entryIconName(entry) {
  const cat = typeof entry === 'string' ? entry : entryCategory(entry);
  switch (cat) {
    case 'folder':
    case 'dir':
      return 'files';
    case 'image':
      return 'image';
    case 'video':
      return 'video';
    case 'audio':
      return 'music';
    case 'archive':
      return 'archive';
    default:
      return 'doc';
  }
}

/**
 * Whether an entry can request a server thumbnail (`fs.thumb` supports pictures and video posters).
 */
export function isThumbEligible(entry) {
  if (!entry || entry.kind === 'dir') return false;
  const cat = entryCategory(entry);
  return cat === 'image' || cat === 'video';
}

/**
 * Whether a file/folder name is hidden (Unix dotfile).
 */
export function isHiddenName(name) {
  const base = baseFileName(name);
  return base.startsWith('.') && base !== '.' && base !== '..';
}

/**
 * Feature detection from a machine's tool catalogue, caller's allowed tool set, and link surface.
 * Nothing in the UI may break when a node lacks the newer Files tools or fields.
 */
export function detectFileCapabilities({ tools = [], allowedSet = null, link = null } = {}) {
  const map = new Map();
  for (const t of tools || []) {
    if (t && typeof t.name === 'string') map.set(t.name, t);
  }
  const hasTool = (name) => {
    const inCatalog = map.size === 0 ? false : map.has(name);
    if (!inCatalog) return false;
    return allowedSet ? allowedSet.has(name) : true;
  };
  const propOf = (toolName, propName) => {
    const spec = map.get(toolName);
    return Boolean(spec?.input_schema?.properties && Object.prototype.hasOwnProperty.call(spec.input_schema.properties, propName));
  };

  const hasList = map.size === 0 ? true : hasTool('fs.list');
  const hasRead = map.size === 0 ? true : hasTool('fs.read');
  const hasFind = hasTool('fs.find');
  const hasThumb = hasTool('fs.thumb');
  const hasFav = hasTool('fav.list');
  const hasFavWrite = hasFav && hasTool('fav.add') && hasTool('fav.remove');
  const hasSync = hasTool('sync.list');
  const hasSyncWrite = hasSync && (hasTool('sync.pause') || hasTool('sync.add'));
  const hasMkdir = hasTool('fs.mkdir');
  const hasCopy = hasTool('fs.copy');
  const hasMove = hasTool('fs.move');
  const hasRemove = hasTool('fs.remove');
  const hasHash = hasTool('fs.hash');
  const hasTransfer = hasTool('fs.transfer');
  const hasTransferBg = hasTransfer && propOf('fs.transfer', 'background');
  const hasTransferMove = hasTransfer && propOf('fs.transfer', 'move') && hasRemove;
  const hasTransferIfExists = hasTransfer && propOf('fs.transfer', 'if_exists');

  // Content URLs are supported when the link exposes `contentUrl` and (for HTTP against older
  // nodes) the node supports the new file capabilities or the link explicitly advertises content.
  const linkHasContent = typeof link?.contentUrl === 'function';
  const nodeSupportsContent =
    link?.kind === 'mock'
      ? linkHasContent
      : linkHasContent && (link?.supportsContent === true || hasThumb || propOf('fs.list', 'offset'));

  const hasDownload = typeof link?.download === 'function' && (link?.kind !== 'http' || nodeSupportsContent);
  const hasOpenExternally = typeof link?.openExternally === 'function';
  const hasUpload = typeof link?.upload === 'function';
  const hasDeviceTransfers = typeof link?.transfers === 'function';

  return {
    hasList,
    hasRead,
    hasListSort: propOf('fs.list', 'sort'),
    hasListOffset: propOf('fs.list', 'offset'),
    hasListCategories: propOf('fs.list', 'categories'),
    hasFind,
    hasFindEnhanced: hasFind && (propOf('fs.find', 'name') || propOf('fs.find', 'categories')),
    hasThumb,
    hasFav,
    hasFavWrite,
    hasSync,
    hasSyncWrite,
    hasMkdir,
    hasCopy,
    hasMove,
    hasRemove,
    hasHash,
    hasTransfer,
    hasTransferBg,
    hasTransferMove,
    hasTransferIfExists,
    hasContentUrl: nodeSupportsContent,
    hasDownload,
    hasOpenExternally,
    hasUpload,
    hasDeviceTransfers,
  };
}

/**
 * Sorts folder entries with directories always first, then by `name`, `modified`, or `size`.
 */
export function sortEntries(entries, { sort = 'name', order = 'asc' } = {}) {
  const list = Array.isArray(entries) ? [...entries] : [];
  const dir = order === 'desc' ? -1 : 1;

  const cmpName = (a, b) => {
    const an = String(a?.name || '').toLowerCase();
    const bn = String(b?.name || '').toLowerCase();
    if (an < bn) return -1;
    if (an > bn) return 1;
    const rawA = String(a?.name || '');
    const rawB = String(b?.name || '');
    return rawA < rawB ? -1 : rawA > rawB ? 1 : 0;
  };

  list.sort((a, b) => {
    const aDir = a?.kind === 'dir' ? 0 : 1;
    const bDir = b?.kind === 'dir' ? 0 : 1;
    if (aDir !== bDir) return aDir - bDir;

    if (sort === 'modified') {
      const ta = a?.mtime ? Date.parse(a.mtime) : 0;
      const tb = b?.mtime ? Date.parse(b.mtime) : 0;
      const diff = (Number.isFinite(ta) ? ta : 0) - (Number.isFinite(tb) ? tb : 0);
      if (diff !== 0) return diff * dir;
      return cmpName(a, b);
    }

    if (sort === 'size') {
      const sa = Number.isFinite(a?.size) ? a.size : 0;
      const sb = Number.isFinite(b?.size) ? b.size : 0;
      if (sa !== sb) return (sa - sb) * dir;
      return cmpName(a, b);
    }

    return cmpName(a, b) * dir;
  });

  return list;
}

/**
 * Filters entries on the client (hidden toggle, optional search query, categories, size/date).
 */
export function filterEntries(
  entries,
  { hidden = false, query = '', categories = [], minSize = null, maxSize = null, modifiedAfter = null } = {},
) {
  const list = Array.isArray(entries) ? entries : [];
  const words = String(query || '')
    .trim()
    .toLowerCase()
    .split(/\s+/)
    .filter(Boolean);
  const catSet = Array.isArray(categories) && categories.length > 0 ? new Set(categories) : null;
  const afterMs = modifiedAfter ? Date.parse(modifiedAfter) : NaN;

  return list.filter((entry) => {
    if (!entry) return false;
    if (!hidden && isHiddenName(entry.name || entry.path || '')) return false;

    if (words.length > 0) {
      const lowerName = String(entry.name || baseFileName(entry.path || '')).toLowerCase();
      for (const w of words) {
        if (!lowerName.includes(w)) return false;
      }
    }

    if (catSet) {
      const cat = entryCategory(entry);
      if (cat === 'folder') {
        if (!catSet.has('folder')) return false;
      } else if (!catSet.has(cat)) {
        return false;
      }
    }

    if (entry.kind !== 'dir') {
      if (minSize != null && (entry.size ?? 0) < minSize) return false;
      if (maxSize != null && (entry.size ?? 0) > maxSize) return false;
    }

    if (Number.isFinite(afterMs)) {
      const ts = entry.mtime ? Date.parse(entry.mtime) : NaN;
      if (!Number.isFinite(ts) || ts < afterMs) return false;
    }

    return true;
  });
}

/**
 * Chooses grid view by default when most non-folder entries in a folder are pictures or video.
 */
export function shouldDefaultToGrid(entries) {
  const files = (entries || []).filter(
    (e) => e && e.kind !== 'dir' && !isHiddenName(e.name || e.path || ''),
  );
  if (files.length < 2) return false;
  let media = 0;
  for (const f of files) {
    const cat = entryCategory(f);
    if (cat === 'image' || cat === 'video') media += 1;
  }
  return media / files.length > 0.5;
}

/**
 * Pure selection state helpers (Set of full paths).
 */
export function toggleSelection(selectedSet, path) {
  const next = new Set(selectedSet || []);
  if (next.has(path)) next.delete(path);
  else next.add(path);
  return next;
}

export function selectAllEntries(entries) {
  return new Set((entries || []).filter((e) => e?.path).map((e) => e.path));
}

export function selectionSummary(entries, selectedSet) {
  const set = selectedSet instanceof Set ? selectedSet : new Set(selectedSet || []);
  const picked = (entries || []).filter((e) => e && set.has(e.path));
  let totalBytes = 0;
  let fileCount = 0;
  let folderCount = 0;
  let allSizesKnown = picked.length > 0;
  for (const item of picked) {
    if (item.kind === 'dir') {
      folderCount += 1;
      allSizesKnown = false;
    } else {
      fileCount += 1;
      if (Number.isFinite(item.size)) totalBytes += item.size;
      else allSizesKnown = false;
    }
  }
  return {
    count: picked.length,
    fileCount,
    folderCount,
    totalBytes,
    allSizesKnown,
    items: picked,
  };
}

/**
 * Formats a folder's item count, respecting `total_exact === false` as a lower bound.
 */
export function formatFolderTotal(total, totalExact = true) {
  if (!Number.isFinite(total) || total < 0) return '';
  if (totalExact === false) {
    return `More than ${total.toLocaleString()} items`;
  }
  return total === 1 ? '1 item' : `${total.toLocaleString()} items`;
}

/**
 * Formats the folder pager status line, respecting `total_exact === false`.
 */
export function formatShowingCount(shown, total, totalExact = true) {
  const shownText = Number.isFinite(shown) ? shown.toLocaleString() : '0';
  if (!Number.isFinite(total) || total <= 0) return `Showing ${shownText}`;
  if (totalExact === false) {
    return `Showing ${shownText} of more than ${total.toLocaleString()}`;
  }
  return `Showing ${shownText} of ${total.toLocaleString()}`;
}

const RETRYABLE_THUMB_CODES = new Set([
  'busy',
  'rate_limited',
  'temporarily_unavailable',
  'timeout',
  'try_again',
  'overloaded',
]);

/**
 * Returns true when an `fs.thumb` per-path error means the node is temporarily busy decoding
 * and the caller should quietly retry after a short backoff.
 */
export function isRetryableThumbError(err) {
  if (!err || typeof err !== 'object') return false;
  if (err.retryable === true) return true;
  const code = typeof err.code === 'string' ? err.code.toLowerCase() : '';
  return RETRYABLE_THUMB_CODES.has(code);
}

/**
 * Path manipulation for Unix, `~`, Windows drive (`C:\...`), and UNC (`\\server\share`) paths.
 */
export function isWindowsPath(filePath) {
  return typeof filePath === 'string' && (/^[A-Za-z]:[\\/]/.test(filePath) || /^[A-Za-z]:$/.test(filePath) || filePath.startsWith('\\\\'));
}

export function baseFileName(filePath) {
  if (!filePath || typeof filePath !== 'string') return '';
  if (filePath === '/' || filePath === '~') return filePath;
  if (/^[A-Za-z]:\\?$/.test(filePath)) return filePath.slice(0, 2);
  const cleaned = filePath.replace(/[/\\]+$/, '');
  const idx = Math.max(cleaned.lastIndexOf('/'), cleaned.lastIndexOf('\\'));
  return idx === -1 ? cleaned : cleaned.slice(idx + 1);
}

export function parentFilePath(filePath) {
  if (!filePath || typeof filePath !== 'string') return null;
  if (filePath === '/' || filePath === '~') return null;
  if (/^[A-Za-z]:\\?$/.test(filePath)) return null;
  if (isWindowsPath(filePath)) {
    const cleaned = filePath.replace(/\\+$/, '');
    const idx = cleaned.lastIndexOf('\\');
    if (idx <= 2) return `${cleaned.slice(0, 2)}\\`;
    return cleaned.slice(0, idx);
  }
  const cleaned = filePath.replace(/\/+$/, '');
  const idx = cleaned.lastIndexOf('/');
  if (idx < 0) return null;
  if (idx === 0) return '/';
  return cleaned.slice(0, idx);
}

export function joinFilePath(dir, name) {
  const cleanName = String(name || '').replace(/^[/\\]+/, '');
  if (!dir) return cleanName;
  if (!cleanName) return dir;
  if (isWindowsPath(dir)) {
    const sep = dir.endsWith('\\') ? '' : '\\';
    return `${dir}${sep}${cleanName}`;
  }
  const sep = dir.endsWith('/') ? '' : '/';
  return `${dir}${sep}${cleanName}`;
}

/**
 * Turns a machine + path into clickable breadcrumb items for the `.where` path line.
 * In plain mode, collapses `/home/<user>` or `/Users/<user>` or `C:\Users\<user>` to `Home`.
 */
export function splitFileCrumbs(machine, fullPath, { technical = false, homePath = null } = {}) {
  const crumbs = [];
  if (machine) {
    crumbs.push({ label: machine, path: '~', isMachine: true });
  }
  if (!fullPath || fullPath === '~') {
    crumbs.push({ label: technical ? '~' : 'Home', path: '~' });
    return crumbs;
  }

  if (isWindowsPath(fullPath)) {
    const norm = fullPath.replace(/\//g, '\\');
    const homeMatch = !technical ? norm.match(/^([A-Za-z]:\\Users\\[^\\]+)(?:\\(.*))?$/i) : null;
    if (homeMatch) {
      const homeRoot = homeMatch[1];
      crumbs.push({ label: 'Home', path: homeRoot });
      const rest = (homeMatch[2] || '').split('\\').filter(Boolean);
      let acc = homeRoot;
      for (const seg of rest) {
        acc = `${acc}\\${seg}`;
        crumbs.push({ label: seg, path: acc });
      }
      return crumbs;
    }
    const parts = norm.split(/\\+/).filter(Boolean);
    let acc = '';
    for (let i = 0; i < parts.length; i += 1) {
      acc = i === 0 ? `${parts[i]}\\` : `${acc}${acc.endsWith('\\') ? '' : '\\'}${parts[i]}`;
      crumbs.push({ label: parts[i], path: acc });
    }
    return crumbs;
  }

  if (fullPath === '/') {
    crumbs.push({ label: '/', path: '/' });
    return crumbs;
  }

  const normalized = fullPath.replace(/\/+$/, '');
  const detectedHome =
    !technical &&
    (homePath && (normalized === homePath || normalized.startsWith(`${homePath}/`))
      ? homePath
      : normalized.match(/^\/(?:home|Users)\/[^/]+/)?.[0] || null);

  if (detectedHome) {
    crumbs.push({ label: 'Home', path: detectedHome });
    const rest = normalized.slice(detectedHome.length).split('/').filter(Boolean);
    let acc = detectedHome;
    for (const seg of rest) {
      acc += `/${seg}`;
      crumbs.push({ label: seg, path: acc });
    }
    return crumbs;
  }

  if (normalized.startsWith('~/')) {
    crumbs.push({ label: technical ? '~' : 'Home', path: '~' });
    const rest = normalized.slice(2).split('/').filter(Boolean);
    let acc = '~';
    for (const seg of rest) {
      acc += `/${seg}`;
      crumbs.push({ label: seg, path: acc });
    }
    return crumbs;
  }

  crumbs.push({ label: '/', path: '/' });
  const parts = normalized.split('/').filter(Boolean);
  let acc = '';
  for (const seg of parts) {
    acc += `/${seg}`;
    crumbs.push({ label: seg, path: acc });
  }
  return crumbs;
}

/**
 * Formats a folder path concisely for search results and destination summaries
 * (e.g. "Home › Videos › Holiday" in plain mode, or raw path in technical mode).
 */
export function formatPathForDisplay(fullPath, { technical = false, homePath = null } = {}) {
  if (!fullPath) return '';
  if (technical) return fullPath;
  const crumbs = splitFileCrumbs(null, fullPath, { technical: false, homePath });
  return crumbs.map((c) => c.label).join(' › ');
}

/**
 * Friendly folder heading from a path (`Holiday`, `Home`, `/`, `C:`).
 */
export function folderDisplayName(fullPath, { technical = false, homePath = null } = {}) {
  if (!fullPath || fullPath === '~') return technical ? '~' : 'Home';
  if (!technical) {
    if (homePath && fullPath === homePath) return 'Home';
    if (/^\/(?:home|Users)\/[^/]+\/?$/.test(fullPath)) return 'Home';
    if (/^[A-Za-z]:\\Users\\[^\\]+\\?$/i.test(fullPath)) return 'Home';
  }
  return baseFileName(fullPath) || fullPath;
}

/**
 * Computes the logical fallback back-step route for Files when there is no in-session browser
 * history (e.g. after reloading or deep-linking a URL).
 */
export function filesFallbackBackRoute(route, { hasSheet = false, selecting = false } = {}) {
  if (!route || route.section !== 'files') return null;
  if (hasSheet) return { action: 'close-sheet' };
  if (selecting) return { action: 'end-select' };
  if (route.viewer) {
    return { action: 'navigate', href: routeHref({ ...route, viewer: null }) };
  }
  if (route.view === 'folder' && route.machine) {
    const parent = parentFilePath(route.path);
    const isHome =
      !parent ||
      route.path === '~' ||
      /^\/(?:home|Users)\/[^/]+\/?$/.test(route.path || '') ||
      /^[A-Za-z]:\\Users\\[^\\]+\\?$/i.test(route.path || '');
    if (isHome) {
      return { action: 'navigate', href: '#/files' };
    }
    return {
      action: 'navigate',
      href: routeHref({ section: 'files', view: 'folder', machine: route.machine, path: parent }),
    };
  }
  if (route.view === 'search' || route.view === 'favourites' || route.view === 'recent' || route.view === 'sync') {
    if (route.view === 'search' && route.machine && route.path) {
      return {
        action: 'navigate',
        href: routeHref({ section: 'files', view: 'folder', machine: route.machine, path: route.path }),
      };
    }
    return { action: 'navigate', href: '#/files' };
  }
  return { action: 'leave-files' };
}

/**
 * Translates a search size preset into `{ min_size, max_size }` bytes.
 */
export function sizePresetBounds(preset) {
  switch (preset) {
    case 'small':
      return { min_size: null, max_size: 1024 * 1024 };
    case 'medium':
      return { min_size: 1024 * 1024, max_size: 100 * 1024 * 1024 };
    case 'large':
      return { min_size: 100 * 1024 * 1024, max_size: null };
    default:
      return { min_size: null, max_size: null };
  }
}

/**
 * Translates a search changed-time preset into an ISO cutoff timestamp.
 */
export function changedPresetCutoff(preset, nowMs = Date.now()) {
  const day = 86_400_000;
  switch (preset) {
    case 'day':
      return new Date(nowMs - day).toISOString();
    case 'week':
      return new Date(nowMs - 7 * day).toISOString();
    case 'month':
      return new Date(nowMs - 30 * day).toISOString();
    case 'year':
      return new Date(nowMs - 365 * day).toISOString();
    default:
      return null;
  }
}

/**
 * Human-readable sort label for the dock's Sort button accessible name.
 */
export function sortDescription({ sort = 'name', order = 'asc' } = {}) {
  if (sort === 'modified') return order === 'desc' ? 'newest first' : 'oldest first';
  if (sort === 'size') return order === 'desc' ? 'largest first' : 'smallest first';
  return order === 'desc' ? 'by name (Z–A)' : 'by name';
}

/**
 * Computes destination path when sending an item to a destination folder.
 */
export function buildTransferDestination(destDir, sourcePath) {
  const name = baseFileName(sourcePath);
  return joinFilePath(destDir || '~', name);
}
