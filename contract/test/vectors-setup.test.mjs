// The setup families: document acceptance, reconciliation and merging (v3.md
// sections 9.7, 9.5 and 9.8).
//
// The reference defects are the ways the clients disagreed before there was
// one function: a site checked against a list that does not exist, endpoints
// without an id counted as duplicates, an applied document thrown away for an
// error instead of shown with it, local intake that skips the checks once it
// has assigned an identity; another setup pushed over because its hash looked
// familiar, a push retried after a refusal no retry can cure, an edit saved
// during a read and then overwritten; a merged entry moved to the end of its
// array, `22.0` printed as `22`, an endpoint without an id dropped, a deletion
// that silently beats an edit, a device whose own document has an error unable
// to merge, an unknown key named like a site taken for the site. Each must be
// caught by validate.mjs and by the generator. The vector-file defects prove that validate.mjs holds every case
// to the schema with its own validator, refuses an agent-enforced case that
// claims to be valid, and checks the reconciliation and merge cases with code
// of its own.

import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { CONTRACT, defect, readJson, referenceDefect, writeJson } from './support.mjs';
import { canonicalHash, rejection } from '../tools/canonical.mjs';
import { ERROR_CODES, INTAKES, WARNING_CODES, checkDocument } from '../tools/document-rules.mjs';
import {
  MERGEABLE_ERRORS, afterPush, decide, decimalKey, entries, gate, jsonEqual, merge, mergedLineage, parseJson, peerState, printJson,
  recordPush, run, topLevelKey,
} from '../tools/setup-sync.mjs';

const acceptance = readJson(path.join(CONTRACT, 'vectors', 'setup-acceptance.json'));
const cases = acceptance.groups.documents;
const named = (name) => cases.find((entry) => entry.name === name);

// --- the reference -------------------------------------------------------------

referenceDefect('a site name checked against an empty sites list', 'tools/document-rules.mjs',
  "typeof machine.site === 'string' && sites.length > 0 && !siteIds.has(machine.site)",
  "typeof machine.site === 'string' && !siteIds.has(machine.site)",
  /site-named-without-sites: tools\/document-rules\.mjs gives/);

referenceDefect('endpoints without an id counted as duplicates', 'tools/document-rules.mjs',
  "if (!isPlainObject(endpoint) || typeof endpoint.id !== 'string') return;",
  'if (!isPlainObject(endpoint)) return;',
  /endpoints-without-ids-are-not-duplicates: tools\/document-rules\.mjs gives/);

referenceDefect('an applied document dropped for its errors', 'tools/document-rules.mjs',
  "keep: intake === 'applied' ? true : valid",
  'keep: valid',
  /applied-version-2: tools\/document-rules\.mjs gives/);

referenceDefect('local intake that stops checking once it assigned an identity', 'tools/document-rules.mjs',
  'const valid = errors.length === 0;',
  'const valid = assignsIdentity || errors.length === 0;',
  /local-identity-then-checked: tools\/document-rules\.mjs gives/);

referenceDefect('the wake alias left out of the helper rules', 'tools/document-rules.mjs',
  'if (wake.helper !== undefined) list.push(wake.helper);',
  '',
  /helper-cycle-through-alias: tools\/document-rules\.mjs gives/);

// --- the committed vectors, against validate.mjs's own reading -----------------

const editCase = (name, change) => (dir) => {
  const file = path.join(dir, 'vectors', 'setup-acceptance.json');
  const data = readJson(file);
  change(data.groups.documents.find((entry) => entry.name === name));
  writeJson(file, data);
};

defect('an agent-enforced case marked valid',
  /documents\/host-comma-star: .*an agent-enforced case must be refused/,
  editCase('host-comma-star', (entry) => { entry.expect = { ...entry.expect, valid: true, errors: [], keep: true }; }));

defect('a schema error the schema does not make',
  /documents\/warn-endpoint-no-id: controller-document\.schema\.json accepts the document and the case says schema/,
  editCase('warn-endpoint-no-id', (entry) => { entry.expect = { ...entry.expect, valid: false, errors: ['schema'], warnings: [], keep: false }; }));

