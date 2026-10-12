// The validator, validated.
//
// Everything in contract/ depends on contract/validate.mjs being right. A
// subset validator that silently ignores a keyword does not report fewer
// problems, it reports none, and 101 fixtures then pass for no reason at all.
// These tests are what stops that.
//
// The composition tests matter most. `unevaluatedProperties` is the keyword
// that lets a reply schema pull the shared envelope in through allOf and still
// refuse a key nobody declared; if its annotation tracking is wrong, either
// every reply fails on `ok` or no reply fails on anything.

import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import { validate, loadSchemas, registry, resolveRef, jsonEqual, schemaForCommand, checkReply } from '../validate.mjs';

loadSchemas();

const ok = (schema, data) => validate(schema, data, { file: 'common.schema.json' }).errors;
const rejects = (schema, data) => {
  const errors = ok(schema, data);
  assert.ok(errors.length > 0, `expected a rejection, got none for ${JSON.stringify(data)}`);
  return errors;
};
const accepts = (schema, data) => {
  const errors = ok(schema, data);
  assert.deepEqual(errors, [], `expected acceptance for ${JSON.stringify(data)}`);
};

test('types, const and enum', () => {
  accepts({ type: 'integer' }, 4);
  rejects({ type: 'integer' }, 4.5);
  accepts({ type: 'number' }, 4);
  rejects({ type: 'string' }, null);
  accepts({ type: 'null' }, null);
  accepts({ const: 3 }, 3);
  rejects({ const: 3 }, '3');
  accepts({ enum: ['a', 'b'] }, 'b');
  rejects({ enum: ['a', 'b'] }, 'c');
});

test('a boolean schema is a schema', () => {
  accepts(true, { anything: 1 });
  rejects(false, 1);
  // This is how a schema says "this key must be absent", which the legacy
  // fixtures use to assert that a 2.x reply carries no `contract`.
  const schema = { type: 'object', properties: { contract: false } };
  accepts(schema, {});
  rejects(schema, { contract: 3 });
});

test('additionalProperties false closes only the keys declared beside it', () => {
  const schema = {
    type: 'object',
    properties: { a: { type: 'integer' } },
    additionalProperties: false,
  };
  accepts(schema, { a: 1 });
  rejects(schema, { a: 1, b: 2 });
});

test('unevaluatedProperties sees through $ref and allOf', () => {
  // Exactly the shape every reply schema uses. `ok` comes from the envelope
  // through allOf, `action` from the schema itself, and `stray` from nowhere.
  const schema = {
    type: 'object',
    allOf: [{ $ref: 'common.schema.json#/$defs/envelope' }],
    properties: { action: { type: 'string' } },
    required: ['action'],
    unevaluatedProperties: false,
  };
  const envelope = { ok: true, contract: 3, agentVersion: '3.0.0', system: { id: 'linux', name: 'Linux' } };
  accepts(schema, { ...envelope, action: 'updated' });
  const errors = rejects(schema, { ...envelope, action: 'updated', stray: 1 });
  assert.match(errors.join(' '), /unexpected key "stray"/);
});

test('unevaluatedProperties counts the branch of a oneOf that matched', () => {
  const schema = {
    type: 'object',
    oneOf: [
      { properties: { a: { type: 'integer' } }, required: ['a'] },
      { properties: { b: { type: 'integer' } }, required: ['b'] },
    ],
    unevaluatedProperties: false,
  };
  accepts(schema, { a: 1 });
  accepts(schema, { b: 1 });
  rejects(schema, { c: 1 });
});

test('oneOf means exactly one, so an ambiguous reply is a failure', () => {
  const schema = {
    oneOf: [
      { type: 'object', properties: { a: { type: 'integer' } } },
      { type: 'object', properties: { b: { type: 'integer' } } },
    ],
  };
  const errors = rejects(schema, { a: 1, b: 2 });
  assert.match(errors.join(' '), /more than one/);
});

