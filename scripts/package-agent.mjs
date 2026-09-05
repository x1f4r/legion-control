#!/usr/bin/env node
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { artifactDigest, signManifest, verifyManifest, verifyArtifact } from './release-integrity.mjs';

const repository = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
export async function packageAgent({ source = path.join(repository, 'agent'), destination = path.join(repository, 'dist'), privateKey = path.join(os.homedir(), '.legion-control/release-signing.key'), publicKey = path.join(repository, 'contract/release-public-key.pem'), clientVersion } = {}) {
  if (!clientVersion) {
    const project = fs.readFileSync(path.join(repository, 'desktop/LegionControl.Desktop/LegionControl.Desktop.csproj'), 'utf8');
    clientVersion = project.match(/<Version>([^<]+)<\/Version>/)?.[1];
  }
  if (!/^\d+\.\d+\.\d+$/.test(clientVersion ?? '')) throw new Error('Client version must be a stable semantic version.');
  const version = JSON.parse(fs.readFileSync(path.join(source, 'package.json'), 'utf8')).version;
  if (!/^\d+\.\d+\.\d+$/.test(version)) throw new Error('Agent version must be a stable semantic version.');
  if (!fs.existsSync(privateKey)) throw new Error('Release signing key missing. See docs/security.md.');
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-agent-package-'));
  const stage = path.join(scratch, 'agent');
  fs.mkdirSync(stage);
  const files = [];
  try {
    async function copy(relative) {
      const input = path.join(source, relative);
      const stat = fs.lstatSync(input);
      if (stat.isSymbolicLink()) throw new Error(`Agent package cannot contain symlinks: ${relative}`);
      if (stat.isDirectory()) {
        fs.mkdirSync(path.join(stage, relative), { recursive: true });
        for (const name of fs.readdirSync(input).sort()) await copy(path.posix.join(relative, name));
      } else if (stat.isFile()) {
        fs.copyFileSync(input, path.join(stage, relative));
        fs.chmodSync(path.join(stage, relative), stat.mode & 0o111 ? 0o755 : 0o644);
        files.push({ path: relative, ...await artifactDigest(path.join(stage, relative)) });
      } else throw new Error(`Unsupported agent package entry: ${relative}`);
    }
    for (const relative of ['src', 'install', 'package.json']) await copy(relative);
    files.sort((a, b) => a.path.localeCompare(b.path, 'en'));
    const manifest = path.join(stage, 'MANIFEST.json');
    fs.writeFileSync(manifest, JSON.stringify({ schema: 1, version, contract: 3, files }, null, 2) + '\n');
    signManifest(manifest, privateKey, manifest + '.sig');
    verifyManifest(manifest, manifest + '.sig', publicKey);
    for (const file of files) await verifyArtifact(path.join(stage, file.path), file);
    fs.mkdirSync(destination, { recursive: true });
    const name = `legionctl-agent-${version}.tgz`;
    const temporaryArchive = path.join(scratch, name);
    const tar = spawnSync('tar', [...(process.platform === 'darwin' ? ['--no-xattrs', '--no-mac-metadata'] : []), '-czf', temporaryArchive, '-C', scratch, 'agent'], { encoding: 'utf8', env: { ...process.env, COPYFILE_DISABLE: '1' } });
    if (tar.status !== 0) throw new Error(`Agent packaging failed: ${tar.stderr || tar.error}`);
    const artifact = { name, ...await artifactDigest(temporaryArchive) };
    const releaseManifest = path.join(scratch, 'Legion-Control-agent-manifest.json');
    fs.writeFileSync(releaseManifest, JSON.stringify({ schema: 1, version: clientVersion, agentVersion: version, artifacts: [artifact] }, null, 2) + '\n');
    signManifest(releaseManifest, privateKey, releaseManifest + '.sig');
    verifyManifest(releaseManifest, releaseManifest + '.sig', publicKey);
    for (const file of [temporaryArchive, releaseManifest, releaseManifest + '.sig']) fs.copyFileSync(file, path.join(destination, path.basename(file)));
    return { archive: path.join(destination, name), manifest: path.join(destination, path.basename(releaseManifest)), artifact };
  } finally { fs.rmSync(scratch, { recursive: true, force: true }); }
}
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try { console.log(JSON.stringify(await packageAgent(), null, 2)); }
  catch (error) { console.error(error.message); process.exitCode = 1; }
}