defect('an applied document that section 12 refuses, kept',
  /documents\/applied-not-json: keep is true; under applied intake it is false/,
  editCase('applied-not-json', (entry) => { entry.expect.keep = true; }));

defect('warnings reported beside errors',
  /documents\/schema-bad-mac: .*warnings are only reported for a document without errors/,
  editCase('schema-bad-mac', (entry) => { entry.expect.warnings = ['wake-unplaced']; }));

// --- coverage --------------------------------------------------------------------

test('every error code, warning code and intake of section 9.7 is pinned by a case', () => {
  const errors = new Set(cases.flatMap((entry) => entry.expect.errors));
  const warnings = new Set(cases.flatMap((entry) => entry.expect.warnings));
  assert.deepEqual([...ERROR_CODES].filter((code) => !errors.has(code)), []);
  assert.deepEqual([...WARNING_CODES].filter((code) => !warnings.has(code)), []);
  assert.deepEqual(INTAKES.filter((intake) => !cases.some((entry) => entry.input.intake === intake)), []);
});

test('each warning code has a case in which it is the only problem', () => {
  for (const code of WARNING_CODES) {
    assert.ok(cases.some((entry) => entry.expect.valid && entry.expect.warnings.length === 1 && entry.expect.warnings[0] === code), code);
  }
});

test('each error code has a case in which it is the only problem', () => {
  for (const code of ERROR_CODES) {
    assert.ok(cases.some((entry) => entry.expect.errors.length === 1 && entry.expect.errors[0] === code), code);
  }
});

test('the agent verifies exactly the peer cases', () => {
  for (const entry of cases) assert.equal(entry.consumers.includes('agent'), entry.input.intake === 'peer', entry.name);
});

// Every refused setup-document fixture has a case refused for the same reason,
// so a client that runs the vectors also runs what the fixture is about.
const FIXTURE_CASES = {
  'controller-document.invalid-helper-cycle.json': 'helper-cycle',
  'controller-document.invalid-unknown-site.json': 'unknown-site',
  'controller-document.invalid-self-helper.json': 'helper-self',
  'controller-document.invalid-helper-mismatch.json': 'helper-alias',
  'controller-document.invalid-host.json': 'host-comma-star',
  'controller-document.invalid-host-option.json': 'host-leading-dash',
  'controller-document.invalid-user.json': 'user-substitution',
  'controller-document.invalid-repo.json': 'repo-dotdot',
  'controller-document.invalid-agent-argv.json': 'agent-argv-empty-item',
  'controller-document.invalid-agent-control.json': 'agent-argv-bell-character',
};

test('every refused setup-document fixture has a minimal case refused for the same reason', () => {
  const index = readJson(path.join(CONTRACT, 'fixtures', 'index.json'));
  const refused = index.fixtures.filter((entry) => entry.schema === 'controller-document.schema.json' && entry.expect === 'invalid');
  assert.deepEqual(refused.map((entry) => entry.file).sort(), Object.keys(FIXTURE_CASES).sort());
  for (const entry of refused) {
    const verdict = checkDocument(fs.readFileSync(path.join(CONTRACT, 'fixtures', entry.file)), { intake: 'peer' });
    const mirror = named(FIXTURE_CASES[entry.file]);
    const codes = [...new Set(verdict.errors.map((problem) => problem.code))].sort();
    assert.deepEqual(mirror.expect.errors, codes, entry.file);
    assert.equal(entry.tags.includes('agent-enforced'), mirror.tags.includes('agent-enforced'), entry.file);
  }
});

test('every setup-document fixture is accepted or refused by peer intake exactly as its index entry says', () => {
  const index = readJson(path.join(CONTRACT, 'fixtures', 'index.json'));
  for (const entry of index.fixtures.filter((fixture) => fixture.schema === 'controller-document.schema.json')) {
    const verdict = checkDocument(fs.readFileSync(path.join(CONTRACT, 'fixtures', entry.file)), { intake: 'peer' });
    assert.equal(verdict.valid, entry.expect === 'valid', entry.file);
  }
});

// --- properties ------------------------------------------------------------------

