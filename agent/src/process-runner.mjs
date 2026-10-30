// Running commands: the runners every probe reads the machine with, and the
// one a command that changes the machine goes through, which has to know when
// that command has stopped.
//
// Reading is the simpler half. runCommand and runCommandAsync take an argv,
// never a shell string, and a reading that runs out of time says nothing,
// whatever the command exited with. What a probe leaves behind changes
// nothing, so they do not wait to see it gone.
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
// A probe never goes through runMutation: what it leaves behind changes
// nothing, and holding the machine for it would only make status slower.

import { AsyncLocalStorage } from 'node:async_hooks';
import { spawn, spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import path from 'node:path';
import { OPERATION_ID_PATTERN, OPERATION_KINDS, PHASES } from './contract.mjs';
import { log } from './log.mjs';
import { bootIdentity, readOperation } from './operations.mjs';
import { stateDir } from './paths.mjs';
import { parseProcessStart, processIsAlive, readJsonStore, SAME_PROCESS_SLACK_MS, withStore } from './state.mjs';

// ---------------------------------------------------------------------------
// Reading the machine
// ---------------------------------------------------------------------------

/**
 * How a message names a command: by its executable, never its arguments.
 * Arguments are configuration a person chose, a token or a private path among
 * them, and every failure message used to carry the first 160 characters of
 * the command line to whoever read the reply (contract section 13).
 */
export function commandLabel(file) {
  return path.basename(String(file));
}

/**
 * A PowerShell single-quoted string that holds `value` exactly, for a script
 * that names a path or a name. PowerShell ends such a string at any of U+2018
 * to U+201B as well as at the ASCII quote, so each of them is written twice
 * like it, or a folder such as C:\Users\me\O’Brien would end the string half
 * way through. The clients quote the same way (contract section 15.2).
 */
export function powerShellLiteral(value) {
  return `'${String(value).replace(/['\u2018\u2019\u201a\u201b]/g, (quote) => quote + quote)}'`;
}

/**
 * Shared process runner. Always an argv array, never a shell string, so a
 * Windows npm prefix or "C:\Program Files\nodejs" cannot break quoting.
 *
 * For reading the machine only. Its timeout stops the command and nothing the
 * command started, which a probe can live with and a mutation cannot: every
 * command that changes the machine goes through runMutation, below, instead.
 */
export function runCommand(file, args = [], options = {}) {
  const result = spawnSync(file, args, {
    encoding: 'utf8',
    timeout: options.timeoutMs ?? 30000,
    windowsHide: true,
    maxBuffer: options.maxBuffer ?? 16 * 1024 * 1024,
    cwd: options.cwd,
    env: options.env ?? process.env,
    shell: options.shell === true,
  });

  const stdout = typeof result.stdout === 'string' ? result.stdout : '';
  const stderr = typeof result.stderr === 'string' ? result.stderr : '';
  const timedOut = result.error?.code === 'ETIMEDOUT' || result.signal === 'SIGTERM';

  const command = commandLabel(file);

  return {
    // A reading that ran out of time says nothing, whatever the command exited
    // with. spawnSync waits for the output pipes as well as the command, so a
    // command that exited 0 while something it started still held stdout came
    // back with status 0 and a timeout, and a health check, a version or the
    // staged self-test read that as a pass.
    ok: result.status === 0 && !timedOut,
    code: result.status,
    signal: result.signal ?? null,
    stdout,
    stderr,
    timedOut,
    error: result.error ? String(result.error.message || result.error) : null,
    command,
  };
}

/** Run a configured argv array through the shared runner. */
export function runArgv(command, options = {}) {
  return runCommand(command[0], command.slice(1), options);
}

/**
 * The same runner, without blocking the event loop.
 *
 * spawnSync is the right tool almost everywhere in the agent: the work is
 * sequential and the code reads better for it. It is the wrong tool for status,
 * where several independent probes each have their own timeout and running them
 * one after another made a healthy machine look unreachable — two probes with a
 * sixteen second budget added up to a thirty-two second reply, past both clients'
 * command budgets. Anything that has to fit inside a shared deadline uses this.
 */
const activeCommandGroups = new Set();
let commandExitCleanupInstalled = false;

export function runCommandAsync(file, args = [], options = {}) {
  return new Promise((resolve) => {
    const timeoutMs = options.timeoutMs ?? 30000;
    const maxBuffer = options.maxBuffer ?? 16 * 1024 * 1024;
    const command = commandLabel(file);

    let child;
    try {
      child = spawn(file, args, {
        windowsHide: true,
        // A timeout must terminate descendants too, including a child whose
        // parent exited while it still holds one of our output pipes open.
        detached: process.platform !== 'win32',
        cwd: options.cwd,
        env: options.env ?? process.env,
        shell: options.shell === true,
      });
    } catch (err) {
      resolve({
        ok: false,
        code: null,
        signal: null,
        stdout: '',
        stderr: '',
        timedOut: false,
        error: String(err.message || err),
        command,
      });
      return;
    }

    let stdout = '';
    let stderr = '';
    let timedOut = false;
    let settled = false;
    // Internal disposable workers can release parent-owned scratch only after
    // Windows confirms the process has released its native file handles.
    if (typeof options.onExit === 'function') child.once('exit', options.onExit);
    const closePipes = () => {
      child.stdin?.destroy();
      child.stdout?.destroy();
      child.stderr?.destroy();
    };
    const killPosixGroup = () => {
      if (!child.pid) return false;
      try { process.kill(-child.pid, 'SIGKILL'); return true; } catch (error) {
        if (error.code === 'ESRCH') return true;
        try { child.kill('SIGKILL'); } catch { /* cancellation could not be confirmed */ }
        return false;
      }
    };
    if (process.platform !== 'win32' && child.pid) {
      activeCommandGroups.add(child.pid);
      if (!commandExitCleanupInstalled) {
        commandExitCleanupInstalled = true;
        process.once('exit', () => {
          for (const pid of activeCommandGroups) {
            try { process.kill(-pid, 'SIGKILL'); } catch { /* the group may already be gone */ }
          }
        });
      }
    }
    const finish = (value) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      activeCommandGroups.delete(child.pid);
      resolve(value);
    };

    const timer = setTimeout(() => {
      timedOut = true;
      let cancellationRequested = false;
      const timeoutResult = (terminationConfirmed = false) => ({
        ok: false, code: null, signal: null, stdout, stderr, timedOut: true, terminationConfirmed,
        error: terminationConfirmed
          ? 'command deadline expired; the process exited and the command outcome is unknown'
          : cancellationRequested
            ? 'command deadline expired; process-tree termination was requested and the command outcome is unknown'
            : 'command deadline expired; process-tree termination could not be confirmed and the command outcome is unknown',
        command,
      });
      if (process.platform !== 'win32') {
        cancellationRequested = killPosixGroup();
      } else if (child.pid && options.terminateTree === false) {
        // Disposable workers that cannot spawn children need no taskkill tree
        // traversal. Terminate their owned handle directly, then await the exit
        // notification: Windows keeps SQLite files locked until that happens.
        // Only trusted internal workers may opt out of process-tree cleanup.
        closePipes();
        let terminationWait;
        const exited = () => {
          clearTimeout(terminationWait);
          child.unref();
          finish(timeoutResult(true));
        };
        child.once('exit', exited);
        try { cancellationRequested = child.kill('SIGKILL'); } catch { /* return an unconfirmed timeout */ }
        if (child.exitCode !== null || child.signalCode !== null) exited();
        else terminationWait = setTimeout(() => {
          child.removeListener('exit', exited);
          child.unref();
          finish(timeoutResult(false));
        }, 500);
        return;
      } else if (child.pid) {
        // Windows has no POSIX process group signal. Ask its tree-aware tool to
        // terminate the command and descendants, without waiting on its pipes.
        // The result below remains unknown even if taskkill later succeeds.
        try {
          const killer = spawn('taskkill.exe', ['/PID', String(child.pid), '/T', '/F'], { windowsHide: true, stdio: 'ignore' });
          // Do not abort traversal after an arbitrary 250 ms: on a loaded
          // machine that can leave the command alive. The cleanup helper has
          // no inherited pipes and may finish independently of this deadline.
          killer.once('error', () => {});
          killer.unref();
          cancellationRequested = true;
        } catch { /* report unconfirmed cancellation below */ }
      }
      // Resolving only from `close` is unsafe: grandchildren can keep these
      // pipes open indefinitely, even after their immediate parent is killed.
      closePipes();
      child.unref();
      finish(timeoutResult());
    }, timeoutMs);

    const collect = (stream, append) => {
      if (!stream) return;
      stream.setEncoding('utf8');
      stream.on('data', (chunk) => append(chunk));
      stream.on('error', () => {
        /* a closed pipe is not an outcome the caller can act on */
      });
    };
    collect(child.stdout, (chunk) => {
      if (stdout.length < maxBuffer) stdout += chunk.slice(0, maxBuffer - stdout.length);
    });
    collect(child.stderr, (chunk) => {
      if (stderr.length < maxBuffer) stderr += chunk.slice(0, maxBuffer - stderr.length);
    });

    child.on('error', (err) => {
      closePipes();
      finish({ ok: false, code: null, signal: null, stdout, stderr, timedOut, error: String(err.message || err), command });
    });
    child.on('close', (code, signal) => {
      if (timedOut) return;
      finish({
        ok: code === 0 && !timedOut,
        code: timedOut ? null : code,
        signal: signal ?? null,
        stdout,
        stderr,
        timedOut,
        error: null,
        command,
      });
    });
  });
}

