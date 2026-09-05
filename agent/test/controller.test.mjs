// The setup document: one canonical byte form, and ancestry rather than
// revision numbers deciding what may replace what.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { canonicalBytes, canonicalHash, judgeIncoming, readControllerBlock, readController, readControllerMeta, storeController } from '../src/controller.mjs';
import { REPO, fresh, withHome } from './helpers.mjs';

const vectors = JSON.parse(fs.readFileSync(path.join(REPO, 'contract', 'hash-vectors.json'), 'utf8'));

test('every shared hash vector agrees with this implementation', () => {
  assert.ok(vectors.vectors.length >= 20, 'the shared vectors should be present');
  for (const vector of vectors.vectors) {
    const input = Buffer.from(vector.inputBase64, 'base64');
    if (vector.sha256 === null) {
      assert.throws(() => canonicalHash(input), new RegExp('UTF-8'), `${vector.name} should have no hash`);
      continue;
    }
    assert.equal(canonicalHash(input), vector.sha256, vector.name);
    assert.equal(canonicalBytes(input).toString('base64'), vector.canonicalBase64, `${vector.name} bytes`);
  }
});

test('a document with no final newline hashes the same as one with it', () => {
  const document = '{"version":1,"machines":[]}';
  assert.equal(canonicalHash(document), canonicalHash(`${document}\n`));
  assert.equal(canonicalHash(document), canonicalHash(`\n\n${document}\n\n\n`));
  assert.equal(canonicalHash(document), canonicalHash(`﻿${document}`));
  assert.equal(canonicalHash(document), canonicalHash(document.replace(/\n/g, '\r\n')));
});

test('the controller block is read and validated, and nothing else in the document is', () => {
  const good = readControllerBlock({
    version: 1,
    machines: [{ id: 'x' }],
    controller: { id: 'setup-a', revision: 3, lineage: ['a'.repeat(64)], source: 'phone', device: 'Pixel' },
  });
  assert.deepEqual(good.problems, []);
  assert.equal(good.id, 'setup-a');
  assert.equal(good.revision, 3);
  assert.equal(good.device, 'Pixel');

  for (const block of [
    { id: 'setup-a', revision: 1, lineage: ['nothex'] },
    { id: 'setup-a', revision: 1, lineage: ['a'.repeat(64), 'a'.repeat(64)] },
    { id: 'setup-a', revision: 1.5 },
    { id: 'setup-a', revision: 1, source: 'android' },
    { id: 'a b', revision: 1 },
  ]) {
    const parsed = readControllerBlock({ version: 1, machines: [], controller: block });
    assert.ok(parsed.problems.length > 0, `${JSON.stringify(block)} should have been rejected`);
  }
});

// The six acceptance rules, as pure decisions.
const held = { id: 'setup-a', revision: 5, hash: 'h5'.padEnd(64, '0'), lineage: ['h4'.padEnd(64, '0')] };

test('rule 1: anything valid beats holding nothing', () => {
  assert.equal(judgeIncoming({ id: null, revision: 0, hash: null, lineage: [] }, { id: 'x', revision: 1, hash: 'a', lineage: [] }).action, 'stored');
});

test('rule 2: a different setup is a question, not a version', () => {
  const refused = judgeIncoming(held, { id: 'setup-b', revision: 9, hash: 'z', lineage: [] });
  assert.equal(refused.ok, false);
  assert.equal(refused.reasonCode, 'controller-conflict');
  assert.equal(judgeIncoming(held, { id: 'setup-b', revision: 9, hash: 'z', lineage: [], replace: true }).action, 'replaced');
});

test('rule 3: the same bytes again are a no-op, not an error', () => {
  assert.equal(judgeIncoming(held, { id: 'setup-a', revision: 5, hash: held.hash, lineage: [] }).action, 'noop');
});

test('rule 4: a document that descends from what we hold fast-forwards', () => {
  assert.equal(judgeIncoming(held, { id: 'setup-a', revision: 6, hash: 'h6', lineage: [held.hash] }).action, 'stored');
  // ...but a descendant has to carry a higher revision than its parent.
  const wrong = judgeIncoming(held, { id: 'setup-a', revision: 5, hash: 'h6', lineage: [held.hash] });
  assert.equal(wrong.ok, false);
  assert.equal(wrong.reasonCode, 'bad-argument');
});

test('rule 5: a peer offering one of our ancestors is behind', () => {
  const refused = judgeIncoming(held, { id: 'setup-a', revision: 4, hash: held.lineage[0], lineage: [] });
  assert.equal(refused.ok, false);
  assert.equal(refused.reasonCode, 'stale-revision');
});

test('rule 6: two edits from the same base diverge, and nothing is chosen', () => {
  const refused = judgeIncoming(held, { id: 'setup-a', revision: 6, hash: 'other', lineage: ['h4'.padEnd(64, '0')] });
  assert.equal(refused.ok, false);
  assert.equal(refused.reasonCode, 'controller-conflict');
  assert.equal(refused.divergent, true, 'a client has to be able to tell divergence from being behind');
});

test('a revision number alone never decides acceptance', () => {
  // Higher revision, unrelated ancestry: this is the case revision numbers get
  // wrong and it must not fast-forward.
  const refused = judgeIncoming(held, { id: 'setup-a', revision: 99, hash: 'x', lineage: ['unrelated'.padEnd(64, '0')] });
  assert.equal(refused.ok, false);
  assert.equal(refused.divergent, true);
});