test('peer and local intake refuse what they do not keep, and applied intake keeps what section 12 accepts', () => {
  for (const entry of cases) {
    for (const intake of INTAKES) {
      const verdict = checkDocument(entry.input.text, { intake });
      const kept = intake === 'applied' ? rejection(Buffer.from(entry.input.text, 'utf8')) === null : verdict.valid;
      assert.equal(verdict.keep, kept, `${entry.name} under ${intake}`);
      assert.equal(verdict.valid, verdict.errors.length === 0);
      if (verdict.errors.length > 0) assert.deepEqual(verdict.warnings, []);
    }
  }
});

test('a problem names where it is, and a duplicate member is reported alone', () => {
  for (const entry of cases) {
    const verdict = checkDocument(entry.input.text, { intake: entry.input.intake });
    for (const problem of [...verdict.errors, ...verdict.warnings]) {
      assert.equal(typeof problem.path, 'string');
      assert.ok(problem.message.length > 10, `${entry.name}: ${problem.code}`);
    }
    if (verdict.errors.some((problem) => problem.code === 'duplicate-member')) assert.equal(verdict.errors.length, 1, entry.name);
  }
  assert.equal(checkDocument(named('duplicate-member').input.text).errors[0].path, 'machines[0].systems[0].agent');
});

test('an error inside a machine names that machine, so applied intake can disable it alone', () => {
  for (const name of ['duplicate-system', 'duplicate-endpoint', 'helper-self', 'host-comma-star', 'agent-argv-empty-item']) {
    const verdict = checkDocument(named(name).input.text);
    assert.ok(verdict.errors.every((problem) => /^machines\[\d+\]/.test(problem.path)), `${name}: ${verdict.errors.map((problem) => problem.path)}`);
  }
});

// ===========================================================================
// Reconciliation (section 9.5).
// ===========================================================================

const reconcile = readJson(path.join(CONTRACT, 'vectors', 'setup-reconcile.json'));

referenceDefect('rules 7 and 8 swapped, so another setup is pushed over', 'tools/setup-sync.mjs',
  "  if (id !== local.id) return r('differentSetup');\n  if (local.lineage.includes(s.hash)) return r('push', 'machine-behind');",
  "  if (local.lineage.includes(s.hash)) return r('push', 'machine-behind');\n  if (id !== local.id) return r('differentSetup');",
  /different-setup-even-if-in-lineage: tools\/setup-sync\.mjs gives/);

referenceDefect('the head checked after the bytes, so an edit saved during the read can be overwritten', 'tools/setup-sync.mjs',
  "  if (currentLocalHash !== expected.localHash) return 'headMoved';\n  if (fetched.hash !== expected.remoteHash) return fetched.replyHash === fetched.hash ? 'moved' : 'unverifiable';",
  "  if (fetched.hash !== expected.remoteHash) return fetched.replyHash === fetched.hash ? 'moved' : 'unverifiable';\n  if (currentLocalHash !== expected.localHash) return 'headMoved';",
  /head-moved-first: tools\/setup-sync\.mjs gives/);

referenceDefect('the held gate dropped, so a refused push is retried every poll', 'tools/setup-sync.mjs',
  "    if (own(state.held, localHash)) return { action: 'hold', reason: 'held' };\n",
  '',
  /push-held-after-refusal: tools\/setup-sync\.mjs gives/);

referenceDefect('a back-off that still holds at exactly 60 s', 'tools/setup-sync.mjs',
  'now - failed < BACKOFF_MS',
  'now - failed <= BACKOFF_MS',
  /push-backoff-over: tools\/setup-sync\.mjs gives/);

referenceDefect('reads held for a running operation like pushes', 'tools/setup-sync.mjs',
  "  if (decision === 'push') {\n    if (state.localInvalid)",
  '  {\n    if (state.localInvalid)',
  /read-meta-during-operation: tools\/setup-sync\.mjs gives/);

const editReconcile = (group, name, change) => (dir) => {
  const file = path.join(dir, 'vectors', 'setup-reconcile.json');
  const data = readJson(file);
  change(data.groups[group].find((entry) => entry.name === name));
  writeJson(file, data);
};

defect('a fetch that expects another hash than the status reported',
  /setup-reconcile\.json\/decide\/fetch: .*a fetch expects the hash the status reported/,
  editReconcile('decide', 'fetch', (entry) => { entry.expect.expect = entry.input.local.hash; }));

