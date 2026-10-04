// The generated files, and the proof that `generate.mjs --check` means
// something.
//
// Everything in contract/fixtures, contract/vectors and contract/hash-vectors.json
// is written by contract/tools/generate.mjs and nothing else. CI runs it with
// --check, and that is the only thing standing between a hand edit of a fixture
// and four clients that test against it. So these tests make each kind of drift
// in a copy of the contract and assert that --check names it, and that the
// generator writes the same bytes on every run.

import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { CONTRACT, discard, runGenerate, scratch, writeJson } from './support.mjs';
import { managedOnDisk } from '../tools/emit.mjs';

/** Break one thing in a copy, and assert --check fails and says `expect`. */
function drift(name, expect, breakIt) {
  test(`--check catches ${name}`, () => {
    const dir = scratch();
    try {
      breakIt(dir);
      const result = runGenerate(dir, ['--check']);
      assert.equal(result.ok, false, `--check did not notice ${name}`);
      assert.match(result.output, expect);
    } finally {
      discard(dir);
    }
  });
}

test('the committed generated files are exactly what the generators produce', () => {
  const result = runGenerate(CONTRACT, ['--check']);
  assert.equal(result.ok, true, result.output);
  assert.match(result.output, /generated files are up to date/);
});

drift('a one-byte edit of a fixture',
  /^stale fixtures\/status\.full\.json \(run node contract\/tools\/generate\.mjs\)$/m, (dir) => {
    const file = path.join(dir, 'fixtures', 'status.full.json');
    fs.writeFileSync(file, fs.readFileSync(file, 'utf8').replace('"hostname": "beacon"', '"hostname": "beacoN"'));
  });

drift('a line-ending change that parses to the same JSON',
  /^stale vectors\/identifiers\.json/m, (dir) => {
    const file = path.join(dir, 'vectors', 'identifiers.json');
    fs.writeFileSync(file, fs.readFileSync(file, 'utf8').replace(/\n/g, '\r\n'));
  });

drift('a stray fixture no generator writes',
  /^extra fixtures\/stray\.json/m, (dir) => {
    writeJson(path.join(dir, 'fixtures', 'stray.json'), { ok: true });
  });

drift('a deleted vector family',
  /^missing vectors\/outcome\.json/m, (dir) => {
    fs.rmSync(path.join(dir, 'vectors', 'outcome.json'));
  });

drift('a deleted hash vector file',
  /^missing hash-vectors\.json/m, (dir) => {
    fs.rmSync(path.join(dir, 'hash-vectors.json'));
  });

drift('a stated want that the reference disagrees with',
  /want disagrees with tools\/outcome\.mjs on "outcome": stated "failed", the reference gives "deferred"/, (dir) => {
    const file = path.join(dir, 'tools', 'vectors', 'outcome.cases.mjs');
    const source = fs.readFileSync(file, 'utf8');
    const find = "reduce('conflict-ok', { ok: true, action: 'conflict', reasonCode: 'operation-in-progress' }, 'deferred',";
    assert.equal(source.split(find).length - 1, 1);
    fs.writeFileSync(file, source.replace(find, find.replace("'deferred',", "'failed',")));
  });

drift('a hash vector whose stated verdict the reference disagrees with',
  /hash\/vectors\/empty: want disagrees with tools\/canonical\.mjs/, (dir) => {
    const file = path.join(dir, 'tools', 'make-hash-vectors.mjs');
    const source = fs.readFileSync(file, 'utf8');
    const find = "    '',\n    { accepted: false, reject: 'empty' },";
    assert.equal(source.split(find).length - 1, 1);
    fs.writeFileSync(file, source.replace(find, "    '',\n    { accepted: false, reject: 'not-a-json-object' },"));
  });

test('a generated file shows every invisible character as an escape', () => {
  // Vectors carry bidirectional overrides, zero-width spaces and byte order
  // marks on purpose. Written raw, a reviewer reading the file sees nothing
  // where they are, and a diff that adds one looks like no change at all.
  const invisible = /[\u007f-\u009f\u00ad\u061c\u180e\u200b-\u200f\u2028-\u202e\u2060-\u206f\ufeff]/;
  for (const relative of managedOnDisk(CONTRACT)) {
    assert.doesNotMatch(fs.readFileSync(path.join(CONTRACT, relative), 'utf8'), invisible, relative);
  }
});

test('a write brings a drifted tree back, and a second run changes nothing', () => {
  const dir = scratch();
  try {
    fs.rmSync(path.join(dir, 'vectors', 'outcome.json'));
    writeJson(path.join(dir, 'fixtures', 'stray.json'), { ok: true });
    const fixture = path.join(dir, 'fixtures', 'busy.idle.json');
    fs.writeFileSync(fixture, '{}\n');

    const first = runGenerate(dir);
    assert.equal(first.ok, true, first.output);
    assert.match(first.output, /^wrote vectors\/outcome\.json$/m);
    assert.match(first.output, /^wrote fixtures\/busy\.idle\.json$/m);
    assert.match(first.output, /^removed fixtures\/stray\.json$/m);

    const second = runGenerate(dir);
    assert.equal(second.ok, true, second.output);
    assert.match(second.output, /^0 written, 0 removed, \d+ unchanged$/m);
    assert.equal(runGenerate(dir, ['--check']).ok, true);
  } finally {
    discard(dir);
  }
});

test('two runs from nothing, in different places and environments, write the same bytes', () => {
  const digest = (dir) => {
    const hash = crypto.createHash('sha256');
    for (const relative of managedOnDisk(dir)) {
      hash.update(`${relative}\n`);
      hash.update(fs.readFileSync(path.join(dir, relative)));
    }
    return hash.digest('hex');
  };
  const wipe = (dir) => {
    for (const relative of managedOnDisk(dir)) fs.rmSync(path.join(dir, relative));
  };
  const one = scratch();
  const two = scratch();
  try {
    wipe(one);
    wipe(two);
    // A different time zone, locale and clock source for the second run: the
    // generators must depend on none of them.
    const env = { ...process.env, TZ: 'Pacific/Chatham', LANG: 'tr_TR.UTF-8', LC_ALL: 'tr_TR.UTF-8' };
    assert.equal(runGenerate(one).ok, true);
    assert.equal(runGenerate(two, [], { env }).ok, true);
    assert.equal(digest(one), digest(two));
    assert.equal(digest(one), digest(CONTRACT), 'a generation from nothing differs from the committed files');
  } finally {
    discard(one);
    discard(two);
  }
});
