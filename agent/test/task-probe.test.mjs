// The Windows scheduled-task probe, failing (FINDINGS G13).
//
// The probe status uses already stopped at the first error and believed only a
// typed Boolean. The synchronous one, which the stop loop, the busy gate and
// recovery act on, ran with SilentlyContinue and parsed whatever came out. A
// PowerShell that exited 1 after an access-denied process query, having printed
// a well-formed "not running", made the service read as stopped with no error:
// an update could replace its files and the busy gate retire its running turns.
//
// Both paths now share one reading, and these cases are fed to both. A
// stand-in powershell.exe on PATH answers for the synchronous path, the way a
// real one would, so what is exercised is the probe as the agent runs it.

import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { deadline } from '../src/config.mjs';
import { checkT3Sqlite } from '../src/probes/t3-sqlite.mjs';
import { isRunning, probeProcess, probeProcessAsync, readTaskProbe } from '../src/probes/process.mjs';
import { livenessOf } from '../src/service.mjs';
import { withHome } from './helpers.mjs';

const require = createRequire(import.meta.url);
const posix = process.platform !== 'win32';
const service = { id: 'demo', name: 'Demo', kind: 'command', process: { type: 'scheduled-task', task: 'Demo', match: 'demo-server' } };
const absent = JSON.stringify({ running: false, pids: '' });

/** What PowerShell can answer, and none of it says the process is gone. */
const FAILURES = [
  { name: 'exit 1 after printing a well-formed "not running"', code: 1, stdout: absent, stderr: 'Get-CimInstance : Access denied' },
  // What a denied process query is now that the script stops at it.
  { name: 'a CIM access denial', code: 1, stdout: '', stderr: 'Get-CimInstance : Access denied' },
  { name: 'nothing printed at all', code: 0, stdout: '', stderr: '' },
  { name: 'malformed output', code: 0, stdout: 'running: no', stderr: '' },
  { name: 'a running state that is not a Boolean', code: 0, stdout: JSON.stringify({ running: 'False', pids: '' }), stderr: '' },
];

const result = ({ code, stdout, stderr }) => ({ ok: code === 0, code, signal: null, stdout, stderr, timedOut: false, error: null, command: 'powershell.exe' });

/** A stand-in powershell.exe that prints what the test says and keeps the script it was given. */
async function withFakePowerShell(home, body) {
  const bin = path.join(home, 'bin');
  fs.mkdirSync(bin);
  const answer = path.join(home, 'answer');
  fs.mkdirSync(answer);
  fs.writeFileSync(path.join(bin, 'powershell.exe'), [
    '#!/bin/sh',
    `dir='${answer}'`,
    'for last; do :; done',
    'printf "%s" "$last" > "$dir/script"',
    'cat "$dir/stdout"',
    'cat "$dir/stderr" >&2',
    'exit "$(cat "$dir/code")"',
    '',
  ].join('\n'), { mode: 0o755 });
  const previous = process.env.PATH;
  process.env.PATH = `${bin}${path.delimiter}${previous}`;
  const answerWith = ({ code, stdout, stderr }) => {
    fs.writeFileSync(path.join(answer, 'code'), String(code));
    fs.writeFileSync(path.join(answer, 'stdout'), stdout);
    fs.writeFileSync(path.join(answer, 'stderr'), stderr);
  };
  try {
    return await body({ answerWith, script: () => fs.readFileSync(path.join(answer, 'script'), 'utf8') });
  } finally {
    process.env.PATH = previous;
  }
}

/** A T3 state database with one turn still running, an hour old. */
function runningTurn(home) {
  const { DatabaseSync } = require('node:sqlite');
  const dir = path.join(home, 't3', 'userdata');
  fs.mkdirSync(dir, { recursive: true });
  const db = new DatabaseSync(path.join(dir, 'state.sqlite'));
  db.exec('CREATE TABLE projection_turns (thread_id TEXT, turn_id TEXT, state TEXT, requested_at TEXT)');
  db.exec('CREATE TABLE projection_pending_approvals (request_id TEXT, thread_id TEXT, turn_id TEXT, created_at TEXT, status TEXT)');
  db.prepare('INSERT INTO projection_turns VALUES (?, ?, ?, ?)').run('t', 'u', 'running', new Date(Date.now() - 60 * 60 * 1000).toISOString());
  db.close();
  return { home: path.join(home, 't3') };
}

test('a failed, timed-out or malformed task reading is unknown, never stopped', () => {
  const cases = [
    ...FAILURES.map(result),
    { ok: false, code: null, signal: 'SIGTERM', stdout: absent, stderr: '', timedOut: true, error: null, command: 'powershell.exe' },
  ];
  for (const failed of cases) {
    const { probe, parsed } = readTaskProbe(failed);
    assert.equal(parsed, null, failed.stdout);
    assert.equal(probe.unitState, 'unknown');
    assert.notEqual(probe.error, null, 'an unknown reading says why');
  }
  assert.match(readTaskProbe(cases.at(-1)).probe.error, /timed out/);

  const gone = readTaskProbe(result({ code: 0, stdout: absent, stderr: '' })).probe;
  assert.deepEqual(gone, { running: false, unitState: 'stopped', pids: [], startedAt: null, error: null });
  const up = readTaskProbe(result({ code: 0, stdout: JSON.stringify({ running: true, pids: '42,43', startedAt: '2026-09-05T12:00:00Z' }), stderr: '' })).probe;
  assert.deepEqual(up, { running: true, unitState: 'running', pids: [42, 43], startedAt: '2026-09-05T12:00:00.000Z', error: null });
});

test('status reads a failing task probe exactly as the stop guard does', async () => {
  for (const failure of FAILURES) {
    const probe = await probeProcessAsync(service, { clock: deadline(1000), runner: async () => result(failure) });
    assert.equal(probe.unitState, 'unknown', failure.name);
    assert.notEqual(probe.error, null, failure.name);
  }
});

test('a failing synchronous task probe keeps the stop guard and the busy guard', { skip: !posix && 'a stand-in powershell.exe on PATH' }, async () => {
  await withHome(async (home) => withFakePowerShell(home, async ({ answerWith, script }) => {
    const t3 = runningTurn(home);
    for (const failure of FAILURES) {
      answerWith(failure);
      const probe = probeProcess(service);
      assert.equal(probe.unitState, 'unknown', failure.name);
      assert.notEqual(probe.error, null, failure.name);
      assert.match(script(), /^\$ErrorActionPreference = 'Stop'$/m, 'the query stops at its first error');
      assert.doesNotMatch(script(), /\$ErrorActionPreference = 'SilentlyContinue'/);

      // The stop loop waits on this: unknown must read as still running.
      assert.equal(isRunning(service), true, failure.name);
      // And the busy gate retires a running turn only when the service is
      // known to be down, or known to have restarted since.
      const liveness = livenessOf(service);
      assert.notEqual(liveness.serviceRunning, false, failure.name);
      assert.equal(checkT3Sqlite(t3, { liveness }).busy, true, `${failure.name}: the running turn still blocks`);
    }

    // A process that really is gone is still stopped, and only then does the
    // turn it was running stop counting.
    answerWith({ code: 0, stdout: absent, stderr: '' });
    assert.deepEqual(probeProcess(service), { running: false, unitState: 'stopped', pids: [], error: null });
    assert.equal(isRunning(service), false);
    const liveness = livenessOf(service);
    assert.equal(liveness.serviceRunning, false);
    assert.equal(checkT3Sqlite(t3, { liveness }).busy, false);
  }));
});
