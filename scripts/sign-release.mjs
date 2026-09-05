#!/usr/bin/env node
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { artifactDigest, signManifest, verifyManifest, verifyArtifact } from './release-integrity.mjs';
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const [version, directory = path.join(root, 'dist')] = process.argv.slice(2);
try {
  if (!/^\d+\.\d+\.\d+$/.test(version ?? '')) throw new Error('Usage: sign-release.mjs X.Y.Z [artifact-directory]');
  const agentVersion = JSON.parse(fs.readFileSync(path.join(root, 'agent/package.json'), 'utf8')).version;
  const names = ['Legion-Control-macos-arm64.zip', 'Legion-Control-android-arm64.apk', 'Legion-Control-linux-x64.tar.gz', 'Legion-Control-windows-x64.zip', `legionctl-agent-${agentVersion}.tgz`];
  const artifacts = await Promise.all(names.map(async name => ({ name, ...await artifactDigest(path.join(directory, name)) })));
  const manifest = path.join(directory, 'Legion-Control-manifest.json');
  fs.writeFileSync(manifest, JSON.stringify({ schema: 1, version, agentVersion, artifacts }, null, 2) + '\n');
  signManifest(manifest, path.join(os.homedir(), '.legion-control/release-signing.key'), manifest + '.sig');
  verifyManifest(manifest, manifest + '.sig', path.join(root, 'contract/release-public-key.pem'));
  for (const artifact of artifacts) await verifyArtifact(path.join(directory, artifact.name), artifact);
  console.log(`Verified ${artifacts.length} signed release artifacts.`);
} catch (error) { console.error(error.message); process.exitCode = 1; }