defect('a second request let out while one is in flight',
  /setup-reconcile\.json\/gate\/in-flight: .*a second request while one is in flight/,
  editReconcile('gate', 'in-flight', (entry) => { entry.expect = { action: 'act', reason: null }; }));

defect('a refusal current filed that says nothing about what the machine holds',
  /setup-reconcile\.json\/pushReply\/push-bad-argument: .*only the current of a stale-revision or controller-conflict refusal is filed/,
  editReconcile('pushReply', 'push-bad-argument', (entry) => { entry.expect = { effect: 'fileMeta', metaHash: entry.input.reply.current.hash }; }));

test('every decision, reason, admission, gate reason and push effect is pinned by a case', () => {
  const seen = (group, key) => new Set(reconcile.groups[group].map((entry) => entry.expect[key]));
  assert.deepEqual([...seen('decide', 'decision')].sort(), ['diverged', 'differentSetup', 'fetch', 'inSync', 'push', 'readMeta', 'unknown', 'unsupported'].sort());
  assert.deepEqual([...seen('decide', 'reason')].sort(), ['local-unidentified', 'machine-behind', 'machine-empty', 'machine-unidentified', 'no-local', null].sort());
  assert.deepEqual([...seen('admit', 'admission')].sort(), ['adopt', 'headMoved', 'inconsistent', 'invalid', 'moved', 'unverifiable']);
  assert.deepEqual([...seen('gate', 'reason')].sort(), ['backoff', 'held', 'in-flight', 'local-invalid', 'operation-running', 'settled', null].sort());
  assert.deepEqual([...seen('pushReply', 'effect')].sort(), ['clear', 'fail', 'fileMeta', 'hold', 'locked']);
});

test('a push reply case that names a config set fixture carries exactly that fixture', () => {
  let named = 0;
  for (const entry of reconcile.groups.pushReply) {
    const file = /config\.set\.[a-z-]+\.json/.exec(entry.purpose)?.[0];
    if (file === undefined) continue;
    named += 1;
    assert.deepEqual(entry.input.reply, readJson(path.join(CONTRACT, 'fixtures', file)), entry.name);
  }
  assert.ok(named >= 8, 'the push replies are the config set fixtures, not replies written for the vectors');
});

test('an automatic write only ever follows strict descent', () => {
  for (const entry of reconcile.groups.decide) {
    const { local, status, metaByHash } = entry.input;
    const remote = status?.controller?.hash;
    if (entry.expect.decision === 'fetch') assert.ok(metaByHash[remote].lineage.includes(local.hash), entry.name);
    if (entry.expect.reason === 'machine-behind') assert.ok(local.lineage.includes(remote), entry.name);
  }
});

test('a push reply turns into the state change section 9.5 names', () => {
  const now = 5_000_000;
  const localHash = 'a'.repeat(64);
  const start = { ...peerState(), inFlight: true, failedAt: { [localHash]: now - 1_000 } };
  const after = (reply) => recordPush(start, afterPush(reply, localHash, now), { reply, localHash, now });
  assert.deepEqual(after({ ok: true, hash: localHash }).failedAt, {});
  assert.equal(after(null).failedAt[localHash], now);
  assert.equal(after({ ok: false, reasonCode: 'lock-held' }).lockedAt, now);
  assert.equal(after({ ok: false, reasonCode: 'restricted' }).held[localHash], 'restricted');
  const current = { id: null, lineage: [], hash: 'b'.repeat(64) };
  assert.deepEqual(after({ ok: false, reasonCode: 'stale-revision', current }).meta[current.hash], current);
  for (const reply of [null, { ok: true, hash: localHash }]) assert.equal(after(reply).inFlight, false);
  // A held push stays held for that hash only, and the gate reads it.
  const held = after({ ok: false, reasonCode: 'config-invalid' });
  assert.equal(gate({ decision: 'push', localHash, remoteHash: null, agentVersion: '3.1.0', now: now + 120_000, state: held }).reason, 'held');
  assert.equal(gate({ decision: 'push', localHash: 'c'.repeat(64), remoteHash: null, agentVersion: '3.1.0', now: now + 120_000, state: held }).action, 'act');
});

