// Busy detection for T3 Code: is it in the middle of something a restart would
// destroy? Counts running and pending turns, the work they left running in the
// background, and pending approvals in T3 Code's own state database.
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
//     the service process started AFTER the row began — a turn cannot survive
//       the restart of the server that was running it.
//
//   Pending rows and approvals still age out on staleHours, because those are
//   queued work with nothing executing.
//
//   A database that was not there counted as idle. That is right for a machine
//   where the product has never run and indistinguishable from a wrong path,
//   which is the far more likely mistake. Absent now blocks and names the path,
//   unless the config says allowMissing.
//
// And one thing T3 changed underneath it. T3 keeps its state in
// userdata/statev2.sqlite now, with another schema. Releases before that kept
// it in userdata/state.sqlite, and the move left the old file where it was,
// holding whatever was marked running on the day T3 stopped writing to it. The
// probe went on reading that file alone: two rows frozen as running since the
// spring blocked every update on a machine whose T3 had long moved on, and
// none of the work it was really doing was seen. So statev2 is read whenever
// it exists, and state.sqlite only when it is the only file there or has been
// written since statev2 was, as by a T3 that went back to the old schema. A
// legacy file older than the current one never decides anything.

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createRequire } from 'node:module';
import { userHome } from '../paths.mjs';

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

const MAX_REPORTED_THREADS = 20;
const DEFAULT_STALE_HOURS = 6;

/** T3's home, honouring the T3CODE_HOME override the server itself respects. */
export function t3Home(probe = {}) {
  if (typeof probe.home === 'string' && probe.home.length > 0) return probe.home;
  return process.env.T3CODE_HOME || path.join(userHome(), '.t3');
}

/** The state database T3 writes now, and the one releases before it wrote. */
export function t3StateDbPaths(probe = {}) {
  const userdata = path.join(t3Home(probe), 'userdata');
  return { current: path.join(userdata, 'statev2.sqlite'), legacy: path.join(userdata, 'state.sqlite') };
}

// Every query below reads a row in one shape: the thread, the unit of work, its
// own status, the time that decides its fate, and whether it is RUNNING (only
// outside evidence retires it) or PENDING (it ages out on staleHours).

/** The schema of T3 releases that kept their state in state.sqlite. */
function readLegacy(db) {
  const turns = db
    .prepare(
      `SELECT thread_id, turn_id, state, requested_at AS at,
              CASE state WHEN 'running' THEN 'running' ELSE 'pending' END AS kind
         FROM projection_turns
        WHERE state IN ('running', 'pending')`,
    )
    .all();
  const approvals = db
    .prepare(
      `SELECT request_id, thread_id, turn_id, created_at AS at
         FROM projection_pending_approvals
        WHERE status = 'pending'`,
    )
    .all();
  return { turns, approvals, titles: fetchTitles(db, 'projection_threads', [...turns, ...approvals]) };
}

// T3 drops a deleted thread from everything it would resume, so whatever is
// still marked live in one is debris, not work.
const LIVE_THREAD = (alias) =>
  `NOT EXISTS (SELECT 1 FROM orchestration_v2_projection_threads AS thread
                WHERE thread.thread_id = ${alias}.thread_id AND thread.deleted_at IS NOT NULL)`;

// A field of a row's payload_json. A payload that is not JSON has no fields,
// which leaves the row counted rather than failing the whole read.
const FIELD = (alias, key) =>
  `CASE WHEN json_valid(${alias}.payload_json) THEN json_extract(${alias}.payload_json, '$.${key}') END`;

/**
 * The schema T3 writes to statev2.sqlite. What a restart of T3 cancels is
 * what its own recovery cancels on startup, and these rows hold it:
 *
 *   provider turns    a turn the provider is running, or about to;
 *   native subagents  work a provider runs inside its own process, which a
 *                     restart ends with it, so a pending or waiting one is as
 *                     live as a running one. It keeps going after the turn
 *                     that started it has finished;
 *   background items  a shell, monitor or tool a turn left running in the
 *                     background, which T3 marks the turn's own item;
 *   background roster the tasks a provider thread says it still has going;
 *   runs              a run that is running, or waiting on background work
 *                     after its turn has ended. One that is being prepared
 *                     or started, or is queued for its turn, is the old
 *                     pending turn. A queue T3 holds until the user releases
 *                     it is idle, as T3 itself shows it, and a restart leaves
 *                     it held;
 *   runtime requests  an approval or a question waiting for an answer.
 *
 * Work with no start time yet is dated by its last update, which is as good
 * for the restart rule: a row last touched before the server started
 * predates it, since T3's recovery rewrites every row it cancels. A run is
 * dated by when it started, never by when it was asked for: a run queued
 * before a restart can be running after it.
 */
