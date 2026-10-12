// The vector framework, and the proof that checking it means something.
//
// Every consumer trusts contract/vectors/index.json to say which groups it
// runs and how many cases it must have run, and trusts each family file to say
// what its production code has to compute. These tests break the index, the
// files, the schemas and the references one at a time in a copy of the
// contract, and assert that validate.mjs notices each break for the reason it
// was made. A family that later joins the index brings its own defects in its
// own vectors-<family>.test.mjs.

import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { CONTRACT, defect, readJson, referenceDefect, writeJson } from './support.mjs';
import { CASE_MODULES, HASH_FAMILY } from '../tools/vectors/families.mjs';
import { loadVectorIndex } from '../validate.mjs';

const index = loadVectorIndex();

test('the index lists the hash family and every family in families.mjs, in that order', () => {
  assert.deepEqual(index.families.map((family) => family.family), [HASH_FAMILY.family, ...CASE_MODULES.map((module) => module.family)]);
  assert.equal(index.contract, 3);
  for (const family of index.families) {
    assert.ok(fs.existsSync(path.join(CONTRACT, family.file)), `${family.file} is missing`);
    assert.ok(fs.existsSync(path.join(CONTRACT, 'schemas', family.schema)), `${family.schema} is missing`);
    assert.ok(fs.existsSync(path.join(CONTRACT, family.reference)), `${family.reference} is missing`);
    assert.ok(fs.existsSync(path.join(CONTRACT, family.generator)), `${family.generator} is missing`);
  }
});

test('every count in the index is the number of cases that name that consumer', () => {
  for (const family of index.families.filter((entry) => entry.layout === 'groups')) {
    const data = readJson(path.join(CONTRACT, family.file));
    for (const group of family.groups) {
      const cases = data.groups[group.group];
      assert.equal(cases.length, group.cases, `${family.family}/${group.group}`);
      for (const [consumer, count] of Object.entries(group.consumers)) {
        assert.equal(cases.filter((entry) => entry.consumers.includes(consumer)).length, count, `${family.family}/${group.group}/${consumer}`);
      }
    }
  }
});

test('every expect key is present on every case of a group', () => {
  for (const family of index.families.filter((entry) => entry.layout === 'groups')) {
    const data = readJson(path.join(CONTRACT, family.file));
    for (const [group, cases] of Object.entries(data.groups)) {
      const keys = Object.keys(cases[0].expect).sort();
      for (const entry of cases) assert.deepEqual(Object.keys(entry.expect).sort(), keys, `${family.family}/${group}/${entry.name}`);
    }
  }
});

// Each code is required once the place that carries it is in the corpus: the
// fixtures always are, a family once the index lists it. A family that joins
// the index without the finding tags it owes fails here on that day.
const TRACEABILITY = {
  t1: 'launch', t5: 'setup-acceptance', t6: 'launch', t11: 'transport',
  c1: 'setup-reconcile', c2: 'outcome', c3: 'setup-acceptance', c4: 'transport', c5: 'outcome',
  c6: 'setup-merge', c7: 'setup-reconcile', c8: 'wake', c9: 'transport', c10: 'identifiers', c13: 'hash',
  m15: 'transport', m16: 'setup-reconcile', a4: 'transport', a9: 'wake', a12: 'transport', d10: 'transport',
  g13: 'fixtures',
};

test('every finding the conformance work answers is pinned by a case or a fixture', () => {
  const tags = new Set();
  for (const entry of readJson(path.join(CONTRACT, 'fixtures', 'index.json')).fixtures) {
    for (const tag of entry.tags) tags.add(tag);
  }
  for (const family of index.families) {
    const data = readJson(path.join(CONTRACT, family.file));
    const cases = family.layout === 'flat' ? data.vectors : Object.values(data.groups).flat();
    for (const entry of cases) for (const tag of entry.tags ?? []) tags.add(tag);
  }
  const present = new Set(['fixtures', ...index.families.map((family) => family.family)]);
  const missing = Object.entries(TRACEABILITY)
    .filter(([code, home]) => present.has(home) && !tags.has(`finding:${code}`))
    .map(([code, home]) => `finding:${code} (owed by ${home})`);
  assert.deepEqual(missing, []);
});

// --- the index ---------------------------------------------------------------

defect('a consumer count that is off by one',
  /a consumer that runs a different number of cases than the index says fails/, (dir) => {
    const file = path.join(dir, 'vectors', 'index.json');
    const data = readJson(file);
    data.families.find((family) => family.family === 'identifiers').groups[0].consumers.mac += 1;
    writeJson(file, data);
  });

