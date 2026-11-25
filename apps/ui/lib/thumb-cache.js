// Bounded LRU thumbnail cache (memory + optional Web Storage) keyed by machine, path, mtime, and size.

const STORAGE_PREFIX = 'legion.thumb.v1.';

export function thumbCacheKey(machine, path, mtime = '', size = 128) {
  return `${machine}\0${path}\0${mtime || ''}\0${size}`;
}

export function createThumbCache({ maxEntries = 180, maxBytes = 4 * 1024 * 1024, storage = null } = {}) {
  const entries = new Map();
  let totalBytes = 0;

  function estimateBytes(item) {
    if (!item) return 0;
    return (item.url?.length || 0) * 2 + 64;
  }

  function evictIfNeeded() {
    while (entries.size > maxEntries || totalBytes > maxBytes) {
      const oldestKey = entries.keys().next().value;
      if (oldestKey === undefined) break;
      const old = entries.get(oldestKey);
      entries.delete(oldestKey);
      totalBytes = Math.max(0, totalBytes - estimateBytes(old));
      if (storage && typeof storage.removeItem === 'function') {
        try {
          storage.removeItem(STORAGE_PREFIX + oldestKey);
        } catch {
          // Ignore storage eviction errors.
        }
      }
    }
  }

  return {
    get(key) {
      if (entries.has(key)) {
        const val = entries.get(key);
        // Refresh LRU order.
        entries.delete(key);
        entries.set(key, val);
        return val;
      }
      if (storage && typeof storage.getItem === 'function') {
        try {
          const raw = storage.getItem(STORAGE_PREFIX + key);
          if (raw) {
            const parsed = JSON.parse(raw);
            if (parsed && typeof parsed === 'object') {
              entries.set(key, parsed);
              totalBytes += estimateBytes(parsed);
              evictIfNeeded();
              return parsed;
            }
          }
        } catch {
          // Ignore corrupt storage entry.
        }
      }
      return null;
    },

    set(key, value) {
      if (entries.has(key)) {
        const old = entries.get(key);
        totalBytes = Math.max(0, totalBytes - estimateBytes(old));
        entries.delete(key);
      }
      entries.set(key, value);
      totalBytes += estimateBytes(value);
      evictIfNeeded();
      if (storage && typeof storage.setItem === 'function' && value && !value.failed) {
        try {
          storage.setItem(STORAGE_PREFIX + key, JSON.stringify(value));
        } catch {
          // Ignore quota exceeded.
        }
      }
    },

    has(key) {
      return entries.has(key);
    },

    size() {
      return entries.size;
    },

    bytes() {
      return totalBytes;
    },

    clear() {
      entries.clear();
      totalBytes = 0;
    },
  };
}
