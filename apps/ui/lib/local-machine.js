// Desktop local-pipe machine identity helpers and quiet one-time storage migration.
//
// The word "local" names a connection pipe, never a machine shown to a person.

import { createResumeStore, RESUME_STORAGE_KEY } from './player.js';

export const LOCAL_PIPE_CONNECTION = 'local';
export const PENDING_LOCAL_MACHINE_NAME = 'This computer';

const RECENT_FILES_KEY = 'legion.files.recent.v1';
const RECENT_DIRS_KEY = 'legion.files.recentDirs.v1';
const VIEW_MODE_KEY = 'legion.files.viewMode.v1';
const FLEET_CACHE_KEY = 'legion.cache.fleet';
const NODE_IDENTITY_PREFIX = 'legion.nodeIdentity.';
const THUMB_KEY_PREFIX = 'legion.thumb.v1.';
const TOOLS_CACHE_PREFIX = 'legion.cache.tools:';

const MIGRATE_FROM_NAMES = [LOCAL_PIPE_CONNECTION, PENDING_LOCAL_MACHINE_NAME];

export function isLocalConnectionName(name) {
  return typeof name === 'string' && name.trim() === LOCAL_PIPE_CONNECTION;
}

export function isPendingLocalName(name) {
  return typeof name === 'string' && name.trim() === PENDING_LOCAL_MACHINE_NAME;
}

export function isRealMachineName(name) {
  if (typeof name !== 'string') return false;
  const trimmed = name.trim();
  return Boolean(trimmed && trimmed !== LOCAL_PIPE_CONNECTION && trimmed !== PENDING_LOCAL_MACHINE_NAME);
}

function shouldMigrateName(name) {
  return name === LOCAL_PIPE_CONNECTION || name === PENDING_LOCAL_MACHINE_NAME;
}

function readJsonRaw(storage, key) {
  if (!storage || typeof storage.getItem !== 'function') return { exists: false, value: null };
  try {
    const raw = storage.getItem(key);
    if (raw == null) return { exists: false, value: null };
    return { exists: true, value: JSON.parse(raw) };
  } catch {
    return { exists: false, value: null };
  }
}

function writeJsonRaw(storage, key, value) {
  if (!storage || typeof storage.setItem !== 'function') return;
  try {
    storage.setItem(key, JSON.stringify(value));
  } catch {
    // Ignore storage quota or private-mode errors.
  }
}

function removeKey(storage, key) {
  if (!storage || typeof storage.removeItem !== 'function') return;
  try {
    storage.removeItem(key);
  } catch {
    // Ignore storage errors.
  }
}

function listStorageKeys(storage) {
  if (!storage) return [];
  if (typeof storage.length === 'number' && typeof storage.key === 'function') {
    const out = [];
    for (let i = 0; i < storage.length; i += 1) {
      const k = storage.key(i);
      if (typeof k === 'string') out.push(k);
    }
    return out;
  }
  try {
    return Object.keys(storage);
  } catch {
    return [];
  }
}

function migrateResumeValue(value, realName) {
  if (Array.isArray(value)) {
    let changed = false;
    const next = value.map((item) => {
      if (item && typeof item === 'object' && shouldMigrateName(item.machine)) {
        changed = true;
        return { ...item, machine: realName };
      }
      return item;
    });
    return { changed, value: next };
  }
  if (!value || typeof value !== 'object') {
    return { changed: false, value };
  }
  let changed = false;
  const next = { ...value };
  for (const key of Object.keys(value)) {
    if (shouldMigrateName(key)) {
      if (!(realName in next)) {
        next[realName] = value[key];
      }
      delete next[key];
      changed = true;
      continue;
    }
    for (const from of MIGRATE_FROM_NAMES) {
      const prefix = `${from}\0`;
      if (key.startsWith(prefix)) {
        const destKey = `${realName}\0${key.slice(prefix.length)}`;
        if (!(destKey in next)) {
          next[destKey] = value[key];
        }
        delete next[key];
        changed = true;
        break;
      }
      const colonPrefix = `${from}:`;
      if (key.startsWith(colonPrefix)) {
        const destKey = `${realName}:${key.slice(colonPrefix.length)}`;
        if (!(destKey in next)) {
          next[destKey] = value[key];
        }
        delete next[key];
        changed = true;
        break;
      }
    }
  }
  return { changed, value: next };
}