function readCurrent(db) {
  const turns = [
    ...db
      .prepare(
        `SELECT turn.thread_id, turn.provider_turn_id AS turn_id, turn.run_attempt_id AS attempt_id, turn.provider_thread_id,
                turn.status AS state, turn.started_at AS at,
                CASE turn.status WHEN 'running' THEN 'running' ELSE 'pending' END AS kind
           FROM orchestration_v2_projection_provider_turns AS turn
          WHERE turn.status IN ('running', 'pending') AND ${LIVE_THREAD('turn')}`,
      )
      .all(),
    ...db
      .prepare(
        `SELECT agent.thread_id, agent.subagent_id AS turn_id, ${FIELD('agent', 'nativeTaskRef.nativeId')} AS native_id,
                agent.run_id, agent.provider_thread_id, agent.status AS state,
                COALESCE(agent.started_at, agent.updated_at) AS at, 'running' AS kind
           FROM orchestration_v2_projection_subagents AS agent
          WHERE agent.origin = 'provider_native' AND agent.status IN ('pending', 'running', 'waiting')
            AND ${LIVE_THREAD('agent')}`,
      )
      .all(),
    // A subagent T3 runs as a thread of its own is not cut by the restart:
    // T3 continues that thread, and its turns count there.
    ...db
      .prepare(
        `SELECT item.thread_id, COALESCE(${FIELD('item', 'subagentId')}, item.turn_item_id) AS turn_id,
                ${FIELD('item', 'nativeItemRef.nativeId')} AS native_id, item.run_id, item.provider_thread_id, item.status AS state,
                COALESCE(${FIELD('item', 'startedAt')}, item.updated_at) AS at, 'running' AS kind
           FROM orchestration_v2_projection_turn_items AS item
          WHERE item.type IN ('command_execution', 'dynamic_tool', 'subagent') AND item.status IN ('pending', 'running', 'waiting')
            AND NOT (item.type = 'subagent' AND ${FIELD('item', 'origin')} IS 'app_owned'
                     AND (${FIELD('item', 'childThreadId')} IS NOT NULL OR ${FIELD('item', 'remoteChild')} IS NOT NULL))
            AND ${LIVE_THREAD('item')}`,
      )
      .all(),
    // A subagent's own provider thread is cleared with it, so only the
    // threads no subagent owns are read.
    ...db
      .prepare(
        `SELECT owner.thread_id, json_extract(task.value, '$.taskId') AS turn_id, json_extract(task.value, '$.taskId') AS native_id,
                json_extract(task.value, '$.parentTaskId') AS parent_id, owner.provider_thread_id,
                'background' AS state, owner.updated_at AS at, 'running' AS kind
           FROM orchestration_v2_projection_provider_threads AS owner,
                json_each(CASE WHEN json_valid(owner.payload_json) THEN owner.payload_json END, '$.pendingBackgroundTasks') AS task
          WHERE owner.owner_node_id IS NULL AND ${LIVE_THREAD('owner')}`,
      )
      .all(),
    ...db
      .prepare(
        `SELECT run.thread_id, run.run_id AS turn_id, run.run_id, ${FIELD('run', 'activeAttemptId')} AS attempt_id,
                run.provider_thread_id, 'run' AS scope, run.status AS state,
                CASE WHEN run.status IN ('running', 'waiting') THEN ${FIELD('run', 'startedAt')} ELSE run.requested_at END AS at,
                CASE WHEN run.status IN ('running', 'waiting') THEN 'running' ELSE 'pending' END AS kind
           FROM orchestration_v2_projection_runs AS run
          WHERE (run.status IN ('preparing', 'starting', 'running', 'waiting')
                 OR (run.status = 'queued' AND ${FIELD('run', 'queueHeld')} IS NOT 1))
            AND ${LIVE_THREAD('run')}`,
      )
      .all(),
  ];
  const approvals = db
    .prepare(
      `SELECT request.runtime_request_id AS request_id, request.thread_id, request.provider_turn_id AS turn_id,
              request.created_at AS at
         FROM orchestration_v2_projection_runtime_requests AS request
        WHERE request.status = 'pending' AND ${LIVE_THREAD('request')}`,
    )
    .all();
  const work = distinctWork(turns);
  return { turns: work, approvals, titles: fetchTitles(db, 'orchestration_v2_projection_threads', [...work, ...approvals]) };
}

