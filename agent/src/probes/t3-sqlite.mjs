// Busy detection for T3 Code: is it in the middle of something a restart would
// destroy? Counts running and pending turns and pending approvals in T3 Code's
// own state database.
//
// This feeds the gate in front of every disruptive action (update, restart,
// boot, sleep), so it is deliberately conservative. THE RULE IS FAIL CLOSED: if
// we cannot read the state database we report busy. Restarting mid-turn throws
// away the user's work; deferring only costs a delay, and the next cycle tries
// again.
//
// Two things the first version got wrong, both in the same direction:
//
//   A turn still marked "running" was aged out by its requested_at, so after six
//   hours it stopped blocking. But requested_at is when the turn STARTED, not
//   when it was last alive. A long agent run is exactly the work this gate
//   exists to protect, and it was the first thing to lose the protection. A
//   running row now blocks regardless of age. Only evidence from OUTSIDE the
//   database retires one, and there are exactly two kinds:
//
//     the service process is not running — nothing can be executing the turn;
//     the service process started AFTER the row's requested_at — a turn cannot
//       survive the restart of the server that was running it.
//
//   Pending rows and approvals still age out on staleHours, because those are
//   queued work with nothing executing.
//
//   A database that was not there counted as idle. That is right for a machine
//   where the product has never run and indistinguishable from a wrong path,
//   which is the far more likely mistake. Absent now blocks and names the path,
//   unless the config says allowMissing.

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createRequire } from 'node:module';
import { userHome } from '../config.mjs';

const require = createRequire(import.meta.url);

// node:sqlite is only available on recent Node. Load it lazily so an old Node
// produces a clean "busy state unknown" instead of an import-time crash.
function loadSqlite() {
  try {
    return require('node:sqlite');
  } catch {
    return null;
  }
}

const ACTIVE_TURN_STATES = ['running', 'pending'];
const MAX_REPORTED_THREADS = 20;
const DEFAULT_STALE_HOURS = 6;

/** T3's home, honouring the T3CODE_HOME override the server itself respects. */
export function t3Home(probe = {}) {
  if (typeof probe.home === 'string' && probe.home.length > 0) return probe.home;
  return process.env.T3CODE_HOME || path.join(userHome(), '.t3');
}

export function t3StateDbPath(probe = {}) {
  return path.join(t3Home(probe), 'userdata', 'state.sqlite');
}

function plural(n, word) {
  return `${n} ${word}${n === 1 ? '' : 's'}`;
}

function buildReason({ runningTurns, pendingTurns, pendingApprovals, staleTurns, staleApprovals, abandonedTurns }) {
  const parts = [];
  if (runningTurns > 0) parts.push(`${plural(runningTurns, 'turn')} running`);
  if (pendingTurns > 0) parts.push(`${plural(pendingTurns, 'turn')} pending`);
  if (pendingApprovals > 0) parts.push(`${plural(pendingApprovals, 'approval')} waiting`);
  if (parts.length > 0) return parts.join(', ');

  const ignored = staleTurns + staleApprovals + abandonedTurns;
  if (abandonedTurns > 0) {
    return `idle (${plural(abandonedTurns, 'running row')} left behind by a service that is not running)`;
  }
  if (ignored > 0) return `idle (${plural(ignored, 'stale row')} ignored)`;
  return 'idle';
}

const EMPTY_COUNTS = {
  runningTurns: 0,
  pendingTurns: 0,
  pendingApprovals: 0,
  staleTurns: 0,
  staleApprovals: 0,
  abandonedTurns: 0,
};

/**
 * The shared reply shape. `monitored` is true whenever a probe was configured at
 * all, even when it could not be read: "you asked me to watch this and I could
 * not" is a different statement from "nobody asked me to watch it".
 */
function answer(state, reason, { evidence = 't3-sqlite', startedAt, ...extra } = {}) {
  const blocking = state !== 'idle';
  return {
    busy: blocking,
    unknown: state !== 'idle' && state !== 'busy',
    monitored: true,
    reason,
    evidence,
    checkedAt: new Date().toISOString(),
    elapsedMs: startedAt === undefined ? 0 : Math.max(0, Date.now() - startedAt),
    error: null,
    ...EMPTY_COUNTS,
    threads: [],
    threadsTruncated: 0,
    ...extra,
  };
}

/**
 * A pending row counts as fresh (and therefore blocking) when its timestamp is
 * inside the staleness window. A missing or unparseable timestamp is treated as
 * fresh, again because the safe direction is to block.
 */
