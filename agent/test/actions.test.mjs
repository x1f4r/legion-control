// What `run` replies with, and what it does about the busy gate.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';

// The log and the state file follow LEGIONCTL_HOME, so point it at a throwaway
// directory before anything that might write is imported.
const HOME = fs.mkdtempSync(path.join(os.tmpdir(), 'legionctl-actions-'));
process.env.LEGIONCTL_HOME = HOME;

const { normalizeConfig } = await import('../src/config.mjs');
const { listActions, runAction, shapeOutput } = await import('../src/actions.mjs');

test.after(() => fs.rmSync(HOME, { recursive: true, force: true }));

function config(extra = {}) {
  return normalizeConfig(
    {
      services: [{ id: 'quiet', name: 'Quiet', kind: 'command', process: { type: 'none' }, busy: { type: 'none' } }],
      actions: [
        { id: 'hello', name: 'Say hello', command: ['echo', 'hello'], confirm: 'Nothing happens.' },
        { id: 'nope', name: 'Fail', command: ['sh', '-c', 'echo out; echo err >&2; exit 3'] },
      ],
      ...extra,
    },
    { platform: 'linux', base: HOME },
  );
}

test('output is both streams, condensed, and capped', () => {
  assert.equal(shapeOutput('one\n\ntwo\n', ''), 'one\ntwo');
  assert.equal(shapeOutput('out', 'err'), 'out\nerr');
  assert.equal(shapeOutput('', ''), '');

  const many = Array.from({ length: 25 }, (_, index) => `line ${index + 1}`).join('\n');
  const shaped = shapeOutput(many, '');
  assert.equal(shaped.split('\n').length, 21);
  assert.match(shaped, /^line 1\n/);
  assert.match(shaped, /… 5 more lines$/);
  assert.equal(shapeOutput('a\nb', '', 1), 'a\n… 1 more line');
});

test('status lists actions without their commands', () => {
  assert.deepEqual(listActions(config()), [
    { id: 'hello', name: 'Say hello', confirm: 'Nothing happens.', busyGated: false },
    { id: 'nope', name: 'Fail', confirm: null, busyGated: false },
  ]);
});

test('an action that ran reports ran, exit 0 and its output', async () => {
  const reply = await runAction(config(), 'hello');
  assert.equal(reply.ok, true);
  assert.equal(reply.action, 'ran');
  assert.equal(reply.id, 'hello');
  assert.equal(reply.exitCode, 0);
  assert.equal(reply.output, 'hello');
});

test('an action that failed keeps its exit code and both streams', async () => {
  const reply = await runAction(config(), 'nope');
  assert.equal(reply.ok, false);
  assert.equal(reply.action, 'failed');
  assert.equal(reply.exitCode, 3);
  assert.equal(reply.output, 'out\nerr');
});

test('an unknown action fails and says what there is', async () => {
  const reply = await runAction(config(), 'wishful');
  assert.equal(reply.ok, false);
  assert.equal(reply.action, 'failed');
  assert.equal(reply.exitCode, null);
  assert.match(reply.message, /hello, nope/);
});

test('a busy gated action defers while the machine is busy, and --force overrides', async () => {
  const busyConfig = config({
    services: [
      {
        id: 'widget',
        name: 'Widget',
        kind: 'command',
        process: { type: 'none' },
        busy: { type: 'command', command: ['sh', '-c', 'echo mid-flight >&2; exit 1'] },
      },
    ],
    actions: [{ id: 'hello', name: 'Say hello', command: ['echo', 'hello'], busyGated: true }],
  });

  const deferred = await runAction(busyConfig, 'hello');
  assert.equal(deferred.ok, true);
  assert.equal(deferred.action, 'deferred');
  assert.equal(deferred.exitCode, null);
  assert.match(deferred.message, /mid-flight/);

  const forced = await runAction(busyConfig, 'hello', { force: true });
  assert.equal(forced.action, 'ran');
});

test('an action that overruns its timeout is stopped and reported as failed', async () => {
  const slow = config({
    actions: [{ id: 'slow', name: 'Slow', command: ['sleep', '5'], timeoutSeconds: 1 }],
  });
  const reply = await runAction(slow, 'slow');
  assert.equal(reply.ok, false);
  assert.equal(reply.action, 'failed');
  assert.equal(reply.exitCode, null);
  assert.match(reply.message, /did not finish within 1 s/);
});