test('if/then/else applies the branch the condition selected', () => {
  const schema = {
    type: 'object',
    properties: { action: { type: 'string' }, conflict: { type: 'object' } },
    if: { properties: { action: { const: 'conflict' } }, required: ['action'] },
    then: { required: ['conflict'] },
  };
  accepts(schema, { action: 'updated' });
  accepts(schema, { action: 'conflict', conflict: {} });
  rejects(schema, { action: 'conflict' });
});

test('patterns, lengths, bounds and uniqueness', () => {
  accepts({ pattern: '^[a-z]+$' }, 'abc');
  rejects({ pattern: '^[a-z]+$' }, 'Abc');
  rejects({ type: 'string', minLength: 2 }, 'a');
  rejects({ type: 'integer', minimum: 1 }, 0);
  rejects({ type: 'integer', maximum: 5 }, 6);
  rejects({ type: 'array', minItems: 1 }, []);
  rejects({ type: 'array', uniqueItems: true }, ['a', 'a']);
  accepts({ type: 'array', uniqueItems: true }, ['a', 'b']);
});

test('$ref resolves across files and reports what it could not find', () => {
  assert.equal(resolveRef('common.schema.json#/$defs/sha256', 'status.schema.json').error, undefined);
  assert.equal(resolveRef('common.schema.json', 'status.schema.json').error, undefined);
  assert.match(resolveRef('common.schema.json#/$defs/nope', 'status.schema.json').error, /does not resolve/);
  assert.match(resolveRef('nope.schema.json#/$defs/x', 'status.schema.json').error, /no schema named/);
  // A ref that resolves to nothing must be an error and never an empty schema:
  // an empty schema accepts everything, which is worse than no schema at all.
  rejects({ $ref: 'common.schema.json#/$defs/nope' }, 'anything');
});

test('the reason code enum really is closed', () => {
  const schema = { $ref: 'common.schema.json#/$defs/reasonCode' };
  accepts(schema, 'postcondition-failed');
  rejects(schema, 'nearly-busy');
  rejects(schema, 'BUSY');
});

test('the argv token grammar refuses everything a shell would look at', () => {
  const schema = { $ref: 'common.schema.json#/$defs/token' };
  accepts(schema, 't3');
  accepts(schema, 'C:/Users/me/.legion-control');
  accepts(schema, 'user@host');
  for (const bad of ['t3; rm -rf ~', '$HOME', 'a b', "it's", '`id`', '-leading-dash', '']) {
    rejects(schema, bad);
  }
});

test('the busy gate cannot be described as not knowing and not blocking', () => {
  const schema = { $ref: 'common.schema.json#/$defs/busy' };
  const base = {
    busy: true, unknown: true, monitored: true, reason: 'busy state unknown',
    evidence: 'timed-out', checkedAt: '2026-09-05T14:00:00.000Z', elapsedMs: 8000, error: 'timed out',
  };
  accepts(schema, base);
  // Fail-open is the one thing the gate must never be able to say.
  rejects(schema, { ...base, busy: false });
});

test('an unmonitored service blocks and a declared-none service does not', () => {
  const schema = { $ref: 'common.schema.json#/$defs/busy' };
  const at = '2026-09-05T14:00:00.000Z';
  accepts(schema, {
    busy: true, unknown: true, monitored: false, reason: 'not monitored',
    evidence: 'unmonitored', checkedAt: at, elapsedMs: 0, error: 'no busy probe is configured',
  });
  rejects(schema, {
    busy: false, unknown: false, monitored: false, reason: 'not monitored',
    evidence: 'unmonitored', checkedAt: at, elapsedMs: 0, error: null,
  });
  accepts(schema, {
    busy: false, unknown: false, monitored: true, reason: 'declared never busy',
    evidence: 'none', checkedAt: at, elapsedMs: 0, error: null,
  });
  // "none" is an answer, so it is monitored; counting it as unmonitored would
  // make a client warn about a service the user deliberately declared idle.
  rejects(schema, {
    busy: false, unknown: false, monitored: false, reason: 'declared never busy',
    evidence: 'none', checkedAt: at, elapsedMs: 0, error: null,
  });
});