export function runArgvAsync(command, options = {}) {
  return runCommandAsync(command[0], command.slice(1), options);
}

/**
 * Run tasks with a ceiling on how many are in flight at once.
 *
 * Unbounded parallelism across a dozen services would start a dozen PowerShell
 * processes on a machine that is already busy, which is its own kind of
 * disruption. Four at a time keeps a wide machine inside its status budget
 * without becoming the reason it is slow.
 */
export async function mapWithLimit(items, limit, worker) {
  const results = new Array(items.length);
  let next = 0;
  const runners = new Array(Math.max(1, Math.min(limit, items.length))).fill(null).map(async () => {
    for (;;) {
      const index = next;
      next += 1;
      if (index >= items.length) return;
      results[index] = await worker(items[index], index);
    }
  });
  await Promise.all(runners);
  return results;
}

/** One-line failure description for a runCommand or runMutation result. */
export function describeFailure(result) {
  // A command the mutation runner refused to start, because an earlier one may
  // still be running, never ran: it did not fail, and has no exit code to give.
  if (result.refused) return `${result.command} was ${result.error}`;
  if (result.timedOut && result.terminated === false) return `${result.command} timed out and could not be confirmed stopped`;
  if (result.timedOut) return `${result.command} timed out`;
  // Stopped by the mutation runner for a reason other than its deadline, which
  // its message gives, along with the command's name.
  if (typeof result.terminated === 'boolean') return result.error;
  const output = (result.stderr || result.stdout || '').trim().split('\n').slice(0, 3).join(' ');
  const reason = output || result.error || '';
  const code = result.code === null || result.code === undefined ? 'no exit code' : `exit ${result.code}`;
  return reason ? `${result.command} failed (${code}): ${reason}` : `${result.command} failed (${code})`;
}

