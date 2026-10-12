import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import zlib from 'node:zlib';
import { spawnSync } from 'node:child_process';
import { ARCHIVE_MTIME, packageAgent, reproducibleTarball } from '../scripts/package-agent.mjs';
import { createSigningKey, verifyManifest, verifyArtifact } from '../scripts/release-integrity.mjs';
import { readTar } from '../agent/src/archive.mjs';

test('agent package authenticates exact inventory and rejects symlink payloads', async () => {
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-package-test-'));
  try {
    const source = path.join(scratch, 'source');
    fs.mkdirSync(path.join(source, 'src'), { recursive: true });
    fs.mkdirSync(path.join(source, 'install'));
    fs.writeFileSync(path.join(source, 'package.json'), '{"version":"3.0.0","legionControl":{"agentFloor":"3.0.0"}}\n');
    fs.writeFileSync(path.join(source, 'src/index.mjs'), 'console.log("test agent");\n');
    fs.writeFileSync(path.join(source, 'install/install.sh'), '#!/bin/sh\nexit 0\n', { mode: 0o755 });
    const privateKey = path.join(scratch, 'private.pem');
    const publicKey = path.join(scratch, 'public.pem');
    createSigningKey(privateKey, publicKey);
    const options = { source, clientVersion: '1.3.0', destination: path.join(scratch, 'dist'), privateKey, publicKey };
    const result = await packageAgent(options);
    const release = verifyManifest(result.manifest, result.manifest + '.sig', publicKey);
    assert.equal(release.agentVersion, '3.0.0');
    await verifyArtifact(result.archive, release.artifacts[0]);
    const inventory = spawnSync('tar', ['-tzf', path.basename(result.archive)], { cwd: path.dirname(result.archive), encoding: 'utf8' });
    assert.equal(inventory.status, 0, inventory.stderr);
    const archiveFiles = inventory.stdout.trim().split(/\r?\n/).filter(name => !name.endsWith('/')).sort();
    assert.deepEqual(archiveFiles, ['agent/MANIFEST.json', 'agent/MANIFEST.json.sig', 'agent/install/install.sh', 'agent/package.json', 'agent/src/index.mjs']);
    const extracted = path.join(scratch, 'extracted');
    fs.mkdirSync(extracted);
    const tar = spawnSync('tar', ['-xzf', path.relative(extracted, result.archive).split(path.sep).join('/')], { cwd: extracted });
    assert.equal(tar.status, 0, tar.stderr?.toString());
    const manifestPath = path.join(extracted, 'agent/MANIFEST.json');
    const manifest = verifyManifest(manifestPath, manifestPath + '.sig', publicKey);
    assert.deepEqual(manifest.files.map(file => file.path), ['install/install.sh', 'package.json', 'src/index.mjs']);
    for (const file of manifest.files) await verifyArtifact(path.join(extracted, 'agent', file.path), file);
    fs.writeFileSync(path.join(extracted, 'agent/src/index.mjs'), 'tampered');
    await assert.rejects(verifyArtifact(path.join(extracted, 'agent/src/index.mjs'), manifest.files.find(file => file.path === 'src/index.mjs')));
    if (process.platform !== 'win32') {
      fs.symlinkSync('/tmp', path.join(source, 'src/escape'));
      await assert.rejects(packageAgent(options), /symlinks/);
    }
  } finally { fs.rmSync(scratch, { recursive: true, force: true }); }
});

