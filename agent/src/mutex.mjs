// A mutex the operating system releases when the process holding it dies.
//
// WHY THIS IS NOT A LOCK FILE.
//
// The obvious design is a file created with O_EXCL, holding the owner's pid, and
// reclaimed by whoever finds the owner dead. It cannot be made correct. Reclaim
// is check-then-unlink BY PATH, and unlink cannot be conditioned on the identity
// of the file it removes. So: three processes find the same dead owner; the
// first unlinks it and creates its own lock and starts work; the second's unlink
// then removes THAT live lock; the third creates a third lock and starts work
// too. Two holders, both convinced they are alone. Checking a nonce on release
// does not help, because the damage is done by an unlink inside acquire. A
// deterministic reproduction of exactly this — eight writers, fifteen increments
// each — lands on 117 to 119 of 120.
//
// Renaming instead of unlinking leaves a window where the live holder's file is
// absent. A lock protecting the reclaim would need the same primitive it is
// trying to build. The only thing that actually works is a lock the KERNEL owns
// and releases on process death: fcntl on POSIX, LockFileEx on Windows. Node
// exposes one, through SQLite's transaction locking, and that is what this is.
//
// The mechanism: one connection per acquisition, `BEGIN IMMEDIATE` held open for
// the life of the lock. SQLite takes a write lock on the database file; a second
// `BEGIN IMMEDIATE` anywhere on the machine gets SQLITE_BUSY. When the holder
// exits — cleanly, killed, or power-cut — the kernel drops the file lock and the
// next waiter proceeds within a millisecond. Nothing has to notice a dead pid,
// because nothing depends on noticing.
//
// THREE RULES, each learned from a measured failure:
//
//   1. NOTHING BUT SQLITE MAY OPEN THE .sqlite FILE. A plain fs.readFileSync of
//      the database inside the holding process DROPS the lock: POSIX releases
//      every fcntl lock on a file when any descriptor for it is closed. stat and
//      exists are safe; read and open are not.
//   2. THE FILE MUST BE WRITABLE. A read-only database (chmod 444, or left
//      root-owned by a `sudo` run) opens read-only and silently provides NO
//      exclusion at all. Writability is checked before every acquisition and
//      failure names the ownership problem.
//   3. THE FILE IS NEVER DELETED. POSIX allows unlinking a locked file, and a
//      new file at that path is a new lock domain with none of the old waiters
//      in it. Nothing here removes a lock database, ever.
//
// The database stays empty: no table is created and no row is written. It exists
// only as something for the kernel to lock.

import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);

/** SQLITE_BUSY: somebody else holds the write lock. This is "the lock is taken". */
const SQLITE_BUSY = 5;
/** SQLITE_NOTADB: there is a file at that path and it is not a database. */
const SQLITE_NOTADB = 26;
/** SQLITE_CANTOPEN: usually a directory that cannot be written. */
const SQLITE_CANTOPEN = 14;
const SQLITE_READONLY = 8;

let sqliteModule;
let sqliteError = null;

/**
 * node:sqlite, loaded once and lazily.
 *
 * Lazily because `version` and `help` have to work on a Node that does not have
 * it: a machine whose agent cannot even say what version it is cannot be
 * diagnosed remotely. Mutating commands refuse with a message naming the Node
 * version they need, which is a far better failure than a crash at import.
 */
function sqlite() {
  if (sqliteModule || sqliteError) return sqliteModule;
  try {
    const loaded = require('node:sqlite');
    if (typeof loaded?.DatabaseSync !== 'function') throw new Error('node:sqlite has no DatabaseSync');
    sqliteModule = loaded;
  } catch (err) {
    sqliteError = err;
  }
  return sqliteModule;
}

/** Whether this Node can provide a real mutex at all. */
export function mutexAvailable() {
  return Boolean(sqlite());
}

export function mutexUnavailableReason() {
  if (mutexAvailable()) return null;
  return (
    `this Node (${process.versions.node}) does not provide node:sqlite, which the agent uses for its ` +
    'cross-process locks. Node 24 or newer is required for anything that changes this machine.'
  );
}

/** The lock database that belongs to a lock path. */
export function mutexPath(lockFile) {
  return `${lockFile}.sqlite`;
}

