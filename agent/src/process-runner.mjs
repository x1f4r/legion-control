// Running a command that changes the machine, and knowing when it has stopped.
//
// Until 3.1.0 every mutation went through spawnSync, and its timeout was wrong
// in both directions. It signalled the command and nothing else, so whatever the
// command had started (the installer an update script ran, the file a
// configured action wrote a second later) went on after the reply said it "was
// stopped", after the operation finished, and after the machine lock had passed
// to the next operation. And it waited for the command's output pipes rather
// than for the command, so a start command whose daemon kept stdout open sat out
// the whole timeout and then came back as a success that had also timed out.
//
// Three rules replace that.
//
//   A command is done when the process the agent started exits. Whatever it
//   leaves behind after exiting by itself was left there on purpose, a start
//   command's daemon or the app an action opens, and is never touched. Its
//   pipes are not closed underneath it either, for as long as this agent runs.
//
//   A command still running at its deadline has failed, whatever it would have
//   exited with. Its whole tree is stopped, the process group it was started in
//   on POSIX (SIGTERM, then SIGKILL) and taskkill /T /F on Windows, and the
//   runner answers only once it has seen that tree gone. So nothing is finished,
//   rolled back or started over the top of a command that may still be running.
//
//   When that cannot be confirmed, the command stays in a ledger on disk, and
//   every later mutation, in this process or the next, finds it and stays off
//   the machine until it is gone or the machine has rebooted. Every command is in
//   that ledger while it runs, so an agent killed in the middle of one leaves
//   the same guard behind.
//
// Reading the machine is not this file's business. Probes keep runCommand and
// runCommandAsync, because whatever a probe leaves behind changes nothing.

