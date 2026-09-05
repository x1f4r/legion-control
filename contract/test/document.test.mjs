// The shared setup document and the private bindings.
//
// These are the rules that are about relationships between entries, which is
// exactly what a schema cannot express and what an editor actually gets wrong.
// A helper that names a machine nobody defined, two machines that wake each
// other, a compatibility alias that disagrees with the list it aliases: each of
// them looks fine one entry at a time.

import test from 'node:test';
import assert from 'node:assert/strict';
import { validate, loadSchemas, checkControllerDocument } from '../validate.mjs';

loadSchemas();

const DOC_SCHEMA = { $ref: 'controller-document.schema.json' };
const BINDINGS_SCHEMA = { $ref: 'bindings.schema.json' };

const schemaErrors = (schema, data) => validate(schema, data, { file: 'common.schema.json' }).errors;
const helper = (machine, action) => ({ machine, action });

const document = (overrides = {}) => ({
  version: 1,
  controller: { id: 'setup-3f9c2a71', revision: 1, lineage: [] },
  machines: [{ id: 'pi', name: 'Atlas' }],
  ...overrides,
});

// --- shape ------------------------------------------------------------------

test('the smallest real document is a version and a machine', () => {
  assert.deepEqual(schemaErrors(DOC_SCHEMA, { version: 1, machines: [{ id: 'pi' }] }), []);
  assert.ok(schemaErrors(DOC_SCHEMA, { machines: [] }).length > 0);
  assert.ok(schemaErrors(DOC_SCHEMA, { version: 2, machines: [] }).length > 0,
    'the document version stays 1: a bump would make every 1.2 client refuse a document for keys it can safely ignore');
});

test('unknown keys are legal everywhere and must survive an edit', () => {
  const withFutureKeys = {
    version: 1,
    somethingFrom14: { keep: true },
    machines: [{ id: 'pi', futureMachineKey: [1, 2], endpoints: [{ host: 'h', futureEndpointKey: 'x' }] }],
  };
  assert.deepEqual(schemaErrors(DOC_SCHEMA, withFutureKeys), []);
});

test('a lineage is up to 32 distinct sha256 hashes', () => {
  const hash = (n) => String(n).padStart(64, '0');
  assert.deepEqual(schemaErrors(DOC_SCHEMA, document({ controller: { id: 's', revision: 2, lineage: [hash(1)] } })), []);
  // Duplicates would let a document claim to descend from itself twice and
  // would waste the cap that decides when divergence is reported.
  assert.ok(schemaErrors(DOC_SCHEMA, document({ controller: { id: 's', revision: 2, lineage: [hash(1), hash(1)] } })).length > 0);
  assert.ok(schemaErrors(DOC_SCHEMA, document({ controller: { id: 's', revision: 2, lineage: ['nothex'] } })).length > 0);
  const thirtyThree = Array.from({ length: 33 }, (_, i) => hash(i));
  assert.ok(schemaErrors(DOC_SCHEMA, document({ controller: { id: 's', revision: 2, lineage: thirtyThree } })).length > 0);
});

test('the source enum is the wire enum and does not gain platform names', () => {
  for (const source of ['mac', 'desktop', 'phone', 'cli', 'legacy']) {
    assert.deepEqual(schemaErrors(DOC_SCHEMA, document({ controller: { id: 's', revision: 1, source } })), []);
  }
  for (const invented of ['android', 'windows', 'linux']) {
    assert.ok(schemaErrors(DOC_SCHEMA, document({ controller: { id: 's', revision: 1, source: invented } })).length > 0,
      `${invented} is a device kind, not a client kind; the human name goes in controller.device`);
  }
});

// --- relationships ----------------------------------------------------------

test('a machine cannot be its own wake helper', () => {
  const { errors } = checkControllerDocument(document({
    machines: [{ id: 'tower', wake: { helpers: [helper('tower', 'wake-tower')] } }],
  }));
  assert.match(errors.join(' '), /its own wake helper/);
});

test('a helper must be a machine this document describes', () => {
  const { errors } = checkControllerDocument(document({
    machines: [{ id: 'tower', wake: { helpers: [helper('ghost', 'wake-tower')] } }],
  }));
  assert.match(errors.join(' '), /"ghost" is not a machine/);
});

test('helpers that wake each other are a cycle, and a cycle is refused', () => {
  const { errors } = checkControllerDocument(document({
    machines: [
      { id: 'a', wake: { helpers: [helper('b', 'wake-a')] } },
      { id: 'b', wake: { helpers: [helper('a', 'wake-b')] } },
    ],
  }));
  assert.match(errors.join(' '), /wake helper cycle/);
});

test('a longer ring is still a ring', () => {
  const { errors } = checkControllerDocument(document({
    machines: [
      { id: 'a', wake: { helpers: [helper('b', 'wake-a')] } },
      { id: 'b', wake: { helpers: [helper('c', 'wake-b')] } },
      { id: 'c', wake: { helpers: [helper('a', 'wake-c')] } },
    ],
  }));
  assert.match(errors.join(' '), /wake helper cycle/);
});

test('a chain that is not a ring is fine', () => {
  const { errors } = checkControllerDocument(document({
    machines: [
      { id: 'pi', alwaysOn: true },
      { id: 'legion', wake: { helpers: [helper('pi', 'wake-legion')] } },
      { id: 'tower', wake: { helpers: [helper('legion', 'wake-tower')] } },
    ],
  }));
  assert.deepEqual(errors, []);
});