test('agent package refuses a floor that is missing, malformed or above the version before it signs anything', async () => {
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-package-floor-test-'));
  try {
    const source = path.join(scratch, 'source');
    fs.mkdirSync(path.join(source, 'src'), { recursive: true });
    fs.mkdirSync(path.join(source, 'install'));
    fs.writeFileSync(path.join(source, 'src/index.mjs'), 'console.log("test agent");\n');
    // No signing key at all: the floor is refused first, so nothing is staged, signed or written.
    const options = { source, clientVersion: '1.4.0', destination: path.join(scratch, 'dist'), privateKey: path.join(scratch, 'absent.pem') };
    const floors = [[undefined, /declares no legionControl\.agentFloor/], ['3.1', /"3\.1", which is not an x\.y\.z version/], ['3.2.0', /3\.2\.0, above its own version 3\.1\.0/]];
    for (const [agentFloor, refusal] of floors) {
      fs.writeFileSync(path.join(source, 'package.json'), JSON.stringify({ version: '3.1.0', legionControl: { agentFloor } }) + '\n');
      await assert.rejects(packageAgent(options), (error) => refusal.test(error.message) && /Nothing was packaged/.test(error.message));
      assert.equal(fs.existsSync(options.destination), false);
    }
    for (const agentFloor of ['3.1.0', '3.0.0']) {
      fs.writeFileSync(path.join(source, 'package.json'), JSON.stringify({ version: '3.1.0', legionControl: { agentFloor } }) + '\n');
      await assert.rejects(packageAgent(options), /Release signing key missing/);
    }
  } finally { fs.rmSync(scratch, { recursive: true, force: true }); }
});

// The fields of a ustar header the reproducible tarball fixes, read the way tar reads them.
function tarEntries(archive) {
  const tar = zlib.gunzipSync(archive);
  const field = (header, offset, length) => header.subarray(offset, offset + length).toString('utf8').replace(/\0.*$/s, '');
  const entries = [];
  for (let offset = 0; offset + 512 <= tar.length;) {
    const header = tar.subarray(offset, offset + 512);
    if (header.every(byte => byte === 0)) break;
    const size = parseInt(field(header, 124, 12), 8);
    const prefix = field(header, 345, 155);
    entries.push({
      name: (prefix ? prefix + '/' : '') + field(header, 0, 100),
      mode: parseInt(field(header, 100, 8), 8),
      uid: parseInt(field(header, 108, 8), 8),
      gid: parseInt(field(header, 116, 8), 8),
      mtime: parseInt(field(header, 136, 12), 8),
      type: field(header, 156, 1),
      owner: field(header, 265, 32) + field(header, 297, 32),
    });
    offset += 512 + Math.ceil(size / 512) * 512;
  }
  return entries;
}

test('two builds of one agent tree are the same bytes, whenever its files were written', async () => {
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-package-reproducible-'));
  try {
    const source = path.join(scratch, 'source');
    for (const directory of ['src/providers', 'install']) fs.mkdirSync(path.join(source, directory), { recursive: true });
    fs.writeFileSync(path.join(source, 'package.json'), '{"version":"3.1.0","legionControl":{"agentFloor":"3.1.0"}}\n');
    fs.writeFileSync(path.join(source, 'src/index.mjs'), 'console.log("test agent");\n');
    fs.writeFileSync(path.join(source, 'src/providers/app.mjs'), 'export {};\n');
    fs.writeFileSync(path.join(source, 'install/install.sh'), '#!/bin/sh\nexit 0\n', { mode: 0o755 });
    const privateKey = path.join(scratch, 'private.pem');
    const publicKey = path.join(scratch, 'public.pem');
    createSigningKey(privateKey, publicKey);
    const build = (name) => packageAgent({ source, clientVersion: '1.4.0', destination: path.join(scratch, name), privateKey, publicKey });
    const first = await build('first');
    // What a second checkout of the same commit looks like: every file written at another time.
    const later = new Date(Date.now() + 86_400_000);
    for (const file of ['package.json', 'src/index.mjs', 'src/providers/app.mjs', 'install/install.sh', 'src/providers', 'src', 'install']) fs.utimesSync(path.join(source, file), later, later);
    const second = await build('second');
    for (const file of ['legionctl-agent-3.1.0.tgz', 'Legion-Control-agent-manifest.json', 'Legion-Control-agent-manifest.json.sig']) {
      assert.deepEqual(fs.readFileSync(path.join(scratch, 'second', file)), fs.readFileSync(path.join(scratch, 'first', file)), file);
    }
    assert.deepEqual(second.artifact, first.artifact);

    const archive = fs.readFileSync(first.archive);
    // gzip: no time, no file name, and the same operating system byte from any build host.
    assert.deepEqual([...archive.subarray(0, 4)], [0x1f, 0x8b, 8, 0]);
    assert.equal(archive.readUInt32LE(4), 0);
    assert.equal(archive[9], 3);
    const entries = tarEntries(archive);
    assert.deepEqual(entries.map(entry => entry.name), [
      'agent/', 'agent/MANIFEST.json', 'agent/MANIFEST.json.sig', 'agent/install/', 'agent/install/install.sh',
      'agent/package.json', 'agent/src/', 'agent/src/index.mjs', 'agent/src/providers/', 'agent/src/providers/app.mjs',
    ]);
    for (const entry of entries) {
      assert.deepEqual({ uid: entry.uid, gid: entry.gid, owner: entry.owner, mtime: entry.mtime }, { uid: 0, gid: 0, owner: '', mtime: ARCHIVE_MTIME }, entry.name);
      // Windows keeps no executable bit for a file to carry over, so there only directories have one.
      const executable = entry.type === '5' || (entry.name === 'agent/install/install.sh' && process.platform !== 'win32');
      assert.equal(entry.mode, executable ? 0o755 : 0o644, entry.name);
    }

    // The agent reads a bundle with its own parser, never with tar, so that parser has to agree.
    const { files, directories } = readTar(zlib.gunzipSync(archive));
    assert.deepEqual([...files.keys()].sort(), ['agent/MANIFEST.json', 'agent/MANIFEST.json.sig', 'agent/install/install.sh', 'agent/package.json', 'agent/src/index.mjs', 'agent/src/providers/app.mjs']);
    assert.equal(files.get('agent/src/index.mjs').toString(), 'console.log("test agent");\n');
    assert.deepEqual(directories, ['agent', 'agent/install', 'agent/src', 'agent/src/providers']);
  } finally { fs.rmSync(scratch, { recursive: true, force: true }); }
});

