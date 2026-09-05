import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import { validate, loadSchemas } from '../validate.mjs';

loadSchemas();
const errors = (schema, value) => validate({ $ref: schema }, value, { file: 'common.schema.json' }).errors;
const fixture = (name) => JSON.parse(fs.readFileSync(new URL(`../fixtures/${name}`, import.meta.url), 'utf8'));

test('configured identifiers and setup identities retain their distinct server grammars', () => {
  for (const id of ['demo', 'Demo_2.1-beta']) assert.deepEqual(errors('common.schema.json#/$defs/configuredId', id), []);
  for (const id of ['demo:blue', 'demo/path', 'demo@host', '-demo']) assert.ok(errors('common.schema.json#/$defs/configuredId', id).length);
  assert.deepEqual(errors('common.schema.json#/$defs/setupId', 'setup:desktop-1'), []);
  for (const id of ['setup/path', 'setup@host', 's'.repeat(65)]) assert.ok(errors('common.schema.json#/$defs/setupId', id).length);
  assert.deepEqual(errors('common.schema.json#/$defs/token', 'install:abc'), []);
});

test('metrics allow measured zero and unavailable readings, but stay bounded and typed', () => {
  const snapshot = fixture('status.metrics.json');
  assert.deepEqual(errors('status.schema.json', snapshot), []);
  assert.equal(snapshot.metrics[0].value, 0);
  assert.equal(snapshot.metrics.at(-1).checkedAt, null);
  snapshot.metrics[0].value = '0';
  assert.ok(errors('status.schema.json', snapshot).length);
  snapshot.metrics[0].value = 0;
  snapshot.metrics = Array.from({ length: 9 }, (_, index) => ({ ...snapshot.metrics[0], id: `probe-${index}` }));
  assert.ok(errors('status.schema.json', snapshot).length);
});

test('telemetry is explicit, command based, and capped at two seconds per probe', () => {
  const telemetry = { probes: [{ id: 'queue', command: ['node', '/opt/read-queue.mjs'], timeoutSeconds: 1 }] };
  for (const value of [null, { probes: [] }, telemetry]) assert.deepEqual(errors('telemetry.schema.json', value), []);
  for (const value of [{}, { enabled: true, probes: [] }, { probes: [{ id: 'queue' }] }]) assert.ok(errors('telemetry.schema.json', value).length);
  for (const timeout of [0, -1, 2.1]) {
    telemetry.probes[0].timeoutSeconds = timeout;
    assert.ok(errors('telemetry.schema.json', telemetry).length);
  }
  telemetry.probes[0].timeoutSeconds = 1;
  telemetry.probes[0].unit = 'jobs\n';
  assert.ok(errors('telemetry.schema.json', telemetry).length);
});

test('service configuration requests require an opaque expected hash and complete document envelope', () => {
  const read = fixture('service-config.get.json');
  const request = { expectedHash: read.hash, document: read.document };
  assert.deepEqual(errors('service-config-request.schema.json', request), []);
  assert.ok(errors('service-config-request.schema.json', { document: read.document }).length);
  assert.ok(errors('service-config-request.schema.json', { ...request, force: true }).length);
  assert.ok(errors('service-config-request.schema.json', { ...request, document: { services: [] } }).length);
});

test('service configuration fixtures distinguish read, validation, save and refusal', () => {
  for (const name of ['get', 'validate', 'set', 'conflict', 'invalid', 'restricted']) {
    assert.deepEqual(errors('service-config.schema.json', fixture(`service-config.${name}.json`)), []);
  }
  const saved = fixture('service-config.set.json');
  assert.equal(saved.hash, saved.proposedHash);
  assert.equal(saved.saved, true);
  assert.equal(saved.document, undefined);
  assert.equal(fixture('service-config.conflict.json').conflict, true);
  assert.equal(fixture('service-config.restricted.json').reasonCode, 'restricted');
});

test('application profiles separate detection from supported automation and keep drafts disabled', () => {
  const read = fixture('service-config.profiles.json');
  assert.deepEqual(errors('service-config.schema.json', read), []);
  assert.ok(read.profiles.some((profile) => profile.detected && profile.availability === 'manual' && profile.service === null));
  for (const profile of read.profiles) {
    if (profile.service) assert.equal(profile.service.updates.automatic, false);
  }
  read.profiles[0].availability = 'automatically-supported-everywhere';
  assert.ok(errors('service-config.schema.json', read).length);
});

test('successful agent rollback requires publisher verification and discarded stages stay absent', () => {
  const rollback = fixture('self-update.rolled-back.json');
  assert.deepEqual(errors('self-update.schema.json', rollback), []);
  rollback.manifest = null;
  assert.ok(errors('self-update.schema.json', rollback).length);
  assert.equal(fixture('self-update.self-test-failed.json').staged, null);
  assert.equal(fixture('self-update.legacy-rollback-refused.json').ok, false);
});