function describeErrcode(err, file) {
  const code = err?.errcode ?? err?.errno;
  if (code === SQLITE_NOTADB) {
    return `${file} exists but is not a lock database; remove it while no operation is running`;
  }
  if (code === SQLITE_CANTOPEN) {
    return `${path.dirname(file)} cannot be written, so the lock database cannot be created`;
  }
  if (code === SQLITE_READONLY) {
    return `${file} is read-only, so it cannot provide exclusion; fix its ownership`;
  }
  return null;
}

/**
 * Take the mutex for one lock path.
 *
 * Returns one of:
 *   { ok: true, file, release() }               the lock is held
 *   { ok: false, busy: true, error }             somebody else holds it
 *   { ok: false, busy: false, error, hint }      it could not be used at all
 *
 * Never throws. `waitMs` is a bounded wait implemented by SQLite's own
 * busy_timeout, which retries with backoff inside the C library rather than
 * spinning here; zero returns immediately.
 */
export function acquireMutex(dbFile, { waitMs = 0 } = {}) {
  const unavailable = mutexUnavailableReason();
  if (unavailable) return { ok: false, busy: false, error: unavailable, hint: 'upgrade Node on this machine' };

  const directory = path.dirname(dbFile);
  try {
    fs.mkdirSync(directory, { recursive: true });
  } catch (err) {
    return {
      ok: false,
      busy: false,
      error: `${directory} could not be created: ${err.message}`,
      hint: 'the agent keeps its locks here; fix the ownership or permissions of that directory',
    };
  }

  try {
    fs.accessSync(directory, fs.constants.W_OK);
  } catch {
    return {
      ok: false,
      busy: false,
      error: `${directory} is not writable by this user`,
      hint: 'the lock database lives here; fix the ownership of that directory',
    };
  }

  // A read-only database opens without complaint and provides no exclusion at
  // all, which is the worst possible failure: two operations would both believe
  // they hold the lock. Refusing up front is the only safe answer. This is a
  // stat-family call, not an open, so it cannot disturb a lock somebody holds.
  if (fs.existsSync(dbFile)) {
    try {
      fs.accessSync(dbFile, fs.constants.W_OK);
    } catch {
      return {
        ok: false,
        busy: false,
        error: `${dbFile} is not writable by this user, so it cannot provide exclusion`,
        hint: 'it was probably created by a run under sudo; give it back to the account that owns the install',
      };
    }
  }

  let db;
  try {
    const { DatabaseSync } = sqlite();
    db = new DatabaseSync(dbFile);
  } catch (err) {
    return {
      ok: false,
      busy: false,
      error: `${dbFile} could not be opened: ${err.message}`,
      hint: describeErrcode(err, dbFile),
    };
  }

  try {
    // The bounded wait. SQLite sleeps and retries internally until this expires.
    db.exec(`PRAGMA busy_timeout = ${Math.max(0, Math.floor(waitMs))}`);
    // IMMEDIATE rather than DEFERRED: a deferred transaction takes no lock until
    // its first write, and this transaction never writes anything.
    db.exec('BEGIN IMMEDIATE');
  } catch (err) {
    try {
      db.close();
    } catch {
      /* the open failed; there is nothing to unwind */
    }
    const code = err?.errcode ?? err?.errno;
    if (code === SQLITE_BUSY) {
      return { ok: false, busy: true, error: `another process holds ${dbFile}`, hint: null };
    }
    return {
      ok: false,
      busy: false,
      error: `${dbFile} could not be locked: ${err.message}`,
      hint: describeErrcode(err, dbFile),
    };
  }

  let released = false;
  return {
    ok: true,
    busy: false,
    error: null,
    file: dbFile,
    release() {
      if (released) return;
      released = true;
      try {
        db.exec('ROLLBACK');
      } catch {
        /* the transaction may already be gone; the close below is what matters */
      }
      try {
        db.close();
      } catch {
        /* closing twice is not an error worth reporting */
      }
    },
  };
}

/**
 * Whether a lock is currently held, without taking it.
 *
 * An absent database is `free` and is NOT created: read-only commands ask this
 * question, and a status poll has no business materialising a lock database on a
 * machine that has never run an operation.
 */
export function probeMutex(dbFile) {
  if (!mutexAvailable()) return { state: 'unknown', error: mutexUnavailableReason() };
  if (!fs.existsSync(dbFile)) return { state: 'free', error: null };

  const attempt = acquireMutex(dbFile, { waitMs: 0 });
  if (attempt.ok) {
    attempt.release();
    return { state: 'free', error: null };
  }
  if (attempt.busy) return { state: 'held', error: null };
  return { state: 'unknown', error: attempt.error, hint: attempt.hint ?? null };
}
