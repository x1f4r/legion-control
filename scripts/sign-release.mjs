#!/usr/bin/env node
// Write the release manifest over every artifact in a directory, sign it, and check what was written.
//
//   node scripts/sign-release.mjs X.Y.Z [artifact-directory] [--commit SHA]
//
// With --commit the manifest also names the commit the release is tagged at, so whoever holds a
// set of artifacts can tell which source they were built from. It is an added key: the Mac decodes
// the manifest into a Decodable struct, the desktop reads the keys it knows by name and Android
// decodes with ignoreUnknownKeys, so all three skip it, and no client decides an update on it.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { artifactDigest, signManifest, verifyManifest, verifyArtifact } from './release-integrity.mjs';
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
// A commit as git rev-parse prints it: the full SHA-1 name, or the full SHA-256 one.
const COMMIT = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/;

/** Every artifact of a release, by the exact name each client selects its own by. */
export const releaseArtifacts = (agentVersion) => ['Legion-Control-macos-arm64.zip', 'Legion-Control-android-arm64.apk', 'Legion-Control-linux-x64.tar.gz', 'Legion-Control-linux-arm64.tar.gz', 'Legion-Control-windows-x64.zip', `legionctl-agent-${agentVersion}.tgz`];

/** Writes, signs and verifies `Legion-Control-manifest.json` in `directory`, and returns it as signed. */
export async function signRelease({ version, commit, directory = path.join(root, 'dist'), agentVersion = JSON.parse(fs.readFileSync(path.join(root, 'agent/package.json'), 'utf8')).version, privateKey = path.join(os.homedir(), '.legion-control/release-signing.key'), publicKey = path.join(root, 'contract/release-public-key.pem') } = {}) {
  if (!/^\d+\.\d+\.\d+$/.test(version ?? '')) throw new Error('Usage: sign-release.mjs X.Y.Z [artifact-directory] [--commit SHA]');
  if (commit !== undefined && !COMMIT.test(commit)) throw new Error(`--commit takes the full SHA of the release commit, not ${JSON.stringify(commit)}.`);
  const artifacts = await Promise.all(releaseArtifacts(agentVersion).map(async name => ({ name, ...await artifactDigest(path.join(directory, name)) })));
  const manifest = path.join(directory, 'Legion-Control-manifest.json');
  fs.writeFileSync(manifest, JSON.stringify({ schema: 1, version, agentVersion, ...(commit === undefined ? {} : { commit }), artifacts }, null, 2) + '\n');
  signManifest(manifest, privateKey, manifest + '.sig');
  const signed = verifyManifest(manifest, manifest + '.sig', publicKey);
  for (const artifact of artifacts) await verifyArtifact(path.join(directory, artifact.name), artifact);
  return signed;
}

// Compared through realpath, as the other release scripts do: Node resolves the symlinks in
// import.meta.url but not in argv[1], so through a symlinked checkout this would sign nothing.
const real = (file) => { try { return fs.realpathSync(file); } catch { return path.resolve(file); } };
if (process.argv[1] && real(process.argv[1]) === real(fileURLToPath(import.meta.url))) {
  try {
    const args = process.argv.slice(2);
    const at = args.indexOf('--commit');
    const commit = at === -1 ? undefined : args.splice(at, 2)[1] ?? '';
    const [version, directory] = args;
    const signed = await signRelease({ version, commit, directory });
    console.log(`Verified ${signed.artifacts.length} signed release artifacts${signed.commit ? ` built from ${signed.commit}` : ''}.`);
  } catch (error) { console.error(error.message); process.exitCode = 1; }
}
