// The signed bundle verifier: the one place the agent handles bytes somebody
// else chose, so every refusal below is a real attack against a naive extractor.

import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import zlib from 'node:zlib';
import { rejectUnsafePath, readTar, stageFiles, verifyAgentArchive } from '../src/archive.mjs';
import { verifyDetachedSignature, trustFingerprint } from '../src/trust.mjs';
import { REPO, withHome } from './helpers.mjs';

// --- tar writing, so a hostile archive can be built without a hostile tool ----

function block(header) {
  const buffer = Buffer.alloc(512);
  const put = (value, offset, length) => buffer.write(String(value).slice(0, length - 1), offset, 'utf8');
  put(header.name, 0, 100);
  put('0000644', 100, 8);
  put('0000000', 108, 8);
  put('0000000', 116, 8);
  put(`${header.size.toString(8).padStart(11, '0')} `, 124, 12);
  put(`${Math.floor(Date.now() / 1000).toString(8).padStart(11, '0')} `, 136, 12);
  buffer.write('        ', 148, 8, 'utf8');
  buffer.write(header.type ?? '0', 156, 1, 'utf8');
  put(header.link ?? '', 157, 100);
  buffer.write('ustar\0', 257, 6, 'binary');
  buffer.write('00', 263, 2, 'binary');
  let sum = 0;
  for (const byte of buffer) sum += byte;
  buffer.write(`${sum.toString(8).padStart(6, '0')}\0 `, 148, 8, 'utf8');
  return buffer;
}

function tar(entries) {
  const parts = [];
  for (const entry of entries) {
    const body = entry.body ?? Buffer.alloc(0);
    parts.push(block({ name: entry.name, size: body.length, type: entry.type, link: entry.link }));
    if (body.length > 0) {
      parts.push(body, Buffer.alloc((512 - (body.length % 512)) % 512));
    }
  }
  parts.push(Buffer.alloc(1024));
  return Buffer.concat(parts);
}

const sha256 = (bytes) => crypto.createHash('sha256').update(bytes).digest('hex');

/**
 * A bundle signed by a throwaway key. Everything about it is well formed except
 * that the pinned key did not sign it, which is what most of these tests need.
 */