function isFresh(timestamp, now, windowMs) {
  if (!timestamp) return true;
  const parsed = Date.parse(timestamp);
  if (Number.isNaN(parsed)) return true;
  return now - parsed < windowMs;
}

function fetchTitles(db, threadIds) {
  const titles = new Map();
  if (threadIds.length === 0) return titles;
  try {
    const placeholders = threadIds.map(() => '?').join(',');
    const rows = db
      .prepare(`SELECT thread_id, title FROM projection_threads WHERE thread_id IN (${placeholders})`)
      .all(...threadIds);
    for (const row of rows) titles.set(row.thread_id, row.title ?? null);
  } catch {
    // The titles are cosmetic. A missing projection_threads table must not turn a
    // perfectly good busy answer into "unknown".
  }
  return titles;
}

/**
 * Whether outside evidence retires a running row. Returns null when there is no
 * such evidence, which is the case that keeps the row blocking.
 */
function abandonedReason(requestedAt, { serviceIsRunning, serviceStartedAt }) {
  if (serviceIsRunning === false) return 'the service is not running';
  if (serviceIsRunning === true && serviceStartedAt) {
    const started = Date.parse(serviceStartedAt);
    const requested = Date.parse(requestedAt ?? '');
    // A missing or unreadable requested_at gives no ordering to compare, so it
    // keeps its protection.
    if (!Number.isNaN(started) && !Number.isNaN(requested) && started > requested) {
      return 'the service restarted after this turn began';
    }
  }
  return null;
}

function readSnapshot(db, windowMs, { serviceIsRunning, serviceStartedAt, startedAt }) {
  const now = Date.now();

  const turnRows = db
    .prepare(
      `SELECT thread_id, turn_id, state, requested_at
         FROM projection_turns
        WHERE state IN (${ACTIVE_TURN_STATES.map(() => '?').join(',')})`,
    )
    .all(...ACTIVE_TURN_STATES);

  const approvalRows = db
    .prepare(
      `SELECT request_id, thread_id, turn_id, created_at
         FROM projection_pending_approvals
        WHERE status = 'pending'`,
    )
    .all();

  const threadIds = [
    ...new Set([...turnRows.map((r) => r.thread_id), ...approvalRows.map((r) => r.thread_id)].filter(Boolean)),
  ];
  const titles = fetchTitles(db, threadIds);

  const counts = { ...EMPTY_COUNTS };
  const threads = [];

  for (const row of turnRows) {
    const running = row.state === 'running';
    let blocking;
    let disposition;

    if (running) {
      // The only thing that retires a running row is evidence from outside the
      // database that nothing could be running it. Age is not that evidence: a
      // turn that has been going for seven hours is a long agent run, and it is
      // precisely the work this gate exists to protect.
      const abandoned = abandonedReason(row.requested_at, { serviceIsRunning, serviceStartedAt });
      if (abandoned) {
        blocking = false;
        disposition = 'abandoned';
        counts.abandonedTurns += 1;
      } else {
        blocking = true;
        disposition = 'running';
        counts.runningTurns += 1;
      }
    } else if (isFresh(row.requested_at, now, windowMs)) {
      blocking = true;
      disposition = 'pending';
      counts.pendingTurns += 1;
    } else {
      blocking = false;
      disposition = 'stale';
      counts.staleTurns += 1;
    }

    threads.push({
      threadId: row.thread_id ?? null,
      turnId: row.turn_id ?? null,
      title: titles.get(row.thread_id) ?? null,
      state: row.state,
      at: row.requested_at ?? null,
      blocking,
      disposition,
      stale: disposition === 'stale',
    });
  }

  for (const row of approvalRows) {
    const fresh = isFresh(row.created_at, now, windowMs);
    if (fresh) counts.pendingApprovals += 1;
    else counts.staleApprovals += 1;
    threads.push({
      threadId: row.thread_id ?? null,
      turnId: row.turn_id ?? null,
      title: titles.get(row.thread_id) ?? null,
      state: 'awaiting-approval',
      at: row.created_at ?? null,
      blocking: fresh,
      disposition: fresh ? 'pending-approval' : 'stale',
      stale: !fresh,
    });
  }

  // Blocking rows first, newest first, and capped: a database with months of
  // abandoned pending rows must not produce a megabyte of status JSON.
  threads.sort((a, b) => {
    if (a.blocking !== b.blocking) return a.blocking ? -1 : 1;
    return String(b.at ?? '').localeCompare(String(a.at ?? ''));
  });
  const shown = threads.slice(0, MAX_REPORTED_THREADS);

  const busy = counts.runningTurns + counts.pendingTurns + counts.pendingApprovals > 0;
  return answer(busy ? 'busy' : 'idle', buildReason(counts), {
    startedAt,
    ...counts,
    threads: shown,
    threadsTruncated: threads.length - shown.length,
  });
}