/**
 * Quietly migrates interface storage entries keyed under `"local"` (or the pending
 * `"This computer"` placeholder) to the node's real name. Idempotent and safe to call whenever
 * the local node's real name is known.
 */
export function migrateLocalMachineStorage(realNodeName, { localStorage = null, sessionStorage = null } = {}) {
  if (!isRealMachineName(realNodeName)) return { migrated: false, keys: [] };
  const target = realNodeName.trim();
  const touched = [];

  if (localStorage) {
    // 1. Recent files (`legion.files.recent.v1`)
    const recent = readJsonRaw(localStorage, RECENT_FILES_KEY);
    if (recent.exists && Array.isArray(recent.value)) {
      let changed = false;
      const byKey = new Map();
      const order = [];
      for (const item of recent.value) {
        if (!item || typeof item !== 'object') continue;
        const nextItem = shouldMigrateName(item.machine)
          ? ((changed = true), { ...item, machine: target })
          : item;
        const dedupeKey = `${nextItem.machine || ''}\0${nextItem.path || ''}`;
        const prev = byKey.get(dedupeKey);
        if (!prev) {
          byKey.set(dedupeKey, nextItem);
          order.push(dedupeKey);
        } else {
          changed = true;
          const prevTs = Number(prev.openedAt) || 0;
          const nextTs = Number(nextItem.openedAt) || 0;
          if (nextTs > prevTs) {
            byKey.set(dedupeKey, nextItem);
          }
        }
      }
      if (changed) {
        writeJsonRaw(
          localStorage,
          RECENT_FILES_KEY,
          order.map((k) => byKey.get(k)),
        );
        touched.push(RECENT_FILES_KEY);
      }
    }

    // 2. Remembered Send-to destination folders (`legion.files.recentDirs.v1`)
    const recentDirs = readJsonRaw(localStorage, RECENT_DIRS_KEY);
    if (recentDirs.exists && recentDirs.value && typeof recentDirs.value === 'object' && !Array.isArray(recentDirs.value)) {
      const map = { ...recentDirs.value };
      let changed = false;
      const existing = Array.isArray(map[target]) ? [...map[target]] : [];
      for (const from of MIGRATE_FROM_NAMES) {
        if (Object.prototype.hasOwnProperty.call(map, from)) {
          const fromList = Array.isArray(map[from]) ? map[from] : [];
          for (const dir of fromList) {
            if (typeof dir === 'string' && dir && !existing.includes(dir)) {
              existing.push(dir);
            }
          }
          delete map[from];
          changed = true;
        }
      }
      if (changed) {
        if (existing.length > 0) {
          map[target] = existing.slice(0, 8);
        }
        writeJsonRaw(localStorage, RECENT_DIRS_KEY, map);
        touched.push(RECENT_DIRS_KEY);
      }
    }

    // 3. Remembered per-folder view mode (`legion.files.viewMode.v1`)
    const viewModes = readJsonRaw(localStorage, VIEW_MODE_KEY);
    if (viewModes.exists && viewModes.value && typeof viewModes.value === 'object' && !Array.isArray(viewModes.value)) {
      const map = { ...viewModes.value };
      let changed = false;
      for (const key of Object.keys(viewModes.value)) {
        for (const from of MIGRATE_FROM_NAMES) {
          const prefix = `${from}\0`;
          if (key.startsWith(prefix)) {
            const destKey = `${target}\0${key.slice(prefix.length)}`;
            if (!(destKey in map)) {
              map[destKey] = viewModes.value[key];
            }
            delete map[key];
            changed = true;
            break;
          }
        }
      }
      if (changed) {
        writeJsonRaw(localStorage, VIEW_MODE_KEY, map);
        touched.push(VIEW_MODE_KEY);
      }
    }

    // 4. Cached fleet snapshot (`legion.cache.fleet`)
    const fleetCache = readJsonRaw(localStorage, FLEET_CACHE_KEY);
    if (fleetCache.exists && fleetCache.value && typeof fleetCache.value === 'object') {
      const env = fleetCache.value;
      const payload = env.value && typeof env.value === 'object' ? env.value : env;
      if (payload && Array.isArray(payload.machines)) {
        let changed = false;
        let nextNode = payload.node;
        if (shouldMigrateName(nextNode)) {
          nextNode = target;
          changed = true;
        }
        const byName = new Map();
        for (const m of payload.machines) {
          if (!m || typeof m.name !== 'string' || !m.name) continue;
          const isOld = shouldMigrateName(m.name);
          const name = isOld ? target : m.name;
          if (isOld) changed = true;
          const entry = isOld ? { ...m, name, thisNode: true, isLocalComputer: true } : m;
          const prev = byName.get(name);
          if (!prev) {
            byName.set(name, entry);
          } else {
            changed = true;
            byName.set(name, {
              ...prev,
              ...entry,
              name,
              thisNode: Boolean(prev.thisNode || entry.thisNode),
              isLocalComputer: Boolean(prev.isLocalComputer || entry.isLocalComputer),
              reachable: Boolean(prev.reachable || entry.reachable),
              lastSeenAt: Math.max(prev.lastSeenAt || 0, entry.lastSeenAt || 0) || null,
              lastCheckedAt: Math.max(prev.lastCheckedAt || 0, entry.lastCheckedAt || 0) || null,
            });
          }
        }
        if (changed) {
          const nextPayload = {
            ...payload,
            node: nextNode,
            machines: Array.from(byName.values()),
          };
          const nextEnv =
            env.value && typeof env.value === 'object' ? { ...env, value: nextPayload } : nextPayload;
          writeJsonRaw(localStorage, FLEET_CACHE_KEY, nextEnv);
          touched.push(FLEET_CACHE_KEY);
        }
      }
    }

    // 5. Cached node identity (`legion.nodeIdentity.local` -> `legion.nodeIdentity.<node>`)
    for (const from of MIGRATE_FROM_NAMES) {
      const oldIdKey = `${NODE_IDENTITY_PREFIX}${from}`;
      const oldId = readJsonRaw(localStorage, oldIdKey);
      if (oldId.exists) {
        const destIdKey = `${NODE_IDENTITY_PREFIX}${target}`;
        const existingId = readJsonRaw(localStorage, destIdKey);
        if (!existingId.exists) {
          writeJsonRaw(localStorage, destIdKey, oldId.value);
        }
        removeKey(localStorage, oldIdKey);
        touched.push(oldIdKey);
      }
    }

    // 5b. Player resume positions (`legion.player.resume.v1`)
    const resumeStore = createResumeStore(localStorage);
    let resumeMigrated = false;
    for (const from of MIGRATE_FROM_NAMES) {
      if (resumeStore.renameMachine(from, target)) {
        resumeMigrated = true;
      }
    }
    if (resumeMigrated) {
      touched.push(RESUME_STORAGE_KEY);
    }

    // 6. Thumbnail cache keys (`legion.thumb.v1.local\0...`) and any other resume-position storage keys
    const allKeys = listStorageKeys(localStorage);
    for (const key of allKeys) {
      for (const from of MIGRATE_FROM_NAMES) {
        const thumbPrefix = `${THUMB_KEY_PREFIX}${from}\0`;
        if (key.startsWith(thumbPrefix)) {
          const destKey = `${THUMB_KEY_PREFIX}${target}\0${key.slice(thumbPrefix.length)}`;
          try {
            if (localStorage.getItem(destKey) == null) {
              const raw = localStorage.getItem(key);
              if (raw != null) localStorage.setItem(destKey, raw);
            }
          } catch {
            // Ignore storage errors.
          }
          removeKey(localStorage, key);
          touched.push(key);
          break;
        }
      }
      if (key !== RESUME_STORAGE_KEY && /^legion\.(?:files\.|player\.)?resume/i.test(key)) {
        const parsed = readJsonRaw(localStorage, key);
        if (parsed.exists) {
          const res = migrateResumeValue(parsed.value, target);
          if (res.changed) {
            writeJsonRaw(localStorage, key, res.value);
            touched.push(key);
          }
        }
      }
    }
  }

  // 7. Session-cached tools catalogue (`legion.cache.tools:local`)
  if (sessionStorage) {
    for (const from of MIGRATE_FROM_NAMES) {
      const oldKey = `${TOOLS_CACHE_PREFIX}${from}`;
      const existingOld = readJsonRaw(sessionStorage, oldKey);
      if (existingOld.exists) {
        const destKey = `${TOOLS_CACHE_PREFIX}${target}`;
        const existingDest = readJsonRaw(sessionStorage, destKey);
        if (!existingDest.exists) {
          writeJsonRaw(sessionStorage, destKey, existingOld.value);
        }
        removeKey(sessionStorage, oldKey);
        touched.push(oldKey);
      }
    }
  }

  return { migrated: touched.length > 0, keys: touched };
}

