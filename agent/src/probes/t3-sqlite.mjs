// Busy detection for T3 Code: is it in the middle of something a restart would
// destroy? Counts running and pending turns and pending approvals in T3 Code's
// own state database.
//
// This feeds the gate in front of every disruptive action (update, restart,
// boot, sleep), so it is deliberately conservative. THE RULE IS FAIL CLOSED: if
// we cannot read the state database we report busy:true. Restarting mid-turn
// throws away the user's work; deferring only costs a delay, and the next cycle
// tries again.

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

function buildReason({ runningTurns, pendingTurns, pendingApprovals, staleTurns, staleApprovals }) {
  const parts = [];
  if (runningTurns > 0) parts.push(`${plural(runningTurns, 'turn')} running`);
  if (pendingTurns > 0) parts.push(`${plural(pendingTurns, 'turn')} pending`);
  if (pendingApprovals > 0) parts.push(`${plural(pendingApprovals, 'approval')} waiting`);
  if (parts.length > 0) return parts.join(', ');

  const stale = staleTurns + staleApprovals;
  if (stale > 0) return `idle (${plural(stale, 'stale row')} ignored)`;
  return 'idle';
}

function idleResult(reason) {
  return {
    busy: false,
    runningTurns: 0,
    pendingTurns: 0,
    pendingApprovals: 0,
    staleTurns: 0,
    staleApprovals: 0,
    reason,
    threads: [],
    threadsTruncated: 0,
    unknown: false,
  };
}

function unknownResult(detail) {
  return {
    busy: true,
    runningTurns: 0,
    pendingTurns: 0,
    pendingApprovals: 0,
    staleTurns: 0,
    staleApprovals: 0,
    reason: 'busy state unknown',
    threads: [],
    threadsTruncated: 0,
    unknown: true,
    error: detail,
  };
}

/**
 * A row counts as fresh (and therefore blocking) when its timestamp is inside the
 * staleness window. A missing or unparseable timestamp is treated as fresh, again
 * because the safe direction is to block.
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

function readSnapshot(db, windowMs) {
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

  let runningTurns = 0;
  let pendingTurns = 0;
  let pendingApprovals = 0;
  let staleTurns = 0;
  let staleApprovals = 0;
  const threads = [];

  for (const row of turnRows) {
    const fresh = isFresh(row.requested_at, now, windowMs);
    if (fresh) {
      if (row.state === 'running') runningTurns += 1;
      else pendingTurns += 1;
    } else {
      staleTurns += 1;
    }
    threads.push({
      threadId: row.thread_id ?? null,
      turnId: row.turn_id ?? null,
      title: titles.get(row.thread_id) ?? null,
      state: row.state,
      at: row.requested_at ?? null,
      stale: !fresh,
    });
  }

  for (const row of approvalRows) {
    const fresh = isFresh(row.created_at, now, windowMs);
    if (fresh) pendingApprovals += 1;
    else staleApprovals += 1;
    threads.push({
      threadId: row.thread_id ?? null,
      turnId: row.turn_id ?? null,
      title: titles.get(row.thread_id) ?? null,
      state: 'awaiting-approval',
      at: row.created_at ?? null,
      stale: !fresh,
    });
  }

  // Blocking rows first, newest first, and capped: a database with months of
  // abandoned pending rows must not produce a megabyte of status JSON.
  threads.sort((a, b) => {
    if (a.stale !== b.stale) return a.stale ? 1 : -1;
    return String(b.at ?? '').localeCompare(String(a.at ?? ''));
  });
  const shown = threads.slice(0, MAX_REPORTED_THREADS);

  const counts = { runningTurns, pendingTurns, pendingApprovals, staleTurns, staleApprovals };
  return {
    busy: runningTurns + pendingTurns + pendingApprovals > 0,
    ...counts,
    reason: buildReason(counts),
    threads: shown,
    threadsTruncated: threads.length - shown.length,
    unknown: false,
  };
}

function openAndRead(sqlite, file, windowMs, options) {
  const db = new sqlite.DatabaseSync(file, options);
  try {
    return readSnapshot(db, windowMs);
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
function readViaCopy(sqlite, file, windowMs) {
  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'legionctl-busy-'));
  try {
    const target = path.join(tmpDir, 'state.sqlite');
    fs.copyFileSync(file, target);
    for (const suffix of ['-wal', '-shm']) {
      const sibling = `${file}${suffix}`;
      if (fs.existsSync(sibling)) fs.copyFileSync(sibling, `${target}${suffix}`);
    }
    try {
      return openAndRead(sqlite, target, windowMs, { readOnly: true });
    } catch {
      return openAndRead(sqlite, target, windowMs, { readOnly: false });
    }
  } finally {
    try {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    } catch {
      /* the OS will clean the temp dir eventually */
    }
  }
}

/** Read the T3 state database and say whether it is busy. Never throws. */
export function checkT3Sqlite(probe = {}) {
  const hours = Number(probe.staleHours);
  const windowMs = (Number.isFinite(hours) && hours > 0 ? hours : DEFAULT_STALE_HOURS) * 60 * 60 * 1000;
  const file = t3StateDbPath(probe);

  let exists = false;
  try {
    exists = fs.existsSync(file);
  } catch (err) {
    return unknownResult(`cannot stat ${file}: ${err.message}`);
  }
  // No database means T3 has never run here, which is genuinely idle.
  if (!exists) return idleResult('idle (no T3 state database)');

  const sqlite = loadSqlite();
  if (!sqlite || typeof sqlite.DatabaseSync !== 'function') {
    return unknownResult('node:sqlite is not available in this Node build');
  }

  let firstError;
  try {
    return openAndRead(sqlite, file, windowMs, { readOnly: true });
  } catch (err) {
    firstError = err;
  }

  try {
    return readViaCopy(sqlite, file, windowMs);
  } catch (err) {
    return unknownResult(`direct read failed (${firstError.message}); copy read failed (${err.message})`);
  }
}
