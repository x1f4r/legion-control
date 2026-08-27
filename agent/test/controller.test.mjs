// The controller config the machines carry for the apps: what is stored, what is
// refused, and whether the hash always describes the bytes on disk.

import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';

// controller.json follows LEGIONCTL_HOME, so point it at a throwaway directory
// before anything that might write is imported.
const HOME = fs.mkdtempSync(path.join(os.tmpdir(), 'legionctl-controller-'));
process.env.LEGIONCTL_HOME = HOME;

const { MAX_CONTROLLER_BYTES, controllerPath, readController, storeController } = await import(
  '../src/controller.mjs'
);

test.after(() => fs.rmSync(HOME, { recursive: true, force: true }));

const DOCUMENT = {
  version: 1,
  machines: [
    {
      id: 'legion',
      name: 'Legion',
      ssh: { host: 'legion' },
      systems: [{ id: 'cachyos', name: 'CachyOS', platform: 'linux', agent: ['/usr/bin/node', '/home/me/x.mjs'] }],
    },
  ],
};

function clear() {
  fs.rmSync(controllerPath(), { force: true });
}

function sha256OfFile() {
  return crypto.createHash('sha256').update(fs.readFileSync(controllerPath())).digest('hex');
}

test('the path is controller.json under the install base', () => {
  assert.equal(controllerPath(), path.join(HOME, 'controller.json'));
});

test('nothing stored reads as nothing, with no hash to compare against', () => {
  clear();
  assert.deepEqual(readController(), { document: null, raw: null, hash: null });
});

test('a stored document comes back as it went in, and the hash matches the file', () => {
  clear();
  const text = JSON.stringify(DOCUMENT, null, 2);
  const stored = storeController(text);
  assert.equal(stored.ok, true);
  assert.equal(stored.error, null);
  assert.equal(stored.bytes, Buffer.byteLength(`${text}\n`, 'utf8'));

  // The bytes on disk are exactly what was given, trimmed, with one newline.
  assert.equal(fs.readFileSync(controllerPath(), 'utf8'), `${text}\n`);
  assert.equal(stored.hash, sha256OfFile());

  const read = readController();
  assert.deepEqual(read.document, DOCUMENT);
  assert.equal(read.raw, `${text}\n`);
  assert.equal(read.hash, stored.hash);
  assert.equal(read.error, undefined);
});

test('surrounding whitespace is trimmed and the hash follows the stored bytes', () => {
  clear();
  const text = JSON.stringify(DOCUMENT);
  const stored = storeController(`\n\n  ${text}  \n\n`);
  assert.equal(stored.ok, true);
  assert.equal(fs.readFileSync(controllerPath(), 'utf8'), `${text}\n`);
  assert.equal(stored.hash, sha256OfFile());
  assert.notEqual(stored.hash, crypto.createHash('sha256').update(text, 'utf8').digest('hex'));
});

test('storing again replaces the document and the hash changes with it', () => {
  clear();
  const first = storeController(JSON.stringify(DOCUMENT));
  const second = storeController(JSON.stringify({ ...DOCUMENT, version: 2 }));
  assert.equal(second.ok, true);
  assert.notEqual(second.hash, first.hash);
  assert.equal(readController().document.version, 2);
  assert.equal(readController().hash, sha256OfFile());
});

test('a document that is not a controller config is refused and nothing is written', () => {
  clear();
  const refusals = [
    ['', /does not parse/],
    ['{ not json', /does not parse/],
    ['[]', /not a JSON object/],
    ['"a string"', /not a JSON object/],
    ['null', /not a JSON object/],
    [JSON.stringify({ machines: [] }), /"version" number/],
    [JSON.stringify({ version: '1', machines: [] }), /"version" number/],
    [JSON.stringify({ version: 1 }), /"machines" array/],
    [JSON.stringify({ version: 1, machines: {} }), /"machines" array/],
  ];
  for (const [text, expected] of refusals) {
    const stored = storeController(text);
    assert.equal(stored.ok, false, `expected ${JSON.stringify(text)} to be refused`);
    assert.equal(stored.hash, null);
    assert.equal(stored.bytes, 0);
    assert.match(stored.error, expected);
  }
  assert.equal(fs.existsSync(controllerPath()), false);
  assert.equal(storeController(DOCUMENT).ok, false);
});

test('a document over 1 MB is refused before it is parsed', () => {
  clear();
  const oversize = JSON.stringify({
    version: 1,
    machines: [{ id: 'x', name: 'x'.repeat(MAX_CONTROLLER_BYTES) }],
  });
  assert.ok(Buffer.byteLength(oversize, 'utf8') > MAX_CONTROLLER_BYTES);
  const stored = storeController(oversize);
  assert.equal(stored.ok, false);
  assert.match(stored.error, /larger than 1048576 bytes/);
  assert.equal(fs.existsSync(controllerPath()), false);
});

test('a good document is not replaced by a bad one', () => {
  clear();
  const good = storeController(JSON.stringify(DOCUMENT));
  assert.equal(storeController('{ truncated').ok, false);
  assert.equal(readController().hash, good.hash);
  assert.deepEqual(readController().document, DOCUMENT);
});

test('a stored file that does not parse reads as null, with an error and its hash', () => {
  clear();
  fs.writeFileSync(controllerPath(), '{ half a document', 'utf8');
  const read = readController();
  assert.equal(read.document, null);
  assert.equal(read.raw, '{ half a document');
  // The hash still describes the bytes that are there, so the Mac sees a
  // mismatch and pushes over them rather than leaving them to sit.
  assert.equal(read.hash, sha256OfFile());
  assert.match(read.error, /does not parse/);

  fs.writeFileSync(controllerPath(), '[1, 2, 3]\n', 'utf8');
  const array = readController();
  assert.equal(array.document, null);
  assert.equal(array.hash, sha256OfFile());
  assert.match(array.error, /not a JSON object/);
});

test('the temp file is not left behind', () => {
  clear();
  storeController(JSON.stringify(DOCUMENT));
  assert.deepEqual(
    fs.readdirSync(HOME).filter((entry) => entry.startsWith('controller.json')),
    ['controller.json'],
  );
});