function bundle(files, { manifestOverrides = {}, sign = true } = {}) {
  const entries = [...files];
  const manifest = {
    schema: 1,
    version: '3.0.0',
    contract: 3,
    files: entries
      .filter((entry) => !entry.name.endsWith('MANIFEST.json') && !entry.name.endsWith('.sig') && entry.type !== '5')
      .map((entry) => ({ path: entry.name.replace(/^agent\//, ''), sha256: sha256(entry.body), size: entry.body.length })),
    ...manifestOverrides,
  };
  const manifestBytes = Buffer.from(`${JSON.stringify(manifest, null, 2)}\n`);
  const pair = crypto.generateKeyPairSync('ed25519');
  const signature = sign ? crypto.sign(null, manifestBytes, pair.privateKey).toString('base64') : 'too-short';
  return zlib.gzipSync(
    tar([
      ...entries,
      { name: 'agent/MANIFEST.json', body: manifestBytes },
      { name: 'agent/MANIFEST.json.sig', body: Buffer.from(`${signature}\n`) },
    ]),
  );
}

test('the pinned key verifies the shared golden fixture', () => {
  const dir = path.join(REPO, 'tests', 'fixtures', 'trust');
  const manifest = fs.readFileSync(path.join(dir, 'manifest.json'));
  const signature = fs.readFileSync(path.join(dir, 'manifest.json.sig'), 'utf8');
  assert.equal(verifyDetachedSignature(manifest, signature).ok, true, 'the compiled key must verify the shared fixture');

  const tampered = Buffer.from(manifest);
  tampered[10] ^= 1;
  assert.equal(verifyDetachedSignature(tampered, signature).ok, false, 'one flipped byte must be caught');
  assert.equal(verifyDetachedSignature(manifest, signature.slice(0, 40)).ok, false);
  assert.match(trustFingerprint(), /^[0-9a-f]{16}$/);
});

test('every unsafe path shape is named and refused', () => {
  for (const name of ['/etc/passwd', '../../.ssh/authorized_keys', 'a/../../b', 'C:\\Windows\\x', '\\\\server\\share', '~/x', 'a\0b', 'a//b/', 'a/', 'a//b', 'a:stream', 'CON.txt', 'dir/file.', 'dir/file ']) {
    assert.ok(rejectUnsafePath(name), `${JSON.stringify(name)} should have been refused`);
  }
  assert.equal(rejectUnsafePath('src/index.mjs'), null);
});

test('links, devices and duplicates are refused by the parser', () => {
  assert.throws(() => readTar(tar([{ name: 'agent/x', body: Buffer.from('a'), type: '2', link: '/etc' }])), /is a link/);
  assert.throws(() => readTar(tar([{ name: 'agent/x', body: Buffer.from('a'), type: '3' }])), /not a regular file/);
  assert.throws(
    () => readTar(tar([{ name: 'agent/x', body: Buffer.from('a') }, { name: 'agent/x', body: Buffer.from('b') }])),
    /appears twice/,
  );
  assert.throws(() => readTar(tar([{ name: '../escape', body: Buffer.from('a') }])), /walks out of the archive/);
});

test('a bundle signed by the wrong key is refused before anything is written', async () => {
  await withHome(async (home) => {
    const archive = bundle([{ name: 'agent/src/index.mjs', body: Buffer.from('export default 1;\n') }]);
    const verified = verifyAgentArchive(archive);
    assert.equal(verified.ok, false);
    assert.match(verified.error, /does not match the release key/);
    assert.equal(verified.files, null, 'nothing may be handed back from a bundle that failed verification');
    assert.equal(fs.readdirSync(home).length, 0, 'and nothing may be written');
  });
});

test('a malformed signature is refused for being malformed, not for mismatching', () => {
  const archive = bundle([{ name: 'agent/src/index.mjs', body: Buffer.from('x') }], { sign: false });
  assert.match(verifyAgentArchive(archive).error, /64 base64-encoded bytes/);
});

test('a bundle with no manifest, or one outside agent/, is refused', () => {
  const noManifest = zlib.gzipSync(tar([{ name: 'agent/src/index.mjs', body: Buffer.from('x') }]));
  assert.match(verifyAgentArchive(noManifest).error, /no agent\/MANIFEST\.json/);

  const outside = zlib.gzipSync(tar([{ name: 'elsewhere/x', body: Buffer.from('x') }]));
  assert.match(verifyAgentArchive(outside).error, /outside the "agent\/" directory/);
});

test('a bundle that is not an archive at all is refused', () => {
  for (const rubbish of [Buffer.from('not a tarball'), Buffer.alloc(0), Buffer.from('x'.repeat(2000))]) {
    const verified = verifyAgentArchive(rubbish);
    assert.equal(verified.ok, false);
    assert.equal(verified.files, null);
    assert.ok(verified.error.length > 0, 'a refusal has to say something a person can act on');
  }
  // A gzip member that expands to something that is not a tar at all.
  const notTar = zlib.gzipSync(Buffer.from('x'.repeat(2000)));
  assert.match(verifyAgentArchive(notTar).error, /not a valid tar header|is not a number/);
});

test('staging refuses to write outside its destination', async () => {
  await withHome(async (home) => {
    const files = new Map([['../escape.txt', Buffer.from('x')]]);
    const staged = stageFiles(files, path.join(home, 'agent.new'));
    assert.equal(staged.ok, false);
    assert.equal(fs.existsSync(path.join(home, 'escape.txt')), false);
  });
});

test('staging refuses a destination that already exists', async () => {
  await withHome(async (home) => {
    fs.mkdirSync(path.join(home, 'agent.new'));
    assert.equal(stageFiles(new Map(), path.join(home, 'agent.new')).ok, false);
  });
});

test('a verified bundle stages exactly the files the manifest named', async () => {
  await withHome(async (home) => {
    // A locally signed bundle cannot be verified against the pinned key, so the
    // staging half is exercised directly with the map the verifier would return.
    const files = new Map([
      ['src/index.mjs', Buffer.from('export default 1;\n')],
      ['install/dispatch.sh', Buffer.from('#!/bin/sh\n')],
      ['MANIFEST.json', Buffer.from('{}')],
    ]);
    const staged = stageFiles(files, path.join(home, 'agent.new'));
    assert.equal(staged.ok, true);
    assert.equal(fs.readFileSync(path.join(home, 'agent.new', 'src', 'index.mjs'), 'utf8'), 'export default 1;\n');
    if (process.platform !== 'win32') {
      assert.equal(fs.statSync(path.join(home, 'agent.new', 'install', 'dispatch.sh')).mode & 0o111, 0o111, 'a wrapper has to stay runnable');
    }
  });
});


test('regular files cannot alias directories, trailing slashes, or case-insensitive paths', () => {
  assert.throws(() => readTar(tar([{name: 'agent/a/', body: Buffer.from('x')}])), /empty path segment/);
  assert.throws(() => readTar(tar([{name: 'agent/a', body: Buffer.from('x')}, {name: 'agent/A', body: Buffer.from('y')}])), /twice/);
  assert.throws(() => readTar(tar([{name: 'agent/a', body: Buffer.from('x')}, {name: 'agent/a/', type: '5'}])), /twice/);
  assert.throws(() => readTar(tar([{name: 'agent/a//', type: '5'}])), /empty path segment/);
});
