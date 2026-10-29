// One JSON object, whatever happens to the pipes and whatever throws
// (FINDINGS G3).
//
// stderr is a pipe to an ssh session that can close at any moment. A progress
// line written to it then failed with EPIPE, the `error` event nobody listened
// for ended the process on the spot, and the client got no reply at all from an
// agent that might have been in the middle of a rollback. Each run here gets the
// agent's real entry point with one of its pipes already closed, or with a
// failure planted where main() cannot catch it.

import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { setTimeout as delay } from 'node:timers/promises';
import { pathToFileURL } from 'node:url';
import { commandService, ENTRY, FIXTURE, fixtureConfig, fixtureData, readFixture, withHome, writeConfig } from './helpers.mjs';

/** Run the agent once, with stderr or stdout closed before it can write, and gather every line of its stdout. */
function agent(home, args, { closeStderr = false, closeStdout = false, preload = null } = {}) {
  const nodeArgs = preload ? ['--import', pathToFileURL(preload).href] : [];
  const child = spawn(process.execPath, [...nodeArgs, ENTRY, ...args], {
    env: { ...process.env, LEGIONCTL_HOME: home },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  if (closeStderr) child.stderr.destroy();
  if (closeStdout) child.stdout.destroy();
  let stdout = '';
  if (!closeStdout) child.stdout.on('data', (chunk) => { stdout += chunk; });
  if (!closeStderr) child.stderr.resume();
  return new Promise((resolve, reject) => {
    child.on('error', reject);
    child.on('close', (exitCode) => {
      const lines = stdout.split('\n').filter((line) => line.length > 0);
      resolve({ exitCode, lines, replies: lines.map((line) => JSON.parse(line)) });
    });
  });
}

function oneReply(run) {
  assert.equal(run.lines.length, 1, `exactly one JSON object on stdout:\n${run.lines.join('\n')}`);
  return run.replies[0];
}

function setUp(home, { actions = [], initial = {}, service = {} } = {}) {
  const dir = fixtureData(home, initial);
  const call = (verb) => [process.execPath, FIXTURE, verb, dir];
  writeConfig(home, fixtureConfig(dir, {
    services: [commandService(dir, service)],
    actions: [{ id: 'say', name: 'Say', command: call('action') }, ...actions],
  }));
  return dir;
}

test('a closed stderr does not cost the reply', async () => {
  await withHome(async (home) => {
    const dir = setUp(home);
    // `run` says what it is running on stderr before it runs it.
    const run = await agent(home, ['run', 'say'], { closeStderr: true });
    const reply = oneReply(run);
    assert.equal(reply.action, 'ran');
    assert.equal(run.exitCode, 0);
    assert.equal(readFixture(dir, 'action-runs'), '1');
  });
});

test('a closed stderr does not stop an update half way through its rollback', async () => {
  await withHome(async (home) => {
    const dir = setUp(home, {
      initial: { latest: '2.0.0', 'update-noop': 'on', 'rollback-to': '1.0.0' },
      service: { verify: [process.execPath, FIXTURE, 'verify', path.join(home, 'fixture')] },
    });
    fs.writeFileSync(path.join(dir, 'verify-fails'), 'on');
    const run = await agent(home, ['update'], { closeStderr: true });
    const reply = oneReply(run);
    assert.equal(reply.action, 'rolled-back');
    assert.equal(run.exitCode, 1);
    assert.equal(readFixture(dir, 'rollback-runs'), '1');
    assert.equal(readFixture(dir, 'running'), 'on', 'the rollback got as far as starting the service again');
  });
});

test('a closed stdout loses the reply but not the work, and is not a crash', async () => {
  await withHome(async (home) => {
    const dir = setUp(home);
    const run = await agent(home, ['run', 'say', '--op', 'closed-stdout-0001'], { closeStdout: true });
    assert.equal(run.exitCode, 0, 'rule 7 still gives the exit code of the reply nobody could read');
    assert.equal(readFixture(dir, 'action-runs'), '1');
    const record = JSON.parse(fs.readFileSync(path.join(home, 'ops', 'closed-stdout-0001.json'), 'utf8'));
    assert.equal(record.state, 'finished');
    assert.equal(record.result.action, 'ran');
  });
});

test('an exception nothing catches still answers once, as unknown, and stops the command it interrupted', async () => {
  await withHome(async (home) => {
    const started = path.join(home, 'started');
    const late = path.join(home, 'late');
    setUp(home, {
      actions: [{
        id: 'long',
        name: 'Long',
        command: [process.execPath, '-e', `const fs = require('node:fs'); fs.writeFileSync(${JSON.stringify(started)}, 'started'); setTimeout(() => fs.writeFileSync(${JSON.stringify(late)}, 'too late'), 2000); setInterval(() => {}, 1000)`],
        timeoutSeconds: 60,
      }],
    });
    // Thrown from a timer once the action is running, where no await reaches it.
    const preload = path.join(home, 'throw-later.mjs');
    fs.writeFileSync(preload, [
      "import fs from 'node:fs';",
      'const poll = setInterval(() => {',
      `  if (!fs.existsSync(${JSON.stringify(started)})) return;`,
      '  clearInterval(poll);',
      "  throw new Error('planted failure');",
      '}, 20);',
      'poll.unref();',
      '',
    ].join('\n'));
    const run = await agent(home, ['run', 'long'], { preload });
    const reply = oneReply(run);
    assert.equal(reply.ok, false);
    assert.equal(reply.reasonCode, 'internal');
    assert.equal(reply.outcome, 'unknown', 'whatever the action had done by then, nobody saw it');
    assert.match(reply.message, /planted failure/);
    assert.equal(run.exitCode, 1);
    assert.equal(fs.existsSync(started), true);
    await delay(2500);
    assert.equal(fs.existsSync(late), false, 'the command the failure interrupted went with the agent');
  });
});

test('a rejection nobody awaited after the reply has gone adds nothing to it', async () => {
  await withHome(async (home) => {
    setUp(home);
    const preload = path.join(home, 'reject-after-reply.mjs');
    fs.writeFileSync(preload, [
      'const write = process.stdout.write.bind(process.stdout);',
      'let written = false;',
      'process.stdout.write = (...args) => {',
      '  if (!written) { written = true; setTimeout(() => Promise.reject(new Error(\'late rejection\')), 50); }',
      '  return write(...args);',
      '};',
      '',
    ].join('\n'));
    const run = await agent(home, ['run', 'say'], { preload });
    const reply = oneReply(run);
    assert.equal(reply.action, 'ran');
    assert.equal(run.exitCode, 0, 'the exit code is the reply\'s, as rule 7 says');
    assert.match(fs.readFileSync(path.join(home, 'legionctl.log'), 'utf8'), /unhandled failure: Error: late rejection/);
  });
});