test('a meta answer filed under its hash is what the next decision reads', () => {
  const local = { hash: 'a'.repeat(64), id: 'setup-1', lineage: [] };
  const status = { contract: 3, controller: { hash: 'b'.repeat(64), id: 'setup-1' } };
  assert.equal(decide({ local, status, metaByHash: {} }).decision, 'readMeta');
  const filed = { [status.controller.hash]: { id: 'setup-1', lineage: [local.hash] } };
  assert.deepEqual(decide({ local, status, metaByHash: filed }), { decision: 'fetch', reason: null, expect: status.controller.hash });
  // A hash that only looks like a property of every object is just a key.
  assert.equal(decide({ local, status: { contract: 3, controller: { hash: 'constructor', id: 'setup-1' } }, metaByHash: {} }).decision, 'readMeta');
});

// ===========================================================================
// Merging (section 9.8).
// ===========================================================================

const mergeFamily = readJson(path.join(CONTRACT, 'vectors', 'setup-merge.json'));
const merges = mergeFamily.groups.merges;

referenceDefect('an entry taken from theirs moved to the end of its array', 'tools/setup-sync.mjs',
  '  else array.items[index] = clone(value);',
  '  else {\n    array.items.splice(index, 1);\n    array.items.push(clone(value));\n  }',
  /insert-after-predecessor: tools\/setup-sync\.mjs gives/);

referenceDefect('22.0 printed as 22', 'tools/setup-sync.mjs',
  "    case 'number':\n      return node.lexeme;",
  "    case 'number':\n      return String(Number(node.lexeme));",
  /number-spelling: tools\/setup-sync\.mjs gives/);

referenceDefect('an endpoint without an id dropped from the merge', 'tools/setup-sync.mjs',
  'if (endpointId === null) idless.push(endpoint);',
  'if (endpointId === null) continue;',
  /theirs-adds-idless-endpoint: tools\/setup-sync\.mjs gives/);

referenceDefect('a deletion that silently beats an edit', 'tools/setup-sync.mjs',
  "  if (jsonEqual(found.mine, found.base)) return 'theirs';",
  "  if (found.theirs === undefined || jsonEqual(found.mine, found.base)) return 'theirs';",
  /theirs-deletes-what-mine-edited: tools\/setup-sync\.mjs gives/);

referenceDefect('the lineages interleaved theirs first', 'tools/setup-sync.mjs',
  '    if (index < mineLineage.length) add(mineLineage[index]);\n    if (index < theirsLineage.length) add(theirsLineage[index]);',
  '    if (index < theirsLineage.length) add(theirsLineage[index]);\n    if (index < mineLineage.length) add(mineLineage[index]);',
  /long-lineages: tools\/setup-sync\.mjs gives/);

referenceDefect('mine held to peer intake, so a device whose own document has an error cannot merge', 'tools/setup-sync.mjs',
  "side !== 'mine' || !MERGEABLE_ERRORS.includes(problem.code)",
  'true',
  /mine-with-error-repaired: the reference throws: mine breaks section 9\.7/);

referenceDefect('an unknown top-level key taken for the entry it is named like', 'tools/setup-sync.mjs',
  "add(topLevelKey(name), { scope: 'top', name }, value)",
  "add(name, { scope: 'top', name }, value)",
  /top-level-keys-like-entries: the reference throws: two entries of one document are both called/);

const editMerge = (group, name, change) => (dir) => {
  const file = path.join(dir, 'vectors', 'setup-merge.json');
  const data = readJson(file);
  change(data.groups[group].find((entry) => entry.name === name));
  writeJson(file, data);
};

defect('a merge result section 9.7 refuses',
  /setup-merge\.json\/merges\/both-sides-applied: .*the merged document is refused by section 9\.7: helper-unknown/,
  editMerge('merges', 'both-sides-applied', (entry) => {
    entry.expect.resultText = entry.expect.resultText.replace('"machine": "pi"', '"machine": "ghost"');
  }));

defect('a merge from a mine whose entries are undefined',
  /setup-merge\.json\/merges\/mine-with-error-repaired: .*mineText breaks section 9\.7 \(duplicate-machine\)/,
  editMerge('merges', 'mine-with-error-repaired', (entry) => {
    const mine = JSON.parse(entry.input.mineText);
    mine.machines.push(structuredClone(mine.machines[0]));
    entry.input.mineText = `${JSON.stringify(mine, null, 2)}\n`;
  }));

