import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { packageAgent } from '../scripts/package-agent.mjs';
import { createSigningKey, verifyManifest, verifyArtifact } from '../scripts/release-integrity.mjs';

test('agent package authenticates exact inventory and rejects symlink payloads', async () => {
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-package-test-'));
  try {
    const source = path.join(scratch, 'source');
    fs.mkdirSync(path.join(source, 'src'), { recursive: true });
    fs.mkdirSync(path.join(source, 'install'));
    fs.writeFileSync(path.join(source, 'package.json'), '{"version":"3.0.0"}\n');
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
    const archiveFiles = inventory.stdout.trim().split('\n').filter(name => !name.endsWith('/')).sort();
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