/**
 * Normalizes a `fav.list` entries array so any entry stored under `"local"` (or `"This computer"`)
 * appears under `realNodeName` without duplicates.
 */
export function normalizeFavouritesMachine(entries, realNodeName) {
  const list = Array.isArray(entries) ? entries : [];
  if (!isRealMachineName(realNodeName)) return list;
  const target = realNodeName.trim();
  const byKey = new Map();
  const order = [];

  for (const item of list) {
    if (!item || typeof item !== 'object') continue;
    const isOld = shouldMigrateName(item.machine);
    const normalized = isOld ? { ...item, machine: target } : item;
    const key = `${normalized.machine || ''}\0${normalized.path || ''}`;
    const prev = byKey.get(key);
    if (!prev) {
      byKey.set(key, normalized);
      order.push(key);
    } else {
      // Prefer the entry already stored natively under `target`, preserving pinned if either was pinned.
      const winner = isOld ? prev : normalized;
      byKey.set(key, {
        ...winner,
        pinned: Boolean(prev.pinned || normalized.pinned),
      });
    }
  }

  return order.map((k) => byKey.get(k));
}

/**
 * Quietly migrates any server-side favourite or pinned entries keyed under `"local"` onto
 * `realNodeName`. Never throws; returns `{ migrated, entries }`.
 */