defect('a head hash that is not the hash of its text',
  /setup-merge\.json\/merges\/reorder-only: .*mineHash is not the canonical hash of mineText/,
  editMerge('merges', 'reorder-only', (entry) => { entry.input.mineHash = entry.input.theirsHash; }));

defect('two spellings of one number called two numbers',
  /setup-merge\.json\/numbers\/integer-and-decimal: 22 and 22\.0 are one number/,
  editMerge('numbers', 'integer-and-decimal', (entry) => { entry.expect.equal = false; }));

test('every kind of difference is pinned, and so is a refused result', () => {
  const kinds = new Set(merges.flatMap((entry) => entry.expect.differences.map((difference) => difference.kind)));
  assert.deepEqual([...kinds].sort(), ['both', 'mine', 'noBase', 'theirs']);
  assert.ok(merges.some((entry) => entry.expect.resultText === null && entry.expect.error !== null));
  assert.ok(merges.some((entry) => entry.expect.differences.length === 0));
});

// What section 9.8 promises, checked through the exported entries rather than
// the case tables: every entry of the result holds the side the person chose,
// or the side its kind gives, and every entry that did not differ holds mine.
function assertTakenFromChosenSides(input, result, label) {
  const parse = (text) => parseJson(text);
  const base = input.baseText === null ? null : parse(input.baseText);
  const mine = parse(input.mineText);
  const theirs = parse(input.theirsText);
  const written = parse(result.resultText);
  const kinds = new Map(result.differences.map((difference) => [difference.entry, difference.kind]));
  const chosen = new Map();
  for (const found of entries(mine, theirs, base)) {
    const kind = kinds.get(found.entry);
    const side = kind === undefined ? 'mine' : (input.choices[found.entry] ?? (kind === 'mine' ? 'mine' : 'theirs'));
    chosen.set(found.entry, side === 'mine' ? found.mine : found.theirs);
  }
  // Read the result's entries against mine and theirs in turn, so a machine
  // either side lacks is read whole and one both have is read part by part.
  const fromResult = new Map();
  for (const other of [mine, theirs]) {
    for (const found of entries(written, other, null)) if (!fromResult.has(found.entry)) fromResult.set(found.entry, found.mine);
  }
  for (const [entry, value] of chosen) {
    if (!fromResult.has(entry) && value === undefined) continue;
    assert.ok(jsonEqual(fromResult.get(entry), value), `${label}: ${entry}`);
  }
}

test('every entry of a merged document holds the side that was chosen for it', () => {
  for (const entry of merges.filter((candidate) => candidate.expect.resultText !== null)) {
    assertTakenFromChosenSides(entry.input, entry.expect, entry.name);
  }
});

test('answering every entry mine gives mine, and every entry theirs gives theirs, under the merged identity', () => {
  const identity = new Set(['id', 'revision', 'updatedAt', 'source', 'device', 'lineage']);
  const content = (text) => {
    const document = JSON.parse(text);
    for (const key of identity) delete document.controller[key];
    return document;
  };
  const sameContent = (a, b) => entries(parseJson(JSON.stringify(content(a))), parseJson(JSON.stringify(content(b)))).every((found) => jsonEqual(found.mine, found.theirs));
  for (const entry of merges) {
    for (const side of ['mine', 'theirs']) {
      const first = merge(entry.input);
      const choices = Object.fromEntries(first.differences.map((difference) => [difference.entry, side]));
      const result = merge({ ...entry.input, choices });
      if (result.resultText === null) continue;
      assert.ok(sameContent(result.resultText, entry.input[`${side}Text`]), `${entry.name}, every entry ${side}`);
      assertTakenFromChosenSides({ ...entry.input, choices }, result, `${entry.name}, every entry ${side}`);
      const lineage = JSON.parse(result.resultText).controller.lineage;
      assert.deepEqual(lineage.slice(0, 2), [entry.input.mineHash, entry.input.theirsHash], entry.name);
    }
  }
});

