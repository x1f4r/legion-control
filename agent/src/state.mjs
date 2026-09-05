// Durable state, written under a real lock, split by what losing it costs.
//
//   state/service-<id>.json  what happened to one service: its last update, the
//                            version waiting for an idle window, how many times
//                            a staged build has refused to apply. Losing an
//                            entry here loses a pending update or a failure
//                            counter, so every write is serialized and every
//                            failure is propagated to the caller.
//   state/cache.json         reconstructible lookups (registry versions, npm
//                            prefixes). Losing one costs a network round trip,
//                            so it is kept out of the way of the state that
//                            matters and its failures are only worth logging.
//   state/.lock.sqlite       the one lock both go through, held by the operating
//                            system rather than by a pid written into a file.
//                            See mutex.mjs.
//
// The old layout put everything in a single state.json with an unlocked
// read-modify-write and one shared "state.json.tmp" name, so two agent
// invocations could overwrite each other's work or truncate each other's temp
// file. Eight writers doing fifty writes each ended up with five keys. That file
// is migrated on the first 3.x run and then left alone.

import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { basePath, stateDir } from './config.mjs';
import { acquireMutex, mutexPath } from './mutex.mjs';

/** How long we are willing to wait for another process to finish its write. */
const LOCK_WAIT_MS = 10000;

function isPlainObject(value) {
  return Boolean(value) && typeof value === 'object' && !Array.isArray(value);
}

/** The single lock every state write goes through. */
export function stateLockPath() {
  return path.join(stateDir(), '.lock');
}

export function cachePath() {
  return path.join(stateDir(), 'cache.json');
}

export function serviceStatePath(serviceId) {
  return path.join(stateDir(), `service-${serviceId}.json`);
}

/** The pre-3.0 single file. Read once for migration, never written again. */
export function legacyStatePath() {
  return path.join(basePath(), 'state.json');
}

/**
 * True when the pid is still running. EPERM means it exists but is not ours.
 *
 * The locks no longer use this — exclusion is the operating system's job now, and
 * a lock is held exactly while its owner's process is alive, with nothing here
 * having to work that out. It stays because the Windows watchdog's own lock file
 * records a pid, and doctor reports on it.
 */
export function processIsAlive(pid) {
  if (!Number.isInteger(pid) || pid <= 0) return false;
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    return err.code === 'EPERM';
  }
}

function ensureDir(dir) {
  fs.mkdirSync(dir, { recursive: true });
}

/**
 * Take an exclusive lock. Returns a handle with release(), or throws with a
 * message naming the wait that expired.
 *
 * The exclusion is a SQLite write transaction on `<file>.sqlite`, which the
 * kernel releases the moment the holding process ends. See mutex.mjs for why the
 * obvious lock file cannot be made correct.
 */
export function acquireFileLock(file, { waitMs = LOCK_WAIT_MS, purpose = '' } = {}) {
  const taken = acquireMutex(mutexPath(file), { waitMs });
  if (taken.ok) return { file, release: taken.release };
  if (taken.busy) {
    throw new Error(`${file} is held by another process and did not free within ${waitMs} ms`);
  }
  throw new Error(`cannot take ${file}: ${taken.error}${purpose ? ` (for ${purpose})` : ''}`);
}

function readJson(file) {
  let raw;
  try {
    raw = fs.readFileSync(file, 'utf8');
  } catch (err) {
    if (err.code === 'ENOENT') return { ok: true, value: {}, missing: true, error: null };
    return { ok: false, value: {}, missing: false, error: `${file} could not be read: ${err.message}` };
  }
  if (raw.trim().length === 0) return { ok: false, value: {}, missing: false, error: `${file} is empty` };
  try {
    const parsed = JSON.parse(raw);
    if (!isPlainObject(parsed)) return { ok: false, value: {}, missing: false, error: `${file} is not a JSON object` };
    return { ok: true, value: parsed, missing: false, error: null };
  } catch (err) {
    return { ok: false, value: {}, missing: false, error: `${file} does not parse: ${err.message}` };
  }
}

/**
 * Read one store without taking its lock. Safe because every write lands through
 * an atomic rename, so a reader either sees the whole old file or the whole new
 * one. Used by everything read-only, so `status` never queues behind a write.
 */
export function readJsonStore(file) {
  return readJson(file);
}

/**
 * Atomic write with a temp name unique to this process and call.
 *
 * The shared "<file>.tmp" name the first version used meant two concurrent
 * writers wrote into the same temp file and renamed each other's half-written
 * bytes into place. The pid and a random suffix make that impossible, and the
 * directory fsync makes the rename durable rather than merely ordered.
 */
