// The setup families. Document acceptance (v3.md section 9.7) is here first;
// reconciliation and merging (sections 9.5 and 9.8) join it in this file.
//
// The reference defects are the ways the clients disagreed before there was
// one function: a site checked against a list that does not exist, endpoints
// without an id counted as duplicates, an applied document thrown away for an
// error instead of shown with it, local intake that skips the checks once it
// has assigned an identity. Each must be caught by validate.mjs and by the
// generator. The vector-file defects prove that validate.mjs holds every case
// to the schema with its own validator and refuses an agent-enforced case that
// claims to be valid.

import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { CONTRACT, defect, readJson, referenceDefect, writeJson } from './support.mjs';
import { rejection } from '../tools/canonical.mjs';
import { ERROR_CODES, INTAKES, WARNING_CODES, checkDocument } from '../tools/document-rules.mjs';

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