test('a document printed from its parse is the document, number spelling included', () => {
  for (const entry of merges) {
    for (const text of [entry.input.baseText, entry.input.mineText, entry.input.theirsText, entry.expect.resultText]) {
      if (text !== null) assert.equal(printJson(parseJson(text)), text, entry.name);
    }
  }
  assert.throws(() => parseJson('{"a": 1, "a": 2}'), /repeated/);
  assert.throws(() => parseJson('{"a": 1} x'), /after the JSON value/);
});

test('number equality is exact decimal equality, whatever a double says', () => {
  for (const entry of mergeFamily.groups.numbers) assert.equal(run('numbers', entry.input).equal, entry.expect.equal, entry.name);
  assert.equal(decimalKey('-0.000'), '0');
  assert.equal(decimalKey('1.50e+3'), '15e2');
  assert.equal(decimalKey('1e999999999999999999999'), '1e999999999999999999999');
  assert.notEqual(decimalKey('0.1'), decimalKey('0.10000000000000001'));
});

test('the merged lineage holds both heads, then both lineages interleaved, and never more than 32', () => {
  const h = (n) => n.toString(16).padStart(64, '0');
  assert.deepEqual(mergedLineage(h(1), h(2), [h(3), h(5)], [h(4), h(5), h(6)]), [h(1), h(2), h(3), h(4), h(5), h(6)]);
  assert.equal(mergedLineage(h(1), h(2), Array.from({ length: 32 }, (_, i) => h(100 + i)), Array.from({ length: 32 }, (_, i) => h(200 + i))).length, 32);
});

test('theirs and the base are merged only as peer intake accepts them, and mine as applied intake keeps it', () => {
  const entry = merges[0];
  assert.throws(() => merge({ ...entry.input, theirsText: '{"version": 2, "machines": []}' }), /theirs breaks section 9\.7/);
  assert.throws(() => merge({ ...entry.input, baseText: '{"version": 2, "machines": []}' }), /base breaks section 9\.7/);
  // A mine that breaks a relationship rule is merged: the vectors pin what
  // comes of it. One whose entries are undefined, or text section 12 refuses,
  // is not.
  assert.ok(merges.some((candidate) => !checkDocument(candidate.input.mineText, { intake: 'peer' }).valid && candidate.expect.resultText !== null));
  const twice = JSON.parse(entry.input.mineText);
  twice.machines.push(structuredClone(twice.machines[0]));
  assert.throws(() => merge({ ...entry.input, mineText: `${JSON.stringify(twice, null, 2)}\n` }), /mine breaks section 9\.7 \(duplicate-machine\)/);
  assert.throws(() => merge({ ...entry.input, mineText: '{"version": 1,' }), /mine breaks section 9\.7 \(schema\)/);
  assert.deepEqual(MERGEABLE_ERRORS.filter((code) => !ERROR_CODES.includes(code)), []);
});

test('a top-level key named like another entry is quoted, so every key names one entry', () => {
  assert.equal(topLevelKey('notes'), 'notes');
  assert.equal(topLevelKey('controllers'), 'controllers');
  assert.equal(topLevelKey('sites[home]'), '"sites[home]"');
  assert.equal(topLevelKey('"sites[home]"'), '""sites[home]""');
  // Every structural key a document could mimic, beside the part it mimics.
  const entry = merges[0];
  const theirs = JSON.parse(entry.input.theirsText);
  for (const name of ['controller.name', 'sites[home]', 'machines[pi]', 'machines[tower].fields', 'machines[tower].endpoints[lan]', 'machines[tower].endpoints[]']) theirs[name] = 'mimic';
  const theirsText = `${JSON.stringify(theirs, null, 2)}\n`;
  assert.equal(checkDocument(theirsText, { intake: 'peer' }).valid, true);
  const result = merge({ ...entry.input, theirsText, theirsHash: canonicalHash(theirsText) });
  const quoted = result.differences.filter((difference) => difference.entry.startsWith('"'));
  assert.deepEqual(quoted, ['"controller.name"', '"machines[pi]"', '"machines[tower].endpoints[]"', '"machines[tower].endpoints[lan]"', '"machines[tower].fields"', '"sites[home]"']
    .map((key) => ({ entry: key, kind: 'theirs' })));
  assert.equal(JSON.parse(result.resultText)['sites[home]'], 'mimic');
});