function openAndRead(sqlite, file, windowMs, options, context) {
  const db = new sqlite.DatabaseSync(file, options);
  try {
    return readSnapshot(db, windowMs, context);
  } finally {
    try {
      db.close();
    } catch {
      /* already closed */
    }
  }
}

/**
 * Fallback for the case the contract warns about: a read-only open of a WAL
 * database fails when the -shm file cannot be mapped (different user, locked
 * file, read-only mount). Copy the database and its WAL siblings somewhere we
 * fully own and read the copy. The copy is opened read-only first; if SQLite
 * refuses because it needs to recover the WAL, we open the throwaway copy
 * writable, which is harmless because it is a copy.
 */
function readViaCopy(sqlite, file, windowMs, context) {
  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'legionctl-busy-'));
  try {
    const target = path.join(tmpDir, 'state.sqlite');
    fs.copyFileSync(file, target);
    for (const suffix of ['-wal', '-shm']) {
      const sibling = `${file}${suffix}`;
      if (fs.existsSync(sibling)) fs.copyFileSync(sibling, `${target}${suffix}`);
    }
    try {
      return openAndRead(sqlite, target, windowMs, { readOnly: true }, context);
    } catch {
      return openAndRead(sqlite, target, windowMs, { readOnly: false }, context);
    }
  } finally {
    try {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    } catch {
      /* the OS will clean the temp dir eventually */
    }
  }
}

/**
 * Read the T3 state database and say whether it is busy. Never throws.
 *
 * `liveness.serviceRunning` is the outside evidence described at the top of the
 * file. Pass it whenever it is already known; leave it null when it is not,
 * since guessing at it would defeat the point.
 */
export function checkT3Sqlite(probe = {}, { liveness = null, serviceName = 'the service', startedAt = Date.now() } = {}) {
  const hours = Number(probe.staleHours);
  const windowMs = (Number.isFinite(hours) && hours > 0 ? hours : DEFAULT_STALE_HOURS) * 60 * 60 * 1000;
  const file = t3StateDbPath(probe);
  const serviceIsRunning = typeof liveness?.serviceRunning === 'boolean' ? liveness.serviceRunning : null;
  const serviceStartedAt = typeof liveness?.startedAt === 'string' ? liveness.startedAt : null;

  let exists = false;
  try {
    exists = fs.existsSync(file);
  } catch (err) {
    return answer('unknown', 'busy state unknown', { evidence: 'probe-error', error: `cannot stat ${file}: ${err.message}`, path: file, startedAt });
  }

  if (!exists) {
    if (probe.allowMissing === true) {
      return answer('idle', 'idle (no state database, and the config says that is expected)', { path: file, startedAt });
    }
    return answer('unknown', 'busy state unknown', {
      evidence: 'probe-error',
      path: file,
      startedAt,
      error:
        `there is no state database at ${file}, so there is no way to tell whether ${serviceName} is in the middle of something. ` +
        'A machine where the product has never run looks exactly like a wrong path from here. ' +
        'Set busy.home to the right directory, or busy.allowMissing to true if this machine really has never run it.',
    });
  }

  const sqlite = loadSqlite();
  if (!sqlite || typeof sqlite.DatabaseSync !== 'function') {
    return answer('unknown', 'busy state unknown', {
      evidence: 'probe-error',
      path: file,
      startedAt,
      error: 'node:sqlite is not available in this Node build',
    });
  }

  const context = { serviceIsRunning, serviceStartedAt, startedAt };
  let firstError;
  try {
    return openAndRead(sqlite, file, windowMs, { readOnly: true }, context);
  } catch (err) {
    firstError = err;
  }

  try {
    return readViaCopy(sqlite, file, windowMs, context);
  } catch (err) {
    return answer('unknown', 'busy state unknown', {
      evidence: 'probe-error',
      path: file,
      startedAt,
      error: `direct read failed (${firstError.message}); copy read failed (${err.message})`,
    });
  }
}