test('the signed agent manifest lists its files in byte order, which no locale has a say in', async () => {
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-package-order-'));
  try {
    const source = path.join(scratch, 'source');
    for (const directory of ['src', 'install']) fs.mkdirSync(path.join(source, directory), { recursive: true });
    fs.writeFileSync(path.join(source, 'package.json'), '{"version":"3.1.0","legionControl":{"agentFloor":"3.1.0"}}\n');
    // A collation puts alpha before Zeta; their bytes put Zeta first.
    for (const name of ['alpha.mjs', 'Zeta.mjs']) fs.writeFileSync(path.join(source, 'src', name), 'export {};\n');
    fs.writeFileSync(path.join(source, 'install/install.sh'), '#!/bin/sh\nexit 0\n', { mode: 0o755 });
    const privateKey = path.join(scratch, 'private.pem');
    const publicKey = path.join(scratch, 'public.pem');
    createSigningKey(privateKey, publicKey);
    const result = await packageAgent({ source, clientVersion: '1.4.0', destination: path.join(scratch, 'dist'), privateKey, publicKey });
    const { files } = readTar(zlib.gunzipSync(fs.readFileSync(result.archive)));
    const manifest = JSON.parse(files.get('agent/MANIFEST.json').toString('utf8'));
    assert.deepEqual(manifest.files.map(file => file.path), ['install/install.sh', 'package.json', 'src/Zeta.mjs', 'src/alpha.mjs']);
  } finally { fs.rmSync(scratch, { recursive: true, force: true }); }
});

test('a path longer than a ustar name is split at a slash, and one that cannot be is refused', () => {
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-package-long-'));
  try {
    const deep = path.join('src', 'a'.repeat(60), 'b'.repeat(60));
    fs.mkdirSync(path.join(scratch, deep), { recursive: true });
    fs.writeFileSync(path.join(scratch, deep, 'module.mjs'), 'export {};\n');
    const names = tarEntries(reproducibleTarball(scratch, 'agent')).map(entry => entry.name);
    assert.ok(names.includes(`agent/src/${'a'.repeat(60)}/${'b'.repeat(60)}/module.mjs`), names.join('\n'));
    fs.writeFileSync(path.join(scratch, 'c'.repeat(120)), '');
    assert.throws(() => reproducibleTarball(scratch, 'agent'), /too long for a ustar header/);
  } finally { fs.rmSync(scratch, { recursive: true, force: true }); }
});
