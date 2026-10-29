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
import { basePath, runCommand, stateDir } from './config.mjs';
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
 * How far apart two start times may be and still name the same process. Process
 * ids are reused, so a record of one is checked against when it started, and
 * the clocks on either side of that comparison are not read at the same moment.
 */
export const SAME_PROCESS_SLACK_MS = 2000;

const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

/**
 * A start time as `ps -o lstart=` prints it in the C locale, "Thu Oct 29
 * 10:15:42 2026" in local time, in milliseconds; null for anything else.
 */
export function parseProcessStart(text) {
  const match = /^\s*[A-Z][a-z]{2}\s+([A-Z][a-z]{2})\s+(\d{1,2})\s+(\d{1,2}):(\d{2}):(\d{2})\s+(\d{4})\s*$/.exec(String(text ?? ''));
  const month = match ? MONTHS.indexOf(match[1]) : -1;
  if (month < 0) return null;
  const [day, hours, minutes, seconds, year] = match.slice(2).map(Number);
  return new Date(year, month, day, hours, minutes, seconds).getTime();
}

/** When this process started, as a record keeps it beside the pid. */
export function ownStartTime() {
  return new Date(Date.now() - process.uptime() * 1000).toISOString();
}

/**
 * When a process started, in milliseconds, or null when nothing here can say.
 *
 * ps prints the time in the format of its locale, and an ssh session passes on
 * whatever locale its client had, so it is always asked in the C locale.
 * Windows has no ps; CIM keeps the creation date of every process.
 */
export function processStartTime(pid) {
  if (!Number.isInteger(pid) || pid <= 0) return null;
  if (process.platform === 'win32') {
    const script = `$process = Get-CimInstance -ClassName Win32_Process -Filter 'ProcessId = ${pid}'; ` +
      'if ($process.CreationDate) { ([DateTimeOffset]$process.CreationDate).ToUnixTimeMilliseconds() }';
    const result = runCommand('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], { timeoutMs: 15000 });
    const text = result.ok ? result.stdout.trim() : '';
    return /^[0-9]+$/.test(text) ? Number(text) : null;
  }
  const result = runCommand('/bin/ps', ['-o', 'lstart=', '-p', String(pid)], { timeoutMs: 5000, env: { ...process.env, LC_ALL: 'C' } });
  return result.ok ? parseProcessStart(result.stdout) : null;
}

/**
 * True when the pid is still running. EPERM means it exists but is not ours.
 *
 * A pid on its own only says that SOME process has it now, and process ids are
 * handed out again, on Windows soon. So whoever wrote the pid down also writes
 * down when that process started, no later than which it must have, and a
 * process that holds the id now and started well after that is somebody else's:
 * the one that was named is gone. A start time that cannot be read cannot prove
 * that, and the pid is taken for running, as it always was. The locks do not
 * depend on any of this, the operating system holds those; the records of
 * operations, the command ledger and the Windows watchdog's lock file do.
 */
export function processIsAlive(pid, { startedAt = null } = {}) {
  if (!Number.isInteger(pid) || pid <= 0) return false;
  try {
    process.kill(pid, 0);
  } catch (err) {
    if (err.code !== 'EPERM') return false;
  }
  const named = Date.parse(startedAt ?? '');
  if (Number.isNaN(named)) return true;
  const started = pid === process.pid ? Date.parse(ownStartTime()) : processStartTime(pid);
  return started === null || started <= named + SAME_PROCESS_SLACK_MS;
}

/**
 * A directory only this user can enter (T8). The base and everything the agent
 * writes under it hold configured commands, the setup and the record of every
 * operation, so they are 0700 whatever the umask was. A directory created with
 * more, by an older agent or an installer, is narrowed when the agent next
 * writes into it. Windows has no mode bits worth setting here; its installer
 * sets the ACL, and doctor says when it allows too much.
 */
export function ensurePrivateDir(dir) {
  fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
  if (process.platform === 'win32') return;
  try {
    if ((fs.statSync(dir).mode & 0o077) !== 0) fs.chmodSync(dir, 0o700);
  } catch {
    /* not ours to change; doctor's files.private says so */
  }
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

const sharingWait = new Int32Array(new SharedArrayBuffer(4));

/**
 * Run a file operation again, for up to a second, while Windows refuses it
 * with EPERM, EACCES or EBUSY. A reader, the search indexer or an antivirus
 * scanner holding the destination open for a moment makes a rename over it
 * fail until it lets go, and deleting the destination first would leave a
 * moment with no file at all. So the same atomic operation is tried again.
 * Anywhere else those errors mean what they say, the first time.
 */
export function retryWindowsSharing(operation, { platform = process.platform, waitMs = 1000 } = {}) {
  const deadline = performance.now() + waitMs;
  let pauseMs = 10;
  for (;;) {
    try {
      return operation();
    } catch (error) {
      const remaining = deadline - performance.now();
      if (platform !== 'win32' || !['EPERM', 'EACCES', 'EBUSY'].includes(error?.code) || remaining <= 0) throw error;
      Atomics.wait(sharingWait, 0, 0, Math.min(pauseMs, remaining));
      pauseMs = Math.min(pauseMs * 2, 100);
    }
  }
}

/**
 * Atomic write with a temp name unique to this process and call.
 *
 * The shared "<file>.tmp" name the first version used meant two concurrent
 * writers wrote into the same temp file and renamed each other's half-written
 * bytes into place. The pid and a random suffix make that impossible, and the
 * directory fsync makes the rename durable rather than merely ordered. On
 * Windows the rename waits out a reader holding the file (retryWindowsSharing).
 *
 * Every file the agent writes this way is private, as writePrivateAtomic's are.
 */
export function writeJsonAtomic(file, value) {
  return writePrivateAtomic(file, `${JSON.stringify(value, null, 2)}\n`);
}

/**
 * The atomic write for exact bytes that only this user may read: the temp file
 * is created 0600 in a 0700 directory, so the rename puts a private file in
 * place rather than one a later chmod has to catch up with. A rename replaces
 * the file, so a chmod somebody made by hand never lasted past the next write.
 */
export function writePrivateAtomic(file, data) {
  ensurePrivateDir(path.dirname(file));
  const tmp = `${file}.tmp-${process.pid}-${crypto.randomBytes(6).toString('hex')}`;
  try {
    const fd = fs.openSync(tmp, 'wx', 0o600);
    try {
      fs.writeFileSync(fd, data);
      fs.fsyncSync(fd);
    } finally {
      fs.closeSync(fd);
    }
    retryWindowsSharing(() => fs.renameSync(tmp, file));
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
