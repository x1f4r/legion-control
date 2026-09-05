// What `run` replies with, and what it does about the busy gate.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import dgram from 'node:dgram';

// The log and the state file follow LEGIONCTL_HOME, so point it at a throwaway
// directory before anything that might write is imported.
const HOME = fs.mkdtempSync(path.join(os.tmpdir(), 'legionctl-actions-'));
process.env.LEGIONCTL_HOME = HOME;

const { normalizeConfig } = await import('../src/config.mjs');
const { listActions, runAction, shapeOutput, magicPacket, sendMagicPacket } = await import('../src/actions.mjs');

test.after(() => fs.rmSync(HOME, { recursive: true, force: true }));

function config(extra = {}) {
  const result = normalizeConfig(
    {
      services: [{ id: 'quiet', name: 'Quiet', kind: 'command', process: { type: 'none' }, busy: { type: 'none' } }],
      actions: [
        { id: 'hello', name: 'Say hello', command: [process.execPath, '-e', 'console.log("hello")'], confirm: 'Nothing happens.' },
        { id: 'nope', name: 'Fail', command: [process.execPath, '-e', 'console.log("out"); console.error("err"); process.exit(3)'] },
      ],
      ...extra,
    },
    { platform: process.platform, base: HOME },
  );
  assert.equal(result.ok, true, JSON.stringify(result.errors));
  return result.config;
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
    { id: 'hello', name: 'Say hello', confirm: 'Nothing happens.', busyGated: false, kind: 'command' },
    { id: 'nope', name: 'Fail', confirm: null, busyGated: false, kind: 'command' },
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
        busy: { type: 'command', command: [process.execPath, '-e', 'console.error("mid-flight"); process.exit(1)'] },
      },
    ],
    actions: [{ id: 'hello', name: 'Say hello', command: [process.execPath, '-e', 'console.log("hello")'], busyGated: true }],
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
    actions: [{ id: 'slow', name: 'Slow', command: [process.execPath, '-e', 'setTimeout(() => {}, 5000)'], timeoutSeconds: 1 }],
  });
  const reply = await runAction(slow, 'slow');
  assert.equal(reply.ok, false);
  assert.equal(reply.action, 'failed');
  assert.equal(reply.exitCode, null);
  assert.match(reply.message, /did not finish within 1 s/);
});


test('magic packet contains exactly sixteen MAC repetitions and rejects malformed pairs', () => {
  const packet = magicPacket('00:11:22:33:44:55');
  assert.equal(packet.length, 102);
  assert.deepEqual(packet.subarray(0, 6), Buffer.alloc(6, 255));
  for (let n = 0; n < 16; n++) assert.equal(packet.subarray(6 + 6 * n, 12 + 6 * n).toString('hex'), '001122334455');
  for (const bad of ['xx:11:22:33:44:55', '0:1:2:3:4:5', '00:11:22:33:44:55:66']) assert.throws(() => magicPacket(bad));
});

test('native wake sends configured repeats to a real UDP listener', async () => {
  const listener = dgram.createSocket('udp4');
  try {
    await new Promise((resolve) => listener.bind(0, '127.0.0.1', resolve));
    const received = [];
    const arrivals = new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('packets did not arrive')), 3000);
      listener.on('message', (packet) => {
        received.push(packet);
        if (received.length === 2) { clearTimeout(timer); resolve(); }
      });
    });
    const sent = await sendMagicPacket({ mac: '00:11:22:33:44:55', broadcast: ['127.0.0.1'], ports: [listener.address().port], repeats: 2 });
    await arrivals;
    assert.equal(sent.sent, 2);
    assert.deepEqual(sent.errors, []);
    for (const packet of received) assert.deepEqual(packet, magicPacket('00:11:22:33:44:55'));
  } finally { listener.close(); }
});