/**
 * One row for each piece of running work. T3 records some of it in more than
 * one place: a native subagent in its own table and as an item of its turn, a
 * background task as an item and in its thread's roster, where a task a
 * subagent started names it as its parent. A run counts on its own only when
 * nothing running in it, or on its provider thread, does already, as when it
 * waits on output after its turn has ended. Pending rows are never the same
 * work twice.
 */
const HOLDERS = ['run_id', 'attempt_id', 'provider_thread_id'];

function distinctWork(rows) {
  const seen = new Set();
  const holding = new Set();
  return rows.filter((row) => {
    if (row.kind !== 'running') return true;
    const ids = [row.turn_id, row.native_id].filter(Boolean).map((id) => `${row.thread_id}\0${id}`);
    const holds = HOLDERS.filter((column) => row[column]).map((column) => `${column}\0${row[column]}`);
    if (row.scope === 'run') return !holds.some((id) => holding.has(id));
    if ([...ids, row.parent_id && `${row.thread_id}\0${row.parent_id}`].some((id) => id && seen.has(id))) return false;
    for (const id of ids) seen.add(id);
    for (const id of holds) holding.add(id);
    return true;
  });
}

const RETIRED = { stopped: 'the service is not running', restarted: 'the service restarted after this turn began' };

function plural(n, word) {
  return `${n} ${word}${n === 1 ? '' : 's'}`;
}