export function writeJsonAtomic(file, value) {
  ensureDir(path.dirname(file));
  const tmp = `${file}.tmp-${process.pid}-${crypto.randomBytes(6).toString('hex')}`;
  try {
    const fd = fs.openSync(tmp, 'wx');
    try {
      fs.writeFileSync(fd, `${JSON.stringify(value, null, 2)}\n`, 'utf8');
      fs.fsyncSync(fd);
    } finally {
      fs.closeSync(fd);
    }
    fs.renameSync(tmp, file);
    try {
      const dirFd = fs.openSync(path.dirname(file), 'r');
      try {
        fs.fsyncSync(dirFd);
      } finally {
        fs.closeSync(dirFd);
      }
    } catch {
      // Directory fsync is not permitted on every platform (Windows in
      // particular). The rename itself is still atomic.
    }
    return { ok: true, error: null };
  } catch (err) {
    try {
      fs.rmSync(tmp, { force: true });
    } catch {
      /* nothing useful to do */
    }
    return { ok: false, error: `${file} could not be written: ${err.message}` };
  }
}

/**
 * Read-modify-write one file under the shared state lock.
 *
 * `mutate` is handed the current contents and returns the object to write, or
 * null to leave the file alone. Errors are returned, never swallowed: a caller
 * that has just recorded a pending version needs to know it did not land.
 */
export function withStore(file, mutate, { waitMs = LOCK_WAIT_MS, purpose = '', lockFile = null } = {}) {
  const taken = acquireMutex(mutexPath(lockFile ?? stateLockPath()), { waitMs });
  if (!taken.ok) {
    const where = lockFile ?? stateLockPath();
    return {
      ok: false,
      value: null,
      error: taken.busy
        ? `${where} is held by another process and did not free within ${waitMs} ms`
        : `cannot take ${where}: ${taken.error}${purpose ? ` (for ${purpose})` : ''}`,
    };
  }
  const lock = taken;
  try {
    const read = readJson(file);
    if (!read.ok) return { ok: false, value: null, error: read.error };
    const next = mutate(read.value);
    if (next === null || next === undefined) return { ok: true, value: read.value, error: null };
    const written = writeJsonAtomic(file, next);
    if (!written.ok) return { ok: false, value: null, error: written.error };
    return { ok: true, value: next, error: null };
  } catch (err) {
    return { ok: false, value: null, error: err?.message ?? String(err) };
  } finally {
    lock.release();
  }
}

// ---------------------------------------------------------------------------
// Migration from the single state.json
// ---------------------------------------------------------------------------

let migrationChecked = false;

/**
 * Split a pre-3.0 state.json into the new layout, once.
 *
 * The old file is left on disk rather than deleted: if a 3.x install has to be
 * rolled back to 2.x, the machine should still know when it last updated. The
 * new files win from here on, so a stale copy cannot resurrect old values.
 */
export function migrateLegacyState({ force = false } = {}) {
  if (migrationChecked && !force) return { migrated: false, reason: 'already checked' };
  migrationChecked = true;

  const legacy = readJson(legacyStatePath());
  if (legacy.missing || !legacy.ok) return { migrated: false, reason: legacy.missing ? 'nothing to migrate' : legacy.error };

  const value = legacy.value;
  const moved = [];

  // The reconstructible half.
  const cache = {};
  for (const key of ['channelCache', 'npmPrefixes', 'npmPrefix', 'npmPrefixProbedAt']) {
    if (value[key] !== undefined) cache[key] = value[key];
  }
  if (Object.keys(cache).length > 0 && !fs.existsSync(cachePath())) {
    withStore(cachePath(), (current) => ({ ...cache, ...current }), { purpose: 'migrate:cache' });
    moved.push('cache');
  }

  // Per-service corners, plus the first version's top-level keys, which belonged
  // to whichever service happened to be first.
  const services = isPlainObject(value.services) ? value.services : {};
  for (const [id, scoped] of Object.entries(services)) {
    if (!isPlainObject(scoped)) continue;
    const file = serviceStatePath(id);
    if (fs.existsSync(file)) continue;
    withStore(file, (current) => ({ ...scoped, ...current }), { purpose: `migrate:${id}` });
    moved.push(`service-${id}`);
  }

  const topLevel = {};
  if (value.lastUpdate !== undefined) topLevel.lastUpdate = value.lastUpdate;
  if (value.pendingRestart !== undefined) topLevel.pendingRestart = value.pendingRestart;
  if (value.pendingVersion !== undefined) topLevel.pendingVersion = value.pendingVersion;
  if (value.macApplyFailures !== undefined) topLevel.applyFailures = value.macApplyFailures;
  if (Object.keys(topLevel).length > 0) {
    // Kept aside rather than guessed at: only the caller knows which service was
    // first, and it applies them through serviceState() below.
    const file = path.join(stateDir(), 'legacy-first-service.json');
    if (!fs.existsSync(file)) {
      withStore(file, () => topLevel, { purpose: 'migrate:legacy-first' });
      moved.push('legacy-first-service');
    }
  }

  return { migrated: moved.length > 0, moved };
}

