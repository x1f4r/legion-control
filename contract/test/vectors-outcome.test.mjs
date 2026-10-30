// The outcome family: the reducer of v3.md section 5.6, its vectors, and the
// outcomes the fixtures state.
//
// Each reference defect below is a mistake a port of the reducer could make
// without noticing, because every one of them still gives a plausible answer.
// Each must be caught twice: by validate.mjs, because the committed vectors no
// longer follow from the reference, and by the generator, because a hand-stated
// want disagrees with it. The properties after them are the ones the vectors
// cannot state case by case.

import test from 'node:test';
import assert from 'node:assert/strict';
import path from 'node:path';
import { CONTRACT, readJson, referenceDefect } from './support.mjs';
import { OUTCOMES, TERMINAL_OUTCOMES, reduceOutcome, recordInput, run } from '../tools/outcome.mjs';

referenceDefect('already-running read as a success', 'tools/outcome.mjs',
  "  if (PENDING_ACTIONS.has(action)) return 'pending';",
  "  if (action === 'already-running') return 'succeeded';\n  if (PENDING_ACTIONS.has(action)) return 'pending';",
  /already-running: tools\/outcome\.mjs gives \{"outcome":"succeeded"\}/);

referenceDefect('a cancel that ignores its reason code', 'tools/outcome.mjs',
  "    if (reasonCode === 'cancelled') return 'deferred';\n",
  '',
  /cancelled-request: tools\/outcome\.mjs gives \{"outcome":"failed"\}/);

referenceDefect('a cancellation the agent could not finish read as a success', 'tools/outcome.mjs',
  "return reasonCode === 'internal' ? 'unknown' : 'failed';",
  "return 'succeeded';",
  /cancelled-internal: tools\/outcome\.mjs gives \{"outcome":"succeeded"\}/);

referenceDefect('a stated outcome passed through whatever it says', 'tools/outcome.mjs',
  "if (typeof x.outcome === 'string') return OUTCOMES.includes(x.outcome) ? x.outcome : 'unknown';",
  "if (typeof x.outcome === 'string') return x.outcome;",
  /explicit-unknown-value: tools\/outcome\.mjs gives \{"outcome":"partial"\}/);

referenceDefect('the state of a record ignored', 'tools/outcome.mjs',
  "if (x.state === 'queued' || x.state === 'running') return 'pending';",
  "if (x.state === 'queued') return 'pending';",
  /state-running-with-updated: tools\/outcome\.mjs gives \{"outcome":"succeeded"\}/);

referenceDefect('a timed-out failure called a failure', 'tools/outcome.mjs',
  "if (action === 'failed') return UNKNOWN_FAILURES.has(reasonCode) ? 'unknown' : 'failed';",
  "if (action === 'failed') return 'failed';",
  /failed-timed-out: tools\/outcome\.mjs gives \{"outcome":"failed"\}/);

referenceDefect('a lock refusal with ok false called a failure', 'tools/outcome.mjs',
  "if (reasonCode === 'operation-in-progress' || reasonCode === 'lock-held') return 'deferred';",
  "if (reasonCode === 'operation-in-progress') return 'deferred';",
  /no-action-lock-held: tools\/outcome\.mjs gives \{"outcome":"failed"\}/);

referenceDefect('a record reduced from its result alone', 'tools/outcome.mjs',
  'return { ...result, state: op.state, outcome: op.outcome ?? result.outcome };',
  'return { ...result, state: op.state, outcome: result.outcome };',
  /record-outcome-beats-result-outcome: tools\/outcome\.mjs gives \{"outcome":"failed"\}/);

test('the reducer only ever answers with an outcome, and a stated one only when it knows it', () => {
  const values = [undefined, null, true, false, 'running', 'queued', 'finished', 'accepted', 'conflict', 'failed',
    'teleported', 'busy', 'timed-out', 'internal', 'cancelled', 'partial', 'succeeded'];
  for (const outcome of values) {
    for (const state of values) {
      for (const ok of values) {
        for (const action of values) {
          for (const reasonCode of [undefined, null, 'busy', 'timed-out', 'cancelled', 'lock-held']) {
            const answer = reduceOutcome({ outcome, state, ok, action, reasonCode });
            assert.ok(OUTCOMES.includes(answer), JSON.stringify({ outcome, state, ok, action, reasonCode }));
          }
        }
      }
    }
  }
});

test('queued and running are pending whatever else they say, and finished never is unless stated', () => {
  for (const action of ['updated', 'failed', 'conflict', 'interrupted', null]) {
    assert.equal(reduceOutcome({ state: 'running', action, ok: false, reasonCode: 'internal' }), 'pending');
    assert.equal(reduceOutcome({ state: 'queued', action }), 'pending');
    assert.notEqual(reduceOutcome({ state: 'finished', action, ok: false, reasonCode: 'internal' }), 'pending');
  }
});

test('a record reduces from its own outcome first, then its result’s, then its fields', () => {
  const result = { ok: false, action: 'failed', reasonCode: 'apply-failed', message: 'x' };
  assert.equal(reduceOutcome(recordInput({ state: 'finished', result })), 'failed');
  assert.equal(reduceOutcome(recordInput({ state: 'finished', result: { ...result, outcome: 'unknown' } })), 'unknown');
  assert.equal(reduceOutcome(recordInput({ state: 'finished', outcome: 'deferred', result: { ...result, outcome: 'unknown' } })), 'deferred');
  assert.equal(reduceOutcome(recordInput({ state: 'running', result: null })), 'pending');
  assert.throws(() => run('nonsense', {}), /no group "nonsense"/);
});

test('every terminal outcome is reachable from the fields alone', () => {
  const reached = new Set();
  const data = readJson(path.join(CONTRACT, 'vectors', 'outcome.json'));
  for (const entry of data.groups.reduce) {
    if (!('outcome' in entry.input)) reached.add(entry.expect.outcome);
  }
  for (const outcome of [...TERMINAL_OUTCOMES, 'pending']) assert.ok(reached.has(outcome), `${outcome} is never derived`);
});

test('every outcome a fixture states is what the reducer gives, or unknown where it gives failed', () => {
  const index = readJson(path.join(CONTRACT, 'fixtures', 'index.json'));
  let compared = 0;
  for (const entry of index.fixtures.filter((fixture) => fixture.outcome !== null)) {
    const data = readJson(path.join(CONTRACT, 'fixtures', entry.file));
    const { outcome, ...fields } = data;
    const derived = reduceOutcome(fields);
    assert.ok(outcome === derived || (outcome === 'unknown' && derived === 'failed'), `${entry.file}: states ${outcome}, the reducer gives ${derived}`);
    compared += 1;
  }
  assert.ok(compared >= 60, `only ${compared} fixtures state an outcome`);
});