test('every schema file is registered under its own name', () => {
  assert.ok(registry.size >= 28);
  for (const [name, schema] of registry) assert.equal(schema.$id, name);
  for (const required of [
    'common.schema.json', 'status.schema.json', 'config-set.schema.json',
    'controller-document.schema.json', 'bindings.schema.json', 'hash-vectors.schema.json',
  ]) {
    assert.ok(registry.has(required), `${required} is missing`);
  }
});

test('a type may be a list of types', () => {
  // The shared document writes its deprecated keys as "type": ["object", "null"].
  // Read as one string, that refused every document that carried them.
  accepts({ type: ['object', 'null'] }, null);
  accepts({ type: ['object', 'null'] }, { local: true });
  rejects({ type: ['object', 'null'] }, 'local');
  accepts({ $ref: 'controller-document.schema.json' }, { version: 1, machines: [], local: { enabled: false }, ssh: null });
});

test('JSON equality ignores key order and nothing else', () => {
  assert.equal(jsonEqual({ a: 1, b: [1, { c: null }] }, { b: [1, { c: null }], a: 1 }), true);
  assert.equal(jsonEqual([1, 2], [2, 1]), false);
  assert.equal(jsonEqual({ a: 1 }, { a: 1, b: undefined }), false);
  assert.equal(jsonEqual({ a: null }, { a: false }), false);
  assert.equal(jsonEqual('1', 1), false);
});

test('a reply is read with the schema its command and its shape call for', () => {
  const v3 = { ok: true, contract: 3 };
  assert.equal(schemaForCommand('status', { ok: true }), 'legacy-2x-status.schema.json');
  assert.equal(schemaForCommand('config', { ok: true }), 'legacy-2x-config.schema.json');
  assert.equal(schemaForCommand('update', { ok: true }), 'legacy-2x-mutation.schema.json');
  assert.equal(schemaForCommand('cancel', { ok: false, contract: 3, command: 'cancel' }), 'error.schema.json');
  assert.equal(schemaForCommand('self-update', { ok: false, contract: 3, command: 'self-update', action: 'failed' }), 'self-update.schema.json');
  assert.equal(schemaForCommand('config', v3), 'config-read.schema.json');
  assert.equal(schemaForCommand('config meta', v3), 'config-meta.schema.json');
  assert.equal(schemaForCommand('config set', v3), 'config-set.schema.json');
  assert.equal(schemaForCommand('policy set', v3), 'policy.schema.json');
  assert.equal(schemaForCommand('auto-update', v3), 'policy.schema.json');
  assert.equal(schemaForCommand('service-config set', v3), 'service-config.schema.json');
  assert.equal(schemaForCommand('op', v3), 'op.schema.json');
});

test('checkReply returns the problems of one reply, and none for a reply that holds', () => {
  const read = (name) => JSON.parse(fs.readFileSync(new URL(`../fixtures/${name}`, import.meta.url), 'utf8'));
  assert.deepEqual(checkReply('config-read.schema.json', read('config.read.json')), []);
  assert.deepEqual(checkReply('update.schema.json', read('update.conflict.json'), { where: 'conflict' }), []);

  const optimistic = { ...read('run.timed-out.json'), outcome: 'failed' };
  assert.match(checkReply('run.schema.json', optimistic, { where: 'run' }).join(' '), /^run: outcome is "failed", and the reducer of section 5\.6 gives "unknown"/);

  const unknownKey = { ...read('busy.idle.json'), surprise: true };
  assert.match(checkReply('busy.schema.json', unknownKey).join(' '), /unexpected key "surprise"/);
  assert.match(checkReply('nothing.schema.json', {}).join(' '), /no schema named nothing\.schema\.json/);
});
