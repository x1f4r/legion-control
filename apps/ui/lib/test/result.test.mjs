import assert from 'node:assert/strict';
import test from 'node:test';
import { classifyResult, fanOutEntries, formatScalar, isFanOut } from '../result.js';

test('isFanOut detects fan-out shape and leaves ordinary results alone', () => {
  assert.equal(isFanOut({ results: { tower: { ok: { v: 1 } }, pi: { error: { code: 'unreachable' } } } }), true);
  assert.equal(isFanOut({ results: [] }), false);
  assert.equal(isFanOut({ results: { a: 1 } }), false);
  assert.equal(isFanOut({ node: 'tower', results: {} }), false);
});

test('fanOutEntries sorts machines and distinguishes ok vs error', () => {
  const entries = fanOutEntries({
    results: {
      tower: { ok: { node: 'tower' } },
      pi: { error: { code: 'unreachable', message: 'Did not answer.' } },
    },
  });
  assert.equal(entries.length, 2);
  assert.equal(entries[0].machine, 'pi');
  assert.equal(entries[0].ok, false);
  assert.equal(entries[0].error.code, 'unreachable');
  assert.equal(entries[1].machine, 'tower');
  assert.equal(entries[1].ok, true);
  assert.deepEqual(entries[1].value, { node: 'tower' });
});

test('classifyResult structures scalars, arrays, and nested objects', () => {
  assert.equal(formatScalar(true), 'yes');
  assert.equal(formatScalar(false), 'no');
  assert.equal(formatScalar(null), 'null');

  const tree = classifyResult({
    node: 'tower',
    tags: ['a', 'b'],
    empty: [],
    nested: { ok: true },
  });
  assert.equal(tree.kind, 'object');
  assert.equal(tree.entries[0].node.kind, 'scalar');
  assert.equal(tree.entries[1].node.kind, 'scalar-array');
  assert.deepEqual(tree.entries[1].node.items, ['a', 'b']);
  assert.equal(tree.entries[2].node.kind, 'empty-array');
  assert.equal(tree.entries[3].node.kind, 'object');
});