defect('a vector file the index does not list',
  /vectors\/extra\.json is on disk and not in the index/, (dir) => {
    writeJson(path.join(dir, 'vectors', 'extra.json'), { family: 'extra', groups: {} });
  });

defect('an index that leaves out the hash vectors',
  /hash-vectors\.json is not listed/, (dir) => {
    const file = path.join(dir, 'vectors', 'index.json');
    const data = readJson(file);
    data.families = data.families.filter((family) => family.family !== 'hash');
    writeJson(file, data);
  });

defect('a family file that is gone',
  /the index names vectors\/outcome\.json, which does not exist/, (dir) => {
    fs.rmSync(path.join(dir, 'vectors', 'outcome.json'));
  });

defect('a missing vector index',
  /vectors\/index\.json: is missing/, (dir) => {
    fs.rmSync(path.join(dir, 'vectors', 'index.json'));
  });

// --- the family files ---------------------------------------------------------

defect('a non-integer in an expect',
  /not a safe integer at expect\.valid: 0\.5/, (dir) => {
    const file = path.join(dir, 'vectors', 'identifiers.json');
    const data = readJson(file);
    data.groups.token[0].expect.valid = 0.5;
    writeJson(file, data);
  });

defect('a case name used twice in a family',
  /the name is used twice in the family/, (dir) => {
    const file = path.join(dir, 'vectors', 'outcome.json');
    const data = readJson(file);
    data.groups.records[0].name = data.groups.reduce[0].name;
    writeJson(file, data);
  });

defect('an expect the reference no longer produces',
  /tools\/outcome\.mjs gives \{"outcome":"deferred"\} and the file says \{"outcome":"failed"\}/, (dir) => {
    const file = path.join(dir, 'vectors', 'outcome.json');
    const data = readJson(file);
    data.groups.reduce.find((entry) => entry.name === 'conflict-ok').expect.outcome = 'failed';
    writeJson(file, data);
  });

defect('a family file that says it is another family',
  /says family "outcome" and the index says "identifiers"/, (dir) => {
    const file = path.join(dir, 'vectors', 'identifiers.json');
    const data = readJson(file);
    data.family = 'outcome';
    writeJson(file, data);
  });

defect('double-bom given the doubly stripped bytes',
  /hash-vectors\.json\/double-bom: the canonical bytes disagree/, (dir) => {
    const file = path.join(dir, 'hash-vectors.json');
    const data = readJson(file);
    const plain = data.vectors.find((vector) => vector.name === 'plain-no-trailing-newline');
    const double = data.vectors.find((vector) => vector.name === 'double-bom');
    double.canonicalBase64 = plain.canonicalBase64;
    writeJson(file, data);
  });

defect('a hash vector refused for the wrong reason',
  /section 12 refuses these bytes first for "invalid-utf8"/, (dir) => {
    const file = path.join(dir, 'hash-vectors.json');
    const data = readJson(file);
    data.vectors.find((vector) => vector.name === 'invalid-utf8').reject = 'not-a-json-object';
    writeJson(file, data);
  });

// --- the schemas and the references --------------------------------------------

defect('the setupId pattern replaced by the token pattern',
  /identifiers\.json\/setupId\/setup-id-slash: tools\/identifiers\.mjs gives \{"valid":true\}/, (dir) => {
    const file = path.join(dir, 'schemas', 'common.schema.json');
    const data = readJson(file);
    data.$defs.setupId.pattern = data.$defs.token.pattern;
    writeJson(file, data);
  });

referenceDefect('outcome.mjs maps conflict to failed', 'tools/outcome.mjs',
  "  if (PENDING_ACTIONS.has(action)) return 'pending';",
  "  if (action === 'conflict') return 'failed';\n  if (PENDING_ACTIONS.has(action)) return 'pending';",
  /conflict-ok: tools\/outcome\.mjs gives \{"outcome":"failed"\}/);

referenceDefect('identifiers.mjs lets a trailing newline through', 'tools/identifiers.mjs',
  "typeof value === 'string' && !/[\\n\\r]/.test(value) && pattern.test(value)",
  "typeof value === 'string' && pattern.test(value.trimEnd())",
  /token-trailing-lf: tools\/identifiers\.mjs gives \{"valid":true\}/);

referenceDefect('canonical.mjs strips every leading byte order mark', 'tools/canonical.mjs',
  'if (text.charCodeAt(0) === 0xfeff) text = text.slice(1);',
  'while (text.charCodeAt(0) === 0xfeff) text = text.slice(1);',
  /double-bom: tools\/canonical\.mjs gives/);