export async function migrateLocalFavouritesOnNode({
  entries,
  realNodeName,
  link,
  localNode,
  canWrite = false,
}) {
  const list = Array.isArray(entries) ? entries : [];
  if (!isRealMachineName(realNodeName)) {
    return { migrated: false, entries: list };
  }
  const target = realNodeName.trim();
  const oldEntries = list.filter((e) => e && shouldMigrateName(e.machine));
  if (oldEntries.length === 0) {
    return { migrated: false, entries: list };
  }
  const normalized = normalizeFavouritesMachine(list, target);
  if (!canWrite || typeof link?.call !== 'function' || !localNode) {
    return { migrated: false, entries: normalized };
  }

  const nativeByPath = new Map();
  for (const e of list) {
    if (e && e.machine === target && e.path) {
      nativeByPath.set(e.path, e);
    }
  }

  let migratedAny = false;
  for (const old of oldEntries) {
    if (!old.path) continue;
    try {
      const existing = nativeByPath.get(old.path);
      if (existing) {
        if (old.pinned && !existing.pinned && existing.id) {
          await link.call('fav.set', { id: existing.id, pinned: true }, { on: localNode });
        }
        if (old.id) {
          await link.call('fav.remove', { id: old.id }, { on: localNode });
        }
        migratedAny = true;
      } else {
        const added = await link.call(
          'fav.add',
          {
            machine: target,
            path: old.path,
            label: old.label || '',
            pinned: Boolean(old.pinned),
          },
          { on: localNode },
        );
        if (added?.entry) {
          nativeByPath.set(old.path, added.entry);
        }
        if (old.id) {
          await link.call('fav.remove', { id: old.id }, { on: localNode });
        }
        migratedAny = true;
      }
    } catch {
      // Migration is best-effort and stays quiet; normalized in-memory entries remain usable.
    }
  }

  return { migrated: migratedAny, entries: normalized };
}
