// The canonical byte rule, tested as a rule rather than as a table.
//
// contract/validate.mjs already recomputes every vector in hash-vectors.json.
// What it cannot show is WHY the rule is written the way it is, so these tests
// assert the properties: that the operation is idempotent, that the trim set is
// the fixed ASCII one and not the language's, and that the cases with no hash
// really have none.

import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { canonicalBytes, canonicalHash, isValidUtf8, TRIM_CODE_POINTS } from '../tools/canonical.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const vectors = JSON.parse(fs.readFileSync(path.join(HERE, '..', 'hash-vectors.json'), 'utf8'));

const DOC = '{"version":1,"machines":[]}';
const NBSP = '\u00a0';
const BOM = '\ufeff';

test('the trim set is the six ASCII characters and nothing else', () => {
  assert.deepEqual(TRIM_CODE_POINTS, ['\u0009', '\u000a', '\u000b', '\u000c', '\u000d', '\u0020']);
});

test('a non-breaking space is not whitespace here, whatever the language thinks', () => {
  // This is the whole reason the set is written out. JavaScript's String.trim
  // strips U+00A0; if the implementation used it, this document would hash the
  // same as the plain one and the four sides would disagree the first time
  // somebody's keyboard produced a non-breaking space.
  const withNbsp = `${NBSP}${DOC}`;
  assert.notEqual(canonicalHash(withNbsp), canonicalHash(DOC));
  assert.equal(canonicalBytes(withNbsp).toString('utf8'), `${NBSP}${DOC}\n`);
  assert.equal(withNbsp.trim(), DOC, 'if this fails, String.trim changed and the point of the test moved');
});

test('U+FEFF is stripped only at the very start', () => {
  assert.equal(canonicalHash(`${BOM}${DOC}`), canonicalHash(DOC));
  const inside = `{"a":"x${BOM}y"}`;
  assert.ok(canonicalBytes(inside).toString('utf8').includes(BOM));
});

test('every line ending collapses to one LF and the result carries exactly one', () => {
  const body = '{\r\n  "version": 1,\r  "machines": []\r\n}\r\n\r\n\r\n';
  const out = canonicalBytes(body).toString('utf8');
  assert.ok(!out.includes('\r'));
  assert.ok(out.endsWith('}\n'));
  assert.ok(!out.endsWith('\n\n'));
});

test('canonicalising is idempotent, so a stored document rehashes to itself', () => {
  // The agent stores the canonical bytes. If a second pass changed them, the
  // hash the machine reports would drift away from the hash it stored.
  for (const vector of vectors.vectors) {
    if (vector.sha256 === null) continue;
    const once = canonicalBytes(Buffer.from(vector.inputBase64, 'base64'));
    const twice = canonicalBytes(once);
    assert.equal(twice.toString('base64'), once.toString('base64'), vector.name);
    assert.equal(crypto.createHash('sha256').update(once).digest('hex'), vector.sha256, vector.name);
  }
});

test('invalid UTF-8 is refused rather than hashed', () => {
  const bad = Buffer.from([0x7b, 0x22, 0x76, 0x80, 0x22, 0x7d]);
  assert.equal(isValidUtf8(bad), false);
  assert.throws(() => canonicalHash(bad), /not valid UTF-8/);
});

test('lossy decoding would give two different documents the same hash', () => {
  // Why the refusal above matters: these are different byte strings, and a
  // decoder that replaced the bad byte with U+FFFD would hash them alike.
  const a = Buffer.from([0x7b, 0x22, 0x76, 0x80, 0x22, 0x7d]);
  const b = Buffer.from([0x7b, 0x22, 0x76, 0x81, 0x22, 0x7d]);
  assert.notEqual(a.toString('base64'), b.toString('base64'));
  assert.equal(a.toString('utf8'), b.toString('utf8'));
  assert.equal(isValidUtf8(a), false);
  assert.equal(isValidUtf8(b), false);
});

test('ten different encodings of one document share one hash', () => {
  const groups = new Map();
  for (const vector of vectors.vectors) {
    if (vector.sha256 === null) continue;
    groups.set(vector.sha256, [...(groups.get(vector.sha256) ?? []), vector.name]);
  }
  const biggest = [...groups.values()].sort((a, b) => b.length - a.length)[0];
  assert.ok(biggest.length >= 10, `only ${biggest.length} encodings agree: ${biggest.join(', ')}`);
  for (const name of ['plain-no-trailing-newline', 'crlf', 'cr-only', 'bom', 'many-trailing-newlines']) {
    assert.ok(biggest.includes(name), `${name} should hash the same as the plain document`);
  }
});

test('the vectors that carry no hash are the ones that cannot have one', () => {
  const unhashed = vectors.vectors.filter((vector) => vector.sha256 === null);
  assert.ok(unhashed.length > 0);
  for (const vector of unhashed) {
    assert.equal(vector.reject, 'invalid-utf8');
    assert.equal(vector.accepted, false);
    assert.equal(isValidUtf8(Buffer.from(vector.inputBase64, 'base64')), false, vector.name);
  }
});

test('a document that canonicalises to something unparseable is refused, not repaired', () => {
  const withRawCr = '{"version":1,"machines":[],"note":"a\rb"}';
  const canonical = canonicalBytes(withRawCr).toString('utf8');
  // The CR became a newline, and a raw newline inside a JSON string is as
  // illegal as the CR was. Normalisation must not be mistaken for a fix.
  assert.throws(() => JSON.parse(canonical));
});

test('internal whitespace is content: reformatting a document is a real edit', () => {
  const compact = '{"version":1,"machines":[]}';
  const pretty = '{\n  "version": 1,\n  "machines": []\n}';
  assert.notEqual(canonicalHash(compact), canonicalHash(pretty));
});