/** The pre-3.0 top-level keys, which belonged to whichever service was first. */
function legacyFirstServiceState() {
  const read = readJson(path.join(stateDir(), 'legacy-first-service.json'));
  return read.ok ? read.value : {};
}

// ---------------------------------------------------------------------------
// Service state
// ---------------------------------------------------------------------------

/**
 * One service's durable state.
 *
 * `isFirst` exists only for an install upgraded in place from a version that
 * kept these keys at the top level because it looked after exactly one service.
 */
export function serviceState(service, { isFirst = false, readOnly = false } = {}) {
  if (!readOnly) migrateLegacyState();
  const read = readJson(serviceStatePath(service.id));
  if (read.ok && Object.keys(read.value).length > 0) return read.value;
  if (readOnly && read.missing) {
    // Status may read the legacy representation, but must not create migrated
    // service state or wait for the state writer's mutex just to display it.
    const legacy = readJson(legacyStatePath());
    if (legacy.ok && isPlainObject(legacy.value.services?.[service.id])) return legacy.value.services[service.id];
    if (isFirst && legacy.ok) {
      const first = {};
      for (const key of ['lastUpdate', 'pendingRestart', 'pendingVersion']) {
        if (legacy.value[key] !== undefined) first[key] = legacy.value[key];
      }
      if (legacy.value.macApplyFailures !== undefined) first.applyFailures = legacy.value.macApplyFailures;
      const migrated = legacyFirstServiceState();
      return Object.keys(migrated).length > 0 ? migrated : first;
    }
  }
  if (isFirst) {
    const legacy = legacyFirstServiceState();
    if (Object.keys(legacy).length > 0) return legacy;
  }
  return read.ok ? read.value : {};
}

/**
 * Merge a patch into one service's state, computing the merge inside the lock so
 * two writers cannot drop each other. Returns whether the write landed.
 */
export function saveServiceState(service, patch) {
  migrateLegacyState();
  const result = withStore(
    serviceStatePath(service.id),
    (current) => {
      const value = typeof patch === 'function' ? patch(current) : patch;
      return { ...current, ...value };
    },
    { purpose: `state:${service.id}` },
  );
  return { ok: result.ok, state: result.value ?? {}, error: result.error };
}

/** Mandatory service state must be durable before reporting a mutation's outcome. */
export class ServiceStateError extends Error {
  constructor(message) {
    super(message);
    this.name = 'ServiceStateError';
  }
}

export function requireServiceStateSave(service, patch) {
  const result = saveServiceState(service, patch);
  if (!result.ok) throw new ServiceStateError(`cannot persist ${service.id} service state: ${result.error}`);
  return result;
}

/** Called inside the machine lock, before recovery or any disruptive work. */
export function ensureServiceStateWritable(service) {
  return requireServiceStateSave(service, {});
}

/** Every service state file on disk, keyed by service id. Read-only. */
export function allServiceState() {
  migrateLegacyState();
  const states = {};
  let names = [];
  try {
    names = fs.readdirSync(stateDir());
  } catch {
    return states;
  }
  for (const name of names) {
    const match = /^service-(.+)\.json$/.exec(name);
    if (!match) continue;
    const read = readJson(path.join(stateDir(), name));
    if (read.ok) states[match[1]] = read.value;
  }
  return states;
}

// ---------------------------------------------------------------------------
// cache.json
// ---------------------------------------------------------------------------

/** The reconstructible cache. */
export function loadCache({ readOnly = false } = {}) {
  if (!readOnly) migrateLegacyState();
  const read = readJson(cachePath());
  if (readOnly && read.missing) {
    const legacy = readJson(legacyStatePath());
    const cache = {};
    for (const key of ['channelCache', 'npmPrefixes', 'npmPrefix', 'npmPrefixProbedAt']) {
      if (legacy.ok && legacy.value[key] !== undefined) cache[key] = legacy.value[key];
    }
    return cache;
  }
  return read.ok ? read.value : {};
}

/** Merge a patch into the cache. A failed cache write is not worth failing a command over. */
export function saveCache(patch, { skipMigration = false, waitMs = LOCK_WAIT_MS } = {}) {
  if (!skipMigration) migrateLegacyState();
  const result = withStore(
    cachePath(),
    (current) => {
      const value = typeof patch === 'function' ? patch(current) : patch;
      return { ...current, ...value };
    },
    { purpose: 'cache', waitMs },
  );
  return { ok: result.ok, cache: result.value ?? {}, error: result.error };
}