// ---------------------------------------------------------------------------
// On disk
// ---------------------------------------------------------------------------

const document = (id, revision, name, lineage = []) =>
  JSON.stringify(
    { version: 1, controller: { id, revision, name: 'Home', source: 'cli', device: 'test', lineage }, machines: [{ id: 'm', name }] },
    null,
    2,
  );

test('storing writes the canonical bytes and the metadata as one transaction', async () => {
  await withHome(async (home) => {
    const first = document('setup-a', 1, 'alpha');
    const stored = storeController(first, { id: 'setup-a', revision: 1 });
    assert.equal(stored.ok, true);
    assert.equal(stored.action, 'stored');

    const onDisk = fs.readFileSync(path.join(home, 'controller.json'));
    assert.deepEqual(onDisk, canonicalBytes(first), 'the stored bytes are the canonical ones');

    const meta = readControllerMeta();
    assert.equal(meta.hash, stored.hash);
    assert.equal(meta.id, 'setup-a');
    assert.equal(meta.revision, 1);
    assert.equal(readController().hash, stored.hash, 'the document and its metadata always agree');
  });
});

test('the flags and the document have to say the same thing', async () => {
  await withHome(async () => {
    const refused = storeController(document('setup-a', 1, 'alpha'), { id: 'setup-b', revision: 1 });
    assert.equal(refused.ok, false);
    assert.match(refused.error, /--controller-id is "setup-b" but the document says "setup-a"/);

    const mismatch = storeController(document('setup-a', 1, 'alpha'), { id: 'setup-a', revision: 7 });
    assert.equal(mismatch.ok, false);
    assert.match(mismatch.error, /--revision is 7 but the document says 1/);

    const half = storeController(document('setup-a', 1, 'alpha'), { id: 'setup-a', revision: null });
    assert.equal(half.ok, false);
    assert.match(half.error, /go together/);
  });
});

test('a full publish, fast-forward, stale and divergent sequence on disk', async () => {
  await withHome(async () => {
    const one = document('setup-a', 1, 'alpha');
    storeController(one, { id: 'setup-a', revision: 1 });
    const hashOne = canonicalHash(one);

    const two = document('setup-a', 2, 'beta', [hashOne]);
    assert.equal(storeController(two, { id: 'setup-a', revision: 2 }).action, 'stored');
    const hashTwo = canonicalHash(two);

    // The same push again after a cut link.
    assert.equal(storeController(two, { id: 'setup-a', revision: 2 }).action, 'noop');

    // A peer that is behind.
    const behind = storeController(one, { id: 'setup-a', revision: 1 });
    assert.equal(behind.reasonCode, 'stale-revision');

    // A peer that edited the same base offline.
    const alsoThree = document('setup-a', 3, 'delta', [hashOne]);
    const three = document('setup-a', 3, 'gamma', [hashTwo, hashOne]);
    assert.equal(storeController(three, { id: 'setup-a', revision: 3 }).action, 'stored');
    const diverged = storeController(alsoThree, { id: 'setup-a', revision: 3 });
    assert.equal(diverged.reasonCode, 'controller-conflict');
    assert.equal(diverged.divergent, true);
    assert.equal(readControllerMeta().revision, 3);
    assert.equal(readController().document.machines[0].name, 'gamma', 'the loser must not have overwritten anything');
  });
});

test('a document that is not a controller config is refused before anything is written', async () => {
  await withHome(async (home) => {
    for (const bad of ['not json', '[]', '{"version":1}', '{"machines":[]}']) {
      const refused = storeController(bad, { id: 'setup-a', revision: 1 });
      assert.equal(refused.ok, false, `${bad} should have been refused`);
    }
    assert.equal(fs.existsSync(path.join(home, 'controller.json')), false);
  });
});

test('an interrupted write is served consistently without a reader completing its writes', async () => {
  await withHome(async (home) => {
    storeController(document('setup-a', 1, 'alpha'), { id: 'setup-a', revision: 1 });

    // Exactly the state a process killed between the two renames leaves behind.
    const next = canonicalBytes(document('setup-a', 2, 'beta', [canonicalHash(document('setup-a', 1, 'alpha'))]));
    fs.writeFileSync(path.join(home, 'controller.json.staged'), next);
    fs.writeFileSync(
      path.join(home, 'controller.commit.json'),
      JSON.stringify({ meta: { id: 'setup-a', revision: 2, updatedAt: new Date().toISOString(), source: 'cli', hash: canonicalHash(next), lineage: [canonicalHash(document('setup-a', 1, 'alpha'))], device: 'test', bytes: next.length } }),
    );

    const controller = await fresh('controller.mjs');
    const read = controller.readController();
    assert.equal(read.document.machines[0].name, 'beta', 'the committed staged document is readable before its renames finish');
    assert.equal(read.meta.revision, 2);
    assert.equal(read.hash, read.meta.hash, 'the document and metadata must never disagree');
    assert.equal(fs.existsSync(path.join(home, 'controller.commit.json')), true);
    assert.equal(JSON.parse(fs.readFileSync(path.join(home, 'controller.json'), 'utf8')).machines[0].name, 'alpha');
  });
});
