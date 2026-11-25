// Device-local Files preferences: recent opened files, recent Send-to destinations, view mode, and sort.

const RECENT_FILES_KEY = 'legion.files.recent.v1';
const RECENT_DIRS_KEY = 'legion.files.recentDirs.v1';
const VIEW_MODE_KEY = 'legion.files.viewMode.v1';
const SORT_PREF_KEY = 'legion.files.sort.v1';
const TEXT_WRAP_KEY = 'legion.files.textWrap.v1';

const MAX_RECENT_FILES = 40;
const MAX_RECENT_DIRS_PER_MACHINE = 8;
const MAX_VIEW_MODES = 200;

function readJson(storage, key, fallback) {
  if (!storage || typeof storage.getItem !== 'function') return fallback;
  try {
    const raw = storage.getItem(key);
    if (!raw) return fallback;
    const parsed = JSON.parse(raw);
    return parsed ?? fallback;
  } catch {
    return fallback;
  }
}

function writeJson(storage, key, value) {
  if (!storage || typeof storage.setItem !== 'function') return;
  try {
    storage.setItem(key, JSON.stringify(value));
  } catch {
    // Ignore storage quota or private-mode errors.
  }
}

export function readRecentFiles(storage) {
  const list = readJson(storage, RECENT_FILES_KEY, []);
  return Array.isArray(list) ? list : [];
}

export function recordRecentFile(storage, item, nowMs = Date.now()) {
  if (!item || !item.machine || !item.path) return readRecentFiles(storage);
  const existing = readRecentFiles(storage).filter(
    (x) => !(x.machine === item.machine && x.path === item.path),
  );
  const next = [
    {
      machine: item.machine,
      path: item.path,
      name: item.name || '',
      kind: item.kind || 'file',
      category: item.category || null,
      size: Number.isFinite(item.size) ? item.size : null,
      mtime: item.mtime || null,
      openedAt: nowMs,
    },
    ...existing,
  ].slice(0, MAX_RECENT_FILES);
  writeJson(storage, RECENT_FILES_KEY, next);
  return next;
}

export function readRecentDestinations(storage, machine) {
  const map = readJson(storage, RECENT_DIRS_KEY, {});
  if (!map || typeof map !== 'object') return [];
  const list = map[machine];
  return Array.isArray(list) ? list : [];
}

export function recordRecentDestination(storage, machine, dirPath) {
  if (!machine || !dirPath) return [];
  const map = readJson(storage, RECENT_DIRS_KEY, {});
  const safe = map && typeof map === 'object' ? { ...map } : {};
  const existing = Array.isArray(safe[machine]) ? safe[machine] : [];
  const next = [dirPath, ...existing.filter((p) => p !== dirPath)].slice(
    0,
    MAX_RECENT_DIRS_PER_MACHINE,
  );
  safe[machine] = next;
  writeJson(storage, RECENT_DIRS_KEY, safe);
  return next;
}

export function readFolderViewMode(storage, machine, dirPath) {
  if (!machine || !dirPath) return null;
  const map = readJson(storage, VIEW_MODE_KEY, {});
  const val = map?.[`${machine}\0${dirPath}`];
  return val === 'grid' || val === 'list' ? val : null;
}

export function writeFolderViewMode(storage, machine, dirPath, mode) {
  if (!machine || !dirPath || (mode !== 'grid' && mode !== 'list')) return;
  const raw = readJson(storage, VIEW_MODE_KEY, {});
  const map = raw && typeof raw === 'object' ? { ...raw } : {};
  const key = `${machine}\0${dirPath}`;
  delete map[key];
  map[key] = mode;
  const keys = Object.keys(map);
  if (keys.length > MAX_VIEW_MODES) {
    for (const oldKey of keys.slice(0, keys.length - MAX_VIEW_MODES)) {
      delete map[oldKey];
    }
  }
  writeJson(storage, VIEW_MODE_KEY, map);
}

export function readSortPreference(storage) {
  const pref = readJson(storage, SORT_PREF_KEY, null);
  const validSorts = new Set(['name', 'modified', 'size']);
  const validOrders = new Set(['asc', 'desc']);
  if (pref && validSorts.has(pref.sort) && validOrders.has(pref.order)) {
    return { sort: pref.sort, order: pref.order };
  }
  return { sort: 'name', order: 'asc' };
}

export function writeSortPreference(storage, pref) {
  if (!pref) return;
  writeJson(storage, SORT_PREF_KEY, {
    sort: pref.sort || 'name',
    order: pref.order || 'asc',
  });
}

export function readTextWrapPreference(storage) {
  const val = readJson(storage, TEXT_WRAP_KEY, true);
  return val !== false;
}

export function writeTextWrapPreference(storage, wrap) {
  writeJson(storage, TEXT_WRAP_KEY, Boolean(wrap));
}