function buildReason({ runningTurns, pendingTurns, pendingApprovals, staleTurns, staleApprovals, abandonedTurns }, abandonedBy) {
  const parts = [];
  if (runningTurns > 0) parts.push(`${plural(runningTurns, 'turn')} running`);
  if (pendingTurns > 0) parts.push(`${plural(pendingTurns, 'turn')} pending`);
  if (pendingApprovals > 0) parts.push(`${plural(pendingApprovals, 'approval')} waiting`);
  if (parts.length > 0) return parts.join(', ');

  const ignored = staleTurns + staleApprovals + abandonedTurns;
  if (abandonedTurns > 0) {
    // Every row is retired on the same evidence, so one sentence names it.
    const left = abandonedBy === RETIRED.restarted ? 'that has restarted since' : 'that is not running';
    return `idle (${plural(abandonedTurns, 'running row')} left behind by a service ${left})`;
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

function fetchTitles(db, table, rows) {
  const titles = new Map();
  const threadIds = [...new Set(rows.map((row) => row.thread_id).filter(Boolean))];
  if (threadIds.length === 0) return titles;
  try {
    const placeholders = threadIds.map(() => '?').join(',');
    const found = db.prepare(`SELECT thread_id, title FROM ${table} WHERE thread_id IN (${placeholders})`).all(...threadIds);
    for (const row of found) titles.set(row.thread_id, row.title ?? null);
  } catch {
    // The titles are cosmetic. A missing threads table must not turn a
    // perfectly good busy answer into "unknown".
  }
  return titles;
}

/**
 * Whether outside evidence retires a running row. Returns null when there is no
 * such evidence, which is the case that keeps the row blocking.
 */
function abandonedReason(began, { serviceIsRunning, serviceStartedAt }) {
  if (serviceIsRunning === false) return RETIRED.stopped;
  if (serviceIsRunning === true && serviceStartedAt) {
    const started = Date.parse(serviceStartedAt);
    const requested = Date.parse(began ?? '');
    // A missing or unreadable start gives no ordering to compare, so the row
    // keeps its protection.
    if (!Number.isNaN(started) && !Number.isNaN(requested) && started > requested) return RETIRED.restarted;
  }
  return null;
}

function classify({ turns, approvals, titles }, windowMs, { serviceIsRunning, serviceStartedAt, startedAt }) {
  const now = Date.now();
  const counts = { ...EMPTY_COUNTS };
  const threads = [];
  let abandonedBy = null;

  for (const row of turns) {
    let blocking;
    let disposition;

    if (row.kind === 'running') {
      // The only thing that retires a running row is evidence from outside the
      // database that nothing could be running it. Age is not that evidence: a
      // turn that has been going for seven hours is a long agent run, and it is
      // precisely the work this gate exists to protect.
      const abandoned = abandonedReason(row.at, { serviceIsRunning, serviceStartedAt });
      if (abandoned) {
        blocking = false;
        disposition = 'abandoned';
        counts.abandonedTurns += 1;
        abandonedBy = abandoned;
      } else {
        blocking = true;
        disposition = 'running';
        counts.runningTurns += 1;
      }
    } else if (isFresh(row.at, now, windowMs)) {
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
      at: row.at ?? null,
      blocking,
      disposition,
      stale: disposition === 'stale',
    });
  }

  for (const row of approvals) {
    const fresh = isFresh(row.at, now, windowMs);
    if (fresh) counts.pendingApprovals += 1;
    else counts.staleApprovals += 1;
    threads.push({
      threadId: row.thread_id ?? null,
      turnId: row.turn_id ?? null,
      title: titles.get(row.thread_id) ?? null,
      state: 'awaiting-approval',
      at: row.at ?? null,
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
  return answer(busy ? 'busy' : 'idle', buildReason(counts, abandonedBy), {
    startedAt,
    ...counts,
    threads: shown,
    threadsTruncated: threads.length - shown.length,
  });
}

function openAndRead(sqlite, file, options, read) {
  const db = new sqlite.DatabaseSync(file, options);
  try {
    return read(db);
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
function readViaCopy(sqlite, file, read) {
  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'legionctl-busy-'));
  try {
    const target = path.join(tmpDir, path.basename(file));
    fs.copyFileSync(file, target);
    for (const suffix of ['-wal', '-shm']) {
      const sibling = `${file}${suffix}`;
      if (fs.existsSync(sibling)) fs.copyFileSync(sibling, `${target}${suffix}`);
    }
    try {
      return openAndRead(sqlite, target, { readOnly: true }, read);
    } catch {
      return openAndRead(sqlite, target, { readOnly: false }, read);
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
 * When a database was last written. SQLite in WAL mode writes to the -wal file
 * and folds it into the main file only at a checkpoint, so the later of the two
 * is the answer. The -shm file is left out: every reader touches it, this probe
 * included.
 */
function lastWritten(file) {
  let latest = fs.statSync(file).mtimeMs;
  try {
    latest = Math.max(latest, fs.statSync(`${file}-wal`).mtimeMs);
  } catch {
    /* no write-ahead log */
  }
  return latest;
}

/** The databases that decide, current schema first. Empty when there are none. */
function databasesToRead(files) {
  const current = fs.existsSync(files.current) ? { file: files.current, read: readCurrent } : null;
  const legacy = fs.existsSync(files.legacy) ? { file: files.legacy, read: readLegacy } : null;
  if (current && legacy && lastWritten(files.legacy) < lastWritten(files.current)) return [current];
  return [current, legacy].filter(Boolean);
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
  const files = t3StateDbPaths(probe);
  const serviceIsRunning = typeof liveness?.serviceRunning === 'boolean' ? liveness.serviceRunning : null;
  const serviceStartedAt = typeof liveness?.startedAt === 'string' ? liveness.startedAt : null;

  let databases;
  try {
    databases = databasesToRead(files);
  } catch (err) {
    return answer('unknown', 'busy state unknown', { evidence: 'probe-error', error: `cannot stat ${files.current}: ${err.message}`, path: files.current, startedAt });
  }

  if (databases.length === 0) {
    if (probe.allowMissing === true) {
      return answer('idle', 'idle (no state database, and the config says that is expected)', { path: files.current, startedAt });
    }
    return answer('unknown', 'busy state unknown', {
      evidence: 'probe-error',
      path: files.current,
      startedAt,
      error:
        `there is no state database at ${files.current} or ${files.legacy}, so there is no way to tell whether ${serviceName} is in the middle of something. ` +
        'A machine where the product has never run looks exactly like a wrong path from here. ' +
        'Set busy.home to the right directory, or busy.allowMissing to true if this machine really has never run it.',
    });
  }

  const file = databases[0].file;
  const sqlite = loadSqlite();
  if (!sqlite || typeof sqlite.DatabaseSync !== 'function') {
    return answer('unknown', 'busy state unknown', {
      evidence: 'probe-error',
      path: file,
      startedAt,
      error: 'node:sqlite is not available in this Node build',
    });
  }

  const rows = { turns: [], approvals: [], titles: new Map() };
  for (const database of databases) {
    let read;
    let firstError;
    try {
      read = openAndRead(sqlite, database.file, { readOnly: true }, database.read);
    } catch (err) {
      firstError = err;
    }
    if (!read) {
      try {
        read = readViaCopy(sqlite, database.file, database.read);
      } catch (err) {
        return answer('unknown', 'busy state unknown', {
          evidence: 'probe-error',
          path: database.file,
          startedAt,
          error: `direct read failed (${firstError.message}); copy read failed (${err.message})`,
        });
      }
    }
    rows.turns.push(...read.turns);
    rows.approvals.push(...read.approvals);
    for (const [thread, title] of read.titles) rows.titles.set(thread, title);
  }
  return classify(rows, windowMs, { serviceIsRunning, serviceStartedAt, startedAt });
}
