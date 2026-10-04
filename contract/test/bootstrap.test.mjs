// The legacy bootstrap scripts (contract/bootstrap).
//
// A 2.x agent cannot install a signed 3.x tree by itself, so a client uploads
// the archive and runs two short scripts with the agent's own interpreter:
// node -e PREPARE -- BASE, then node -e INSTALL -- BASE ARCHIVE SHA256 SIZE OP.
// The Mac and Android carry them as string constants and compare those with
// these files byte for byte, which is why the files hold exactly the text node
// is given: no comment, no final newline. These tests run them as a client
// does, against a scratch base, to show what the two copies are agreeing on:
// that PREPARE refuses a base with no agent in it, and that INSTALL checks the
// size and the hash of the upload before tar ever reads it.

import test from 'node:test';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { CONTRACT } from './support.mjs';

const PREPARE = fs.readFileSync(path.join(CONTRACT, 'bootstrap', 'prepare.cjs'), 'utf8');
const INSTALL = fs.readFileSync(path.join(CONTRACT, 'bootstrap', 'install.cjs'), 'utf8');
// Some tar builds on a Windows PATH read C:\... as a remote host. The phone
// bootstraps POSIX systems only, so that is where the unpacking is exercised.
const POSIX = process.platform !== 'win32';

/** Run a script as a client does: node -e SCRIPT -- ARGS. */
function run(script, args) {
  return spawnSync(process.execPath, ['-e', script, '--', ...args], { encoding: 'utf8', timeout: 60000 });
}

function base() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-bootstrap-'));
  fs.mkdirSync(path.join(root, 'agent', 'src'), { recursive: true });
  fs.writeFileSync(path.join(root, 'agent', 'src', 'index.mjs'), '// the installed 2.x agent\n');
  return root;
}

/** A signed-tree stand-in: an agent whose entry prints what it was asked to do. */
function archive(directory) {
  const tree = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-tree-'));
  fs.mkdirSync(path.join(tree, 'agent', 'src'), { recursive: true });
  fs.writeFileSync(path.join(tree, 'agent', 'src', 'index.mjs'),
    "process.stdout.write(JSON.stringify({ ok: true, argv: process.argv.slice(2), home: process.env.LEGIONCTL_HOME }));\n");
  const file = path.join(directory, 'incoming', 'legion-bootstrap-0a1b2c3d.tgz');
  const packed = spawnSync('tar', ['-czf', file, '-C', tree, 'agent'], { encoding: 'utf8' });
  fs.rmSync(tree, { recursive: true, force: true });
  assert.equal(packed.status, 0, packed.stderr);
  const bytes = fs.readFileSync(file);
  return { file, size: String(bytes.length), sha256: crypto.createHash('sha256').update(bytes).digest('hex') };
}

test('the scripts are exactly what node is given: no final newline, no carriage return', () => {
  for (const script of [PREPARE, INSTALL]) {
    assert.ok(!script.endsWith('\n'));
    assert.ok(!script.includes('\r'));
    assert.match(script, /^const fs=require\('node:fs'\)/);
  }
});

test('prepare refuses a base that holds no installed agent', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-bootstrap-'));
  try {
    const result = run(PREPARE, [root]);
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /No installed agent exists in this base; bootstrap refused/);
    assert.equal(fs.existsSync(path.join(root, 'incoming')), false);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('prepare makes the incoming directory, and refuses an incoming tree left from an earlier try', () => {
  const root = base();
  try {
    const first = run(PREPARE, [root]);
    assert.equal(first.status, 0, first.stderr);
    assert.deepEqual(JSON.parse(first.stdout), { ok: true });
    assert.ok(fs.statSync(path.join(root, 'incoming')).isDirectory());
    fs.mkdirSync(path.join(root, 'incoming', 'agent'));
    const again = run(PREPARE, [root]);
    assert.notEqual(again.status, 0);
    assert.match(again.stderr, /An incoming agent tree already exists/);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('install refuses an upload whose size or hash is not the signed one, before tar runs', () => {
  const root = base();
  try {
    assert.equal(run(PREPARE, [root]).status, 0);
    // Not even an archive: if tar ran, it would fail with its own message.
    const bytes = Buffer.from('not a tarball, and never read as one\n');
    const upload = { file: path.join(root, 'incoming', 'legion-bootstrap-0a1b2c3d.tgz'), size: String(bytes.length), sha256: crypto.createHash('sha256').update(bytes).digest('hex') };
    fs.writeFileSync(upload.file, bytes);
    const shorter = run(INSTALL, [root, upload.file, upload.sha256, String(Number(upload.size) - 1), '0a1b2c3d']);
    assert.notEqual(shorter.status, 0);
    assert.match(shorter.stderr, /Uploaded archive size does not match/);
    const tampered = run(INSTALL, [root, upload.file, '0'.repeat(64), upload.size, '0a1b2c3d']);
    assert.notEqual(tampered.status, 0);
    assert.match(tampered.stderr, /Uploaded archive hash does not match/);
    assert.equal(fs.existsSync(path.join(root, 'incoming', 'agent')), false, 'tar ran on an upload that was never verified');
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('install unpacks a verified upload and runs the new agent\'s own install with the base as its home', { skip: !POSIX && 'tar on Windows cannot be relied on to read a drive path' }, () => {
  const root = base();
  try {
    assert.equal(run(PREPARE, [root]).status, 0);
    const upload = archive(root);
    const result = run(INSTALL, [root, upload.file, upload.sha256, upload.size, '0a1b2c3d']);
    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(JSON.parse(result.stdout), {
      ok: true,
      argv: ['self-update', '--install', '--op', '0a1b2c3d'],
      home: root,
    });
    const again = run(INSTALL, [root, upload.file, upload.sha256, upload.size, '0a1b2c3d']);
    assert.notEqual(again.status, 0);
    assert.match(again.stderr, /An incoming agent tree already exists; nothing was replaced/);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