export function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * A deadline that several probes can share. Everything that runs under a status
 * budget asks this how much time is left rather than assuming it has its own
 * full allowance, which is what stopped two sixteen-second probes from adding up
 * to a thirty-two-second status reply.
 */
export function deadline(totalMs) {
  const at = Date.now() + totalMs;
  return {
    at,
    remaining() {
      return Math.max(0, at - Date.now());
    },
    expired() {
      return Date.now() >= at;
    },
    /** At most `want` ms, and never more than is left. */
    slice(want) {
      return Math.max(0, Math.min(want, at - Date.now()));
    },
  };
}

// ---------------------------------------------------------------------------
// Changing the machine
// ---------------------------------------------------------------------------

const IS_WINDOWS = process.platform === 'win32';

/** How long a command has to stop after SIGTERM before it is killed outright. */
export const TERM_GRACE_MS = 5000;
/**
 * How long a tree that was killed has to be seen gone before the runner gives
 * up on it. A killed process is gone as soon as whoever inherited it reaps it,
 * so anything still there after this is not going to go.
 */
export const SETTLE_MS = 2000;
/** How long taskkill gets to stop a Windows tree, and the walk afterwards to find what is left. */
const TASKKILL_MS = 15000;
const TREE_WALK_MS = 30000;
/** How long output still in the pipes is read once the command has exited. */
const DRAIN_MS = 200;
const POLL_MS = 50;

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
/**
 * Every entry this process has written. What an entry says about its agent is
 * a process id, and Windows hands the id of an agent that died to the next
 * process soon enough, this agent included.
 */