test('the singular helper must repeat helpers[0]', () => {
  const diverging = document({
    machines: [
      { id: 'pi' }, { id: 'nas' },
      { id: 'tower', wake: { helper: helper('nas', 'wake-tower'), helpers: [helper('pi', 'wake-tower')] } },
    ],
  });
  assert.match(checkControllerDocument(diverging).errors.join(' '), /compatibility alias/);

  const agreeing = document({
    machines: [
      { id: 'pi' }, { id: 'nas' },
      { id: 'tower', wake: { helper: helper('pi', 'wake-tower'), helpers: [helper('pi', 'wake-tower'), helper('nas', 'wake-tower')] } },
    ],
  });
  assert.deepEqual(checkControllerDocument(agreeing).errors, []);
});

test('a machine cannot name a site nobody defined', () => {
  const { errors } = checkControllerDocument(document({
    sites: [{ id: 'attic', lanPrefixes: ['10.0.0.'] }],
    machines: [{ id: 'tower', site: 'flat' }],
  }));
  assert.match(errors.join(' '), /site "flat" is not in sites/);
});

test('duplicate ids are refused wherever a route or a record would become ambiguous', () => {
  assert.match(checkControllerDocument(document({ machines: [{ id: 'pi' }, { id: 'pi' }] })).errors.join(' '), /two machines share/);
  assert.match(checkControllerDocument(document({
    machines: [{ id: 'pi', endpoints: [{ id: 'lan', host: 'a' }, { id: 'lan', host: 'b' }] }],
  })).errors.join(' '), /two endpoints share/);
  assert.match(checkControllerDocument(document({
    machines: [{ id: 'pi', systems: [{ id: 'linux' }, { id: 'linux' }] }],
  })).errors.join(' '), /two systems share/);
});

// --- overlapping subnets ----------------------------------------------------

test('two sites may share a private subnet, and that is a warning, not an error', () => {
  // Home routers ship the same default range. Refusing this would refuse the
  // user's actual network; the right answer is to say that a prefix match no
  // longer identifies a site.
  const { errors, warnings } = checkControllerDocument(document({
    sites: [
      { id: 'attic', lanPrefixes: ['192.168.178.'] },
      { id: 'flat', lanPrefixes: ['192.168.178.'] },
    ],
    machines: [{ id: 'pi', site: 'attic' }, { id: 'tower', site: 'flat' }],
  }));
  assert.deepEqual(errors, []);
  assert.match(warnings.join(' '), /does not say which site a device is on/);
});

test('a cross-site helper warns, because a router action is a legitimate one', () => {
  const { errors, warnings } = checkControllerDocument(document({
    sites: [{ id: 'attic', lanPrefixes: ['10.0.0.'] }, { id: 'flat', lanPrefixes: ['10.1.0.'] }],
    machines: [
      { id: 'router', site: 'attic', alwaysOn: true },
      { id: 'tower', site: 'flat', wake: { helpers: [helper('router', 'wake-tower')] } },
    ],
  }));
  assert.deepEqual(errors, []);
  assert.match(warnings.join(' '), /another way, such as a router or a VPN/);
});

test('a helper that is not always on is worth saying out loud', () => {
  const { warnings } = checkControllerDocument(document({
    machines: [{ id: 'legion' }, { id: 'tower', wake: { helpers: [helper('legion', 'wake-tower')] } }],
  }));
  assert.match(warnings.join(' '), /not marked alwaysOn/);
});

test('a machine that nothing can wake says so', () => {
  const { warnings } = checkControllerDocument(document({
    machines: [{ id: 'tower', wake: { mac: 'AA:BB:CC:DD:EE:FF' } }],
  }));
  assert.match(warnings.join(' '), /only be woken by a controller that happens to be on its LAN/);
});

// --- private bindings -------------------------------------------------------

test('bindings name where a key is, never what it is', () => {
  assert.deepEqual(schemaErrors(BINDINGS_SCHEMA, {
    deviceName: 'Tower',
    self: { machine: 'tower', system: 'linux' },
    localAgent: { argv: ['node', '/home/me/.legion-control/agent/src/index.mjs'] },
    identityFile: '~/.ssh/legion-control_ed25519',
    knownHostsFile: '~/.config/legion-control/known_hosts',
    currentSite: 'flat',
    machines: { legion: { identityFile: '~/.ssh/id_legion', sshAlias: 'legion-win-lan' } },
  }), []);
  // Nothing that could hold key material has a home here.
  for (const leak of [{ privateKey: 'x' }, { identity: 'ssh-ed25519 AAAA' }, { password: 'x' }]) {
    assert.ok(schemaErrors(BINDINGS_SCHEMA, leak).length > 0, JSON.stringify(leak));
  }
});

test('there is no localAgent.shell', () => {
  // Local execution spawns the argv directly. There is no ssh on that path, so
  // there is nothing for a shell to quote for, and adding one would only put a
  // quoting layer between the client and the agent.
  assert.deepEqual(schemaErrors(BINDINGS_SCHEMA, { localAgent: { argv: ['node', 'agent.mjs'] } }), []);
  assert.ok(schemaErrors(BINDINGS_SCHEMA, { localAgent: { argv: ['node', 'agent.mjs'], shell: 'cmd' } }).length > 0);
});

test('an empty bindings file is valid: a device with none behaves as it did before', () => {
  assert.deepEqual(schemaErrors(BINDINGS_SCHEMA, {}), []);
});