import { AsyncLocalStorage } from 'node:async_hooks';
import { spawn, spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import path from 'node:path';
import { commandLabel, runCommandAsync, sleep, stateDir } from './config.mjs';
import { OPERATION_ID_PATTERN, OPERATION_KINDS, PHASES } from './contract.mjs';
import { log } from './log.mjs';
import { bootIdentity, readOperation } from './operations.mjs';
import { processIsAlive, readJsonStore, withStore } from './state.mjs';

const IS_WINDOWS = process.platform === 'win32';

/** How long a command has to stop after SIGTERM before it is killed outright. */
export const TERM_GRACE_MS = 5000;
/**
 * How long a tree that was killed has to be seen gone before the runner gives
 * up on it. A killed process is gone as soon as whoever inherited it reaps it,
 * so anything still there after this is not going to go.
 */
export const SETTLE_MS = 2000;
/** How long output still in the pipes is read once the command has exited. */
const DRAIN_MS = 200;
const POLL_MS = 50;
/**
 * How far apart two start times may be and still name the same process. Process
 * ids are reused, so a record of one is checked against when it started, and
 * the clocks on either side of that comparison are not read at the same moment.
 */
const SAME_PROCESS_SLACK_MS = 2000;

const PHASE_NAMES = new Set(Object.values(PHASES).flat());

// ---------------------------------------------------------------------------
// Which operation is running a command
// ---------------------------------------------------------------------------

/**
 * The operation whose body is running, so a command it starts is recorded as
 * that operation's, without every caller between the operation and the command
 * having to pass the id along.
 */
const operationContext = new AsyncLocalStorage();

export function withOperationContext(opId, body) {
  return operationContext.run({ opId }, body);
}

function operationOf() {
  const opId = operationContext.getStore()?.opId ?? null;
  const record = opId ? readOperation(opId) : null;
  return {
    opId,
    kind: record?.kind ?? null,
    service: record?.service ?? null,
    phase: record?.phase ?? null,
    opStartedAt: record?.startedAt ?? record?.requestedAt ?? null,
    // The boot the command runs in, never the record's. A request queued with
    // --when-idle keeps the boot it was queued in, and when a reboot comes
    // between the queueing and the cycle that runs it, an entry stamped with
    // that boot would be read as left over from before the reboot and dropped
    // the moment it was written.
    bootId: bootIdentity(),
  };
}

// ---------------------------------------------------------------------------
// The ledger
// ---------------------------------------------------------------------------

/**
 * Every command that is running, or that ran out of time and could not be seen
 * to stop. An entry is written before its command starts and removed when the
 * command has been seen to end, so whatever is left in the file is something
 * nobody saw end.
 */
export function ledgerPath() {
  return path.join(stateDir(), 'unsettled.json');
}

/** Commands this process is running right now. They are not in anyone's way but their own. */
const inFlight = new Map();
/** The processes behind them, while they run, for the exit handler at the end of this file. */
const children = new Map();
/** Commands this process could not see stop, held here as well in case the ledger would not take them. */
const held = new Map();

function changeLedger(mutate) {
  const changed = withStore(
    ledgerPath(),
    (current) => ({ commands: mutate(Array.isArray(current.commands) ? current.commands : []) }),
    { purpose: 'the command ledger' },
  );
  if (!changed.ok) log(`the command ledger could not be written: ${changed.error}`, 'runner');
  return changed;
}

/** When a process started, from ps, or null when ps cannot say. */
async function processStartedAt(pid) {
  const result = await runCommandAsync('ps', ['-o', 'lstart=', '-p', String(pid)], { timeoutMs: 5000 });
  const parsed = Date.parse(result.stdout.trim());
  return result.ok && !Number.isNaN(parsed) ? parsed : null;
}

/** A newer process now holds this id: the one the entry names started well before it. */
async function idReused(pid, entry) {
  const started = await processStartedAt(pid);
  return started !== null && started > Date.parse(entry.startedAt) + SAME_PROCESS_SLACK_MS;
}

/** The command's own process, on POSIX. A start time that does not fit is another process with the same id. */
async function posixLeaderLeft(entry) {
  try {
    process.kill(entry.pid, 0);
  } catch (error) {
    if (error.code === 'ESRCH') return false;
  }
  return !(await idReused(entry.pid, entry));
}

/**
 * Anything left in the command's process group, on POSIX. A group id cannot be
 * handed out again while a single member of the group is alive, so a group
 * with that id is the command's own, unless a process started since then owns
 * the id now.
 */
async function posixGroupLeft(entry) {
  try {
    process.kill(-entry.group, 0);
  } catch (error) {
    if (error.code === 'ESRCH') return false;
  }
  return !(await idReused(entry.group, entry));
}

/**
 * The PowerShell that finds what is left of a command's tree on Windows.
 *
 * There are no process groups to ask, so the tree is walked by parent id, and
 * because Windows reuses ids quickly every link is checked against when the
 * process started. The command itself must have started when it was spawned;
 * its children no earlier than that, and no later than it was seen to end; and
 * everything further down no earlier than the command did. A process that does
 * not fit is somebody else's that happens to have an id the tree once used.
 */
function windowsTreeScript(entry, { treeOf = true, stop = false } = {}) {
  const spawned = Date.parse(entry.startedAt);
  const ended = Date.parse(entry.endedAt ?? '');
  const parentEnded = Number.isNaN(ended) ? -1 : ended + SAME_PROCESS_SLACK_MS;
  const roots = treeOf
    ? '($_.ProcessId -eq $root -and $born -le $rootBy) -or ($_.ParentProcessId -eq $root -and ($childBy -lt 0 -or $born -le $childBy))'
    : '$_.ProcessId -eq $root -and $born -le $rootBy';
  return [
    "$ErrorActionPreference = 'Stop'",
    `$root = ${Number(entry.pid)}`,
    `$from = ${spawned - SAME_PROCESS_SLACK_MS}`,
    `$rootBy = ${spawned + SAME_PROCESS_SLACK_MS}`,
    `$childBy = ${parentEnded}`,
    'function Born($process) { if ($process.CreationDate) { ([DateTimeOffset]$process.CreationDate).ToUnixTimeMilliseconds() } else { -1 } }',
    'function Left {',
    '  $all = @(Get-CimInstance -ClassName Win32_Process | Where-Object { $_.ProcessId -ne $PID })',
    '  $found = @{}',
    `  $frontier = @($all | Where-Object { $born = Born $_; $born -ge $from -and (${roots}) })`,
    '  while ($frontier.Count -gt 0) {',
    '    foreach ($process in $frontier) { $found[[string]$process.ProcessId] = $true }',
    ...(treeOf
      ? [
        '    $parents = @($frontier | ForEach-Object { $_.ProcessId })',
        '    $frontier = @($all | Where-Object { -not $found.ContainsKey([string]$_.ProcessId) -and ($parents -contains $_.ParentProcessId) -and (Born $_) -ge $from })',
      ]
      : ['    $frontier = @()']),
    '  }',
    '  @($found.Keys)',
    '}',
    '$left = @(Left)',
    ...(stop
      ? [
        'if ($left.Count -gt 0) {',
        '  foreach ($id in $left) { Stop-Process -Id ([int]$id) -Force -ErrorAction SilentlyContinue }',
        '  Start-Sleep -Milliseconds 500',
        '  $left = @(Left)',
        '}',
      ]
      : []),
    "[pscustomobject]@{ left = ($left -join ',') } | ConvertTo-Json -Compress",
  ].join('\n');
}

/**
 * Run that script. The answer is the ids still there, or null when PowerShell
 * could not say, which every caller reads as "still running".
 */
async function windowsLeft(entry, options = {}) {
  const result = await runCommandAsync(
    'powershell.exe',
    ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-Command', windowsTreeScript(entry, options)],
    { timeoutMs: 30000 },
  );
  if (!result.ok) return null;
  const line = result.stdout.split('\n').map((text) => text.trim()).reverse().find((text) => text.startsWith('{'));
  try {
    const parsed = JSON.parse(line ?? '');
    if (typeof parsed?.left !== 'string') return null;
    return parsed.left.split(',').map((value) => value.trim()).filter((value) => /^[0-9]+$/.test(value)).map(Number);
  } catch {
    return null;
  }
}

/**
 * Whether a ledger entry may still be running.
 *
 * An entry that was still running when its agent stopped is judged by the
 * command's own process: once that has ended, the command ended, and whatever
 * it started is what it meant to leave, as for any command that exits by
 * itself. An entry that ran out of time is judged by its whole tree, every part
 * of which was meant to be stopped. Nothing outlives a reboot, and anything the
 * agent cannot read counts as running.
 */
async function stillRunning(entry, { bootId }) {
  if (entry.bootId && bootId && entry.bootId !== bootId) return false;
  if (entry.platform !== process.platform) return false;
  // Written before the command was started, and its writer never got further.
  if (!Number.isInteger(entry.pid)) return entry.agentPid !== process.pid && processIsAlive(entry.agentPid);
  const whole = entry.state === 'unconfirmed';
  if (IS_WINDOWS) {
    const left = await windowsLeft(entry, { treeOf: whole });
    return left === null || left.length > 0;
  }
  return whole ? posixGroupLeft(entry) : posixLeaderLeft(entry);
}

/** What a later mutation names as being in its way (contract section 5.4). */
function conflictOf(entry) {
  if (!OPERATION_ID_PATTERN.test(entry.opId ?? '') || !OPERATION_KINDS.includes(entry.kind)) return null;
  const started = Date.parse(entry.opStartedAt ?? entry.startedAt ?? '');
  return {
    opId: entry.opId,
    kind: entry.kind,
    service: entry.service ?? null,
    // The record's own phase, which is always one of the contract's; a command
    // that somehow ran outside one is reported as what it was doing.
    phase: PHASE_NAMES.has(entry.phase) ? entry.phase : 'running',
    startedAt: new Date(Number.isNaN(started) ? Date.now() : started).toISOString(),
  };
}

function describeEntry(entry) {
  const owner = entry.opId ? `operation ${entry.opId}${entry.kind ? ` (${entry.kind}${entry.service ? ` of ${entry.service}` : ''})` : ''}` : 'an earlier command';
  const what = entry.state === 'unconfirmed'
    ? 'ran out of time and could not be confirmed stopped'
    : 'was still running when the agent running it stopped';
  const pid = Number.isInteger(entry.pid) ? `, pid ${entry.pid}` : '';
  return `${entry.command} from ${owner} ${what}${pid}; nothing else changes this machine until it is gone`;
}

/**
 * The first command that may still be changing the machine, or null.
 *
 * Every mutation asks this after it takes the machine lock and before it does
 * anything, and so does every command this file starts. Entries that have
 * ended since are dropped on the way. `opId` narrows the question to one
 * operation's commands. The answer carries the message to give and, when the
 * command belonged to an operation, the `conflict` that names it. A ledger that
 * cannot be read is an answer too: nobody can then say that nothing is running.
 */
export async function unsettledWork({ opId = null } = {}) {
  const read = readJsonStore(ledgerPath());
  if (!read.ok) {
    return {
      entry: null,
      conflict: null,
      message: `the command ledger cannot be read (${read.error}), so nothing here can say whether an earlier command is still running`,
    };
  }
  const recorded = (Array.isArray(read.value.commands) ? read.value.commands : []).filter((entry) => entry && typeof entry === 'object');
  const known = new Set(recorded.map((entry) => entry.id));
  const candidates = [...recorded, ...[...held.values()].filter((entry) => !known.has(entry.id))]
    .filter((entry) => !inFlight.has(entry.id) && (opId === null || entry.opId === opId));
  if (candidates.length === 0) return null;

  const bootId = bootIdentity();
  const ended = [];
  let blocking = null;
  for (const entry of candidates) {
    if (await stillRunning(entry, { bootId })) blocking ??= entry;
    else ended.push(entry.id);
  }
  if (ended.length > 0) {
    for (const id of ended) held.delete(id);
    if (ended.some((id) => known.has(id))) changeLedger((commands) => commands.filter((entry) => !ended.includes(entry.id)));
  }
  return blocking ? { entry: blocking, conflict: conflictOf(blocking), message: describeEntry(blocking) } : null;
}

// ---------------------------------------------------------------------------
// Stopping a tree
// ---------------------------------------------------------------------------

function groupExists(group) {
  try {
    process.kill(-group, 0);
    return true;
  } catch (error) {
    // EPERM is a member this user may not signal, or on macOS one that has
    // exited and is waiting to be reaped. Either way it is still there.
    return error.code !== 'ESRCH';
  }
}

function signalGroup(group, signal) {
  try {
    process.kill(-group, signal);
  } catch {
    /* gone already, or not ours to signal; the wait below says which */
  }
}

async function within(ms, done) {
  const until = Date.now() + ms;
  for (;;) {
    if (done()) return true;
    if (Date.now() >= until) return false;
    await sleep(POLL_MS);
  }
}

/**
 * Stop a POSIX command's whole process group and wait until no member is left.
 * Members are reaped by whoever inherits them, which takes a moment after the
 * signal, so "gone" is the group no longer existing, not the signal arriving.
 */
async function stopGroup(group, { graceMs, settleMs }) {
  signalGroup(group, 'SIGTERM');
  if (await within(graceMs, () => !groupExists(group))) return true;
  signalGroup(group, 'SIGKILL');
  return within(settleMs, () => !groupExists(group));
}

/**
 * Stop a Windows command and everything under it. taskkill walks the tree from
 * the command while it is alive; the walk afterwards finds what it could not
 * reach, children of a command that ended just as its deadline came, stops
 * those, and is what says whether anything is left. When that walk cannot run,
 * taskkill having stopped the whole tree, and the command's exit, are the only
 * evidence there is; a command that ended before taskkill reached it leaves
 * none about its children.
 */
async function stopTree(child, entry, { settleMs }) {
  const killed = child.exitCode === null && child.signalCode === null
    ? await runCommandAsync('taskkill.exe', ['/PID', String(child.pid), '/T', '/F'], { timeoutMs: 15000 })
    : null;
  const exited = await within(settleMs, () => child.exitCode !== null || child.signalCode !== null);
  const left = await windowsLeft({ ...entry, endedAt: exited ? new Date().toISOString() : null }, { treeOf: true, stop: true });
  if (left !== null) return left.length === 0;
  return exited && killed !== null && killed.ok;
}

// ---------------------------------------------------------------------------
// The runner
// ---------------------------------------------------------------------------

function notStarted(command, error, extra = {}) {
  return { ok: false, code: null, signal: null, stdout: '', stderr: '', timedOut: false, error, command, terminated: null, ...extra };
}

/**
 * Run one command that changes the machine, and answer once it has ended.
 *
 * The result has runCommand's shape, plus `terminated`: null when the command
 * ended by itself, true when its deadline stopped it and the whole tree was seen
 * gone, false when that could not be seen. A command refused because an earlier
 * one may still be running was never started, and says so with `refused` and the
 * `conflict` naming that earlier command's operation.
 *
 * `graceMs` and `settleMs` are how long a timed-out tree gets after SIGTERM, and
 * after it is killed outright, before the runner stops waiting for it.
 */
export async function runMutation(file, args = [], options = {}) {
  const command = commandLabel(file);
  const blocking = await unsettledWork();
  if (blocking) {
    return notStarted(command, `not started: ${blocking.message}`, { refused: true, conflict: blocking.conflict });
  }

  const entry = {
    id: crypto.randomUUID(),
    pid: null,
    group: null,
    platform: process.platform,
    command,
    state: 'running',
    startedAt: new Date().toISOString(),
    endedAt: null,
    agentPid: process.pid,
    ...operationOf(),
  };
  // Written before anything starts: a command this agent could lose track of
  // is not one it may run.
  const entered = changeLedger((commands) => [...commands, entry]);
  if (!entered.ok) {
    return notStarted(command, `not started: the command ledger could not be written (${entered.error})`, { refused: true, conflict: null });
  }
  inFlight.set(entry.id, entry);
  try {
    return await supervise(file, args, options, entry);
  } finally {
    inFlight.delete(entry.id);
  }
}

/** Run a configured argv array through the mutation runner. */
export function runMutationArgv(command, options = {}) {
  return runMutation(command[0], command.slice(1), options);
}

function duration(ms) {
  return ms < 1000 ? `${ms} ms` : `${Math.round(ms / 1000)} s`;
}

function supervise(file, args, options, entry) {
  const timeoutMs = options.timeoutMs ?? 30000;
  const graceMs = options.graceMs ?? TERM_GRACE_MS;
  const settleMs = options.settleMs ?? SETTLE_MS;
  const maxBuffer = options.maxBuffer ?? 16 * 1024 * 1024;
  const { command } = entry;

  return new Promise((resolve) => {
    let child;
    // Taken again here, after the ledger write, because it is what a later
    // check holds the process's own start time against.
    entry.startedAt = new Date().toISOString();
    try {
      child = spawn(file, args, {
        cwd: options.cwd,
        env: options.env ?? process.env,
        windowsHide: true,
        // Its own process group on POSIX, which is what lets a deadline reach
        // everything the command started and lets the agent see that it is gone.
        detached: !IS_WINDOWS,
        stdio: ['ignore', 'pipe', 'pipe'],
      });
    } catch (error) {
      changeLedger((commands) => commands.filter((recorded) => recorded.id !== entry.id));
      resolve(notStarted(command, String(error?.message ?? error)));
      return;
    }

    let stdout = '';
    let stderr = '';
    let settled = false;
    let stopping = false;
    let exit = null;
    let closed = false;
    let timer = null;

    const collect = (stream, append) => {
      stream.setEncoding('utf8');
      stream.on('data', append);
      stream.on('error', () => { /* a pipe that broke is not an outcome anyone can act on */ });
    };
    collect(child.stdout, (chunk) => {
      if (stdout.length < maxBuffer) stdout += chunk.slice(0, maxBuffer - stdout.length);
    });
    collect(child.stderr, (chunk) => {
      if (stderr.length < maxBuffer) stderr += chunk.slice(0, maxBuffer - stderr.length);
    });
    child.once('close', () => { closed = true; });

    const finish = (result, { keep = false } = {}) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      children.delete(entry.id);
      if (!keep) {
        held.delete(entry.id);
        changeLedger((commands) => commands.filter((recorded) => recorded.id !== entry.id));
      }
      resolve({ stdout, stderr, command, ...result });
    };

    child.once('error', (error) => {
      // Only a spawn that failed reaches here: nothing ran, so nothing is left.
      if (child.pid === undefined) finish({ ok: false, code: null, signal: null, timedOut: false, terminated: null, error: String(error?.message ?? error) });
    });
    if (child.pid === undefined) return;

    entry.pid = child.pid;
    entry.group = IS_WINDOWS ? null : child.pid;
    children.set(entry.id, child);
    const recorded = changeLedger((commands) => commands.map((existing) => (
      existing.id === entry.id ? { ...existing, pid: entry.pid, group: entry.group, startedAt: entry.startedAt } : existing
    )));

    child.once('exit', (code, signal) => {
      exit = { code, signal };
      if (stopping || settled) return;
      clearTimeout(timer);
      // Output already in the pipes is read before answering. Pipes that stay
      // open past that belong to something the command left running on
      // purpose: they are drained for as long as this agent runs, so a daemon
      // that writes to them is not killed by a closed pipe, and they never keep
      // the agent itself from exiting.
      within(DRAIN_MS, () => closed).then(() => {
        if (!closed) {
          for (const stream of [child.stdout, child.stderr]) {
            stream.removeAllListeners('data');
            stream.on('data', () => {});
            stream.unref?.();
          }
        }
        finish({ ok: code === 0, code, signal: signal ?? null, timedOut: false, terminated: null, error: null });
      });
    });

    // Stop the whole tree, and answer only once it is seen gone or the wait
    // for that has run out. `reason` is set when the deadline is not why.
    const stopAll = async (reason = null) => {
      stopping = true;
      clearTimeout(timer);
      const gone = IS_WINDOWS
        ? await stopTree(child, entry, { settleMs })
        : await stopGroup(entry.group, { graceMs, settleMs });
      for (const stream of [child.stdin, child.stdout, child.stderr]) stream?.destroy();
      const result = {
        ok: false,
        code: null,
        signal: exit?.signal ?? null,
        timedOut: reason === null,
        terminated: gone,
        error: reason ?? (gone
          ? `${command} did not finish within ${duration(timeoutMs)} and was stopped`
          : `${command} did not finish within ${duration(timeoutMs)} and could not be confirmed stopped`),
        ...(reason ? { refused: true, conflict: null } : {}),
      };
      if (gone) {
        finish(result);
        return;
      }
      // The ledger watches it from here on. This process does not wait for it
      // to exit: a command nothing can stop must not keep the agent running too.
      child.unref();
      const unconfirmed = { ...entry, state: 'unconfirmed', endedAt: exit ? new Date().toISOString() : null };
      held.set(entry.id, unconfirmed);
      changeLedger((commands) => [...commands.filter((existing) => existing.id !== entry.id), unconfirmed]);
      log(`${command} could not be confirmed stopped (pid ${entry.pid}); further mutations wait for it`, 'runner');
      finish(result, { keep: true });
    };

    if (!recorded.ok) {
      // The ledger took the entry a moment ago and would not take its pid. A
      // command nobody could find again after a crash may not go on.
      stopAll('stopped as it started, because the command ledger would not record it');
      return;
    }
    timer = setTimeout(() => { stopAll(); }, timeoutMs);
  });
}

// A command still running when this process exits is stopped with it, as far
// as an exit handler can: it cannot wait to see the tree go, so the ledger
// keeps its entry and the next mutation checks. One that has already exited by
// itself is left alone, along with whatever it left running on purpose.
process.once('exit', () => {
  for (const child of children.values()) {
    if (child.exitCode !== null || child.signalCode !== null) continue;
    try {
      if (IS_WINDOWS) spawnSync('taskkill.exe', ['/PID', String(child.pid), '/T', '/F'], { windowsHide: true, stdio: 'ignore', timeout: 5000 });
      else process.kill(-child.pid, 'SIGKILL');
    } catch {
      /* already gone */
    }
  }
});