const written = new Set();

function changeLedger(mutate) {
  const changed = withStore(
    ledgerPath(),
    (current) => ({ commands: mutate(Array.isArray(current.commands) ? current.commands : []) }),
    { purpose: 'the command ledger' },
  );
  if (!changed.ok) log(`the command ledger could not be written: ${changed.error}`, 'runner');
  return changed;
}

/**
 * When a process started, from ps, or null when ps cannot say. It is
 * processStartTime() in state.mjs without blocking the event loop.
 *
 * ps prints the time in the format of its locale, and an ssh session passes on
 * whatever locale its client had: a start time in German reads as no start
 * time at all, a reused process id then never looks reused, and its entry holds
 * the machine until the next reboot. So ps is always asked in the C locale.
 */
async function processStartedAt(pid) {
  const result = await runCommandAsync('/bin/ps', ['-o', 'lstart=', '-p', String(pid)], {
    timeoutMs: 5000,
    env: { ...process.env, LC_ALL: 'C' },
  });
  return result.ok ? parseProcessStart(result.stdout) : null;
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
 * everything further down no earlier than the command did, nor than the
 * process it names as its parent. A process that does not fit is somebody
 * else's that happens to have an id the tree once used.
 *
 * A command nobody saw end, because its agent died first, has no end to bound
 * its children by. When a newer process holds its id by now, the command ended
 * before that process started, and a child born since is the newer process's.
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
    `$endedBy = ${parentEnded}`,
    'function Born($process) { if ($process.CreationDate) { ([DateTimeOffset]$process.CreationDate).ToUnixTimeMilliseconds() } else { -1 } }',
    'function Left {',
    '  $all = @(Get-CimInstance -ClassName Win32_Process | Where-Object { $_.ProcessId -ne $PID })',
    '  $childBy = $endedBy',
    '  foreach ($holder in @($all | Where-Object { $_.ProcessId -eq $root -and (Born $_) -gt $rootBy })) {',
    '    $taken = (Born $holder) - 1',
    '    if ($childBy -lt 0 -or $taken -lt $childBy) { $childBy = $taken }',
    '  }',
    '  $found = @{}',
    `  $frontier = @($all | Where-Object { $born = Born $_; $born -ge $from -and (${roots}) })`,
    '  while ($frontier.Count -gt 0) {',
    '    foreach ($process in $frontier) { $found[[string]$process.ProcessId] = $true }',
    ...(treeOf
      ? [
        '    $parents = @{}',
        '    foreach ($process in $frontier) { $since = Born $process; if ($since -lt $from) { $since = $from }; $parents[[string]$process.ProcessId] = $since }',
        '    $frontier = @($all | Where-Object { $parent = [string]$_.ParentProcessId; -not $found.ContainsKey([string]$_.ProcessId) -and $parents.ContainsKey($parent) -and (Born $_) -ge $parents[$parent] })',
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
    { timeoutMs: TREE_WALK_MS },
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
 * An entry that ran out of time is judged by its whole tree, every part of
 * which was meant to be stopped. One that was still running when its agent
 * stopped is judged, on POSIX, by the command's own process: the command is in
 * a process group of its own, so it outlives its agent, and once it has ended
 * by itself whatever it started is what it meant to leave, as for any command
 * that exits by itself.
 *
 * Windows ends the command with its agent instead. Node puts every child it
 * does not detach in a job that closes with Node, and that child's own
 * children leave the job without a word and carry on: the installer an update
 * script started, whatever an npm script runs. So there an entry another agent
 * left is judged by its whole tree too. Its command need not have ended by
 * itself, and then nothing it left behind is there on purpose. Only an entry
 * this agent wrote, for a command it saw end and could not take out of the
 * ledger, is judged by the command alone. Which entries those are, this
 * process remembers rather than going by the agent's process id an entry
 * names: one naming this process's id that it did not write was written by an
 * agent that died and left the id free, and an agent that is still alive
 * counts as another all the same.
 *
 * Among what such an entry holds the machine for is a daemon a start command
 * had launched before its agent died, or the app a configured action had
 * opened: nothing on Windows tells either apart from a child the command had
 * not finished with, and a child like that going on unwatched is the overlap
 * the ledger is there to prevent. It holds the machine for as long as it runs,
 * until somebody stops it or restarts the machine; replacing the agent still
 * goes ahead.
 *
 * Nothing outlives a reboot, and anything the agent cannot read counts as
 * running.
 */
async function stillRunning(entry, { bootId }) {
  if (entry.bootId && bootId && entry.bootId !== bootId) return false;
  if (entry.platform !== process.platform) return false;
  const own = written.has(entry.id);
  // Written before the command was started, and its writer never got further.
  // The writer started before it wrote the entry, so a process holding its id
  // that started later is not it.
  if (!Number.isInteger(entry.pid)) return !own && entry.agentPid !== process.pid && processIsAlive(entry.agentPid, { startedAt: entry.startedAt });
  const whole = entry.state === 'unconfirmed';
  if (IS_WINDOWS) {
    const left = await windowsLeft(entry, { treeOf: whole || !own });
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
 * ended since are dropped on the way. The answer carries the message to give
 * and, when the command belonged to an operation, the `conflict` that names
 * it. A ledger that cannot be read is an answer too: nobody can then say that
 * nothing is running.
 *
 * `opId` narrows the question to one operation's commands, which is what an
 * operation asks once its body is done, and its commands are ones this process
 * ran. Every one of those it could not see stop is in `held` as well, so that
 * question is answered from there when the ledger cannot be read. So the
 * ending a body came to is rewritten only for a command of its own that may
 * still be at work, never for a file nobody can read: replacing the agent goes
 * ahead whatever the ledger says, as the way out for a machine it holds, and
 * its reply has to say what it did.
 */
export async function unsettledWork({ opId = null } = {}) {
  const read = readJsonStore(ledgerPath());
  if (!read.ok && opId === null) {
    return {
      entry: null,
      conflict: null,
      message: `the command ledger cannot be read (${read.error}), so nothing here can say whether an earlier command is still running`,
    };
  }
  const recorded = (read.ok && Array.isArray(read.value.commands) ? read.value.commands : []).filter((entry) => entry && typeof entry === 'object');
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
    ? await runCommandAsync('taskkill.exe', ['/PID', String(child.pid), '/T', '/F'], { timeoutMs: TASKKILL_MS })
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
 * ended by itself, true when the runner stopped it and the whole tree was seen
 * gone, false when that could not be seen. The runner stops a command at its
 * deadline, `timedOut`, and once in a while as it starts, when the ledger took
 * the entry and would not take its process id; either way it ran for a while,
 * and what it did in that while is unknown. A command refused because an
 * earlier one may still be running, or because the ledger would not take it at
 * all, was never started, and says so with `refused` and the `conflict` naming
 * that earlier command's operation.
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
  written.add(entry.id);
  inFlight.set(entry.id, entry);
  try {
    return await supervise(file, args, options, entry);
  } finally {
    inFlight.delete(entry.id);
  }
}

/**
 * The longest runMutation can take with a given timeout: the command's own
 * time, then stopping its tree and seeing that tree gone. What a deadline has
 * to leave room for before it starts one.
 */
export function mutationWorstCaseMs(timeoutMs) {
  return timeoutMs + (IS_WINDOWS ? TASKKILL_MS + SETTLE_MS + TREE_WALK_MS : TERM_GRACE_MS + SETTLE_MS);
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
    // for that has run out. `reason` is set when the deadline is not why. The
    // command was running either way, so it is never `refused`: that tells a
    // caller nothing happened, and here something may have.
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
        error: reason
          ? `${reason}${gone ? '' : ', and could not be confirmed stopped'}`
          : gone
            ? `${command} did not finish within ${duration(timeoutMs)} and was stopped`
            : `${command} did not finish within ${duration(timeoutMs)} and could not be confirmed stopped`,
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
      stopAll(`${command} was stopped as it started, because the command ledger would not record it`);
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
