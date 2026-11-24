// Stale-while-revalidate cache backed by memory and an optional Web Storage object.

export function createCache(storage = null, prefix = 'legion.cache.') {
  const memory = new Map();

  function readStorage(key) {
    if (!storage) return null;
    try {
      const raw = storage.getItem(prefix + key);
      if (!raw) return null;
      const parsed = JSON.parse(raw);
      if (!parsed || typeof parsed !== 'object') return null;
      return parsed;
    } catch {
      return null;
    }
  }

  function writeStorage(key, entry) {
    if (!storage) return;
    try {
      storage.setItem(prefix + key, JSON.stringify(entry));
    } catch {
      // Ignore quota or disabled storage errors.
    }
  }

  return {
    get(key) {
      if (memory.has(key)) return memory.get(key);
      const fromStore = readStorage(key);
      if (fromStore) {
        memory.set(key, fromStore);
        return fromStore;
      }
      return null;
    },
    set(key, value, now = Date.now()) {
      const entry = { value, updatedAt: now };
      memory.set(key, entry);
      writeStorage(key, entry);
      return entry;
    },
    delete(key) {
      memory.delete(key);
      if (storage) {
        try {
          storage.removeItem(prefix + key);
        } catch {
          // Ignore.
        }
      }
    },
    clear() {
      memory.clear();
      if (storage && typeof storage.length === 'number') {
        const keys = [];
        for (let i = 0; i < storage.length; i += 1) {
          const k = storage.key(i);
          if (k && k.startsWith(prefix)) keys.push(k);
        }
        for (const k of keys) {
          try {
            storage.removeItem(k);
          } catch {
            // Ignore.
          }
        }
      }
    },
    isFresh(key, maxAgeMs, now = Date.now()) {
      const entry = this.get(key);
      if (!entry) return false;
      return now - entry.updatedAt < maxAgeMs;
    },
  };
}
