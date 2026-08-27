// The busy gate: how one probe's answer is read, and how several services fold
// into the one object every disruptive action consults.

import assert from 'node:assert/strict';
import test from 'node:test';
import { aggregateBusy, dottedPath, readCommandBusy } from '../src/probes/busy.mjs';

function entry(name, busy) {
  return { service: { id: name.toLowerCase(), name }, busy };
}

const IDLE = { busy: false, reason: 'idle', unknown: false };

test('no services is idle', () => {
  assert.deepEqual(aggregateBusy([]), IDLE);
});

test('one service passes its own answer through unchanged', () => {
  const only = aggregateBusy([entry('T3 Code', { busy: true, reason: '1 turn running', unknown: false })]);
  assert.deepEqual(only, { busy: true, reason: '1 turn running', unknown: false });
});

test('several services are busy when any one of them is, and the reason names them', () => {
  const folded = aggregateBusy([
    entry('T3 Code', IDLE),
    entry('Sunshine', { busy: true, reason: 'a stream is open', unknown: false }),
  ]);
  assert.equal(folded.busy, true);
  assert.equal(folded.unknown, false);
  assert.equal(folded.reason, 'T3 Code: idle; Sunshine: a stream is open');
});

test('an unreadable probe anywhere makes the whole machine busy and unknown', () => {
  const folded = aggregateBusy([
    entry('T3 Code', IDLE),
    entry('Sunshine', { busy: true, reason: 'busy state unknown', unknown: true }),
  ]);
  assert.equal(folded.busy, true);
  assert.equal(folded.unknown, true);
});

test('a busy command reports through its exit code', () => {
  assert.deepEqual(readCommandBusy({ ok: true, code: 0, stdout: '', stderr: '', timedOut: false }), IDLE);

  const busy = readCommandBusy({
    ok: false,
    code: 1,
    stdout: '',
    stderr: 'a stream is open\nand another line',
    timedOut: false,
    command: 'busy.sh',
  });
  assert.deepEqual(busy, { busy: true, reason: 'a stream is open', unknown: false });
});

test('a busy command that prints JSON gets to explain itself', () => {
  const result = readCommandBusy({
    ok: true,
    code: 0,
    stdout: '{"busy":true,"reason":"two jobs queued"}',
    stderr: '',
    timedOut: false,
  });
  assert.deepEqual(result, { busy: true, reason: 'two jobs queued', unknown: false });

  // Output that is not JSON after all still leaves the exit code in charge.
  const fallback = readCommandBusy({ ok: true, code: 0, stdout: '{not json', stderr: '', timedOut: false });
  assert.deepEqual(fallback, IDLE);
});

test('a busy command that never came back fails closed', () => {
  const timedOut = readCommandBusy({
    ok: false,
    code: null,
    stdout: '',
    stderr: '',
    timedOut: true,
    command: 'busy.sh',
  });
  assert.equal(timedOut.busy, true);
  assert.equal(timedOut.unknown, true);
});

test('busyWhen follows a dotted path into the body', () => {
  const body = { state: { queue: { depth: 3 } }, idle: false };
  assert.equal(dottedPath(body, 'state.queue.depth'), 3);
  assert.equal(dottedPath(body, 'idle'), false);
  assert.equal(dottedPath(body, 'state.missing.depth'), undefined);
  assert.equal(dottedPath(body, ''), body);
});
