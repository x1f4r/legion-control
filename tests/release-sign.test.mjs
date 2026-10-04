import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { createSigningKey, verifyArtifact, verifyManifest } from '../scripts/release-integrity.mjs';
import { releaseArtifacts, signRelease } from '../scripts/sign-release.mjs';

const repository = path.resolve(import.meta.dirname, '..');
const COMMIT = '0123456789abcdef0123456789abcdef01234567';

// A dist/ with every artifact of a release in it, and a key pair to sign them with.
function release(t) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-release-sign-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const directory = path.join(root, 'dist');
  fs.mkdirSync(directory);
  for (const name of releaseArtifacts('3.1.0')) fs.writeFileSync(path.join(directory, name), `the bytes of ${name}`);
  const privateKey = path.join(root, 'private.pem');
  const publicKey = path.join(root, 'public.pem');
  createSigningKey(privateKey, publicKey);
  return { root, directory, options: { version: '1.4.0', agentVersion: '3.1.0', directory, privateKey, publicKey } };
}
const manifestOf = (directory) => path.join(directory, 'Legion-Control-manifest.json');

test('the signed release manifest names the commit the release is tagged at', async (t) => {
  const { directory, options } = release(t);
  const signed = await signRelease({ ...options, commit: COMMIT });
  assert.equal(signed.commit, COMMIT);
  // Added beside the keys every client already reads, which keep their places.
  assert.deepEqual(Object.keys(signed), ['schema', 'version', 'agentVersion', 'commit', 'artifacts']);
  assert.deepEqual(verifyManifest(manifestOf(directory), manifestOf(directory) + '.sig', options.publicKey), signed);
  assert.deepEqual(signed.artifacts.map(artifact => artifact.name), releaseArtifacts('3.1.0'));
  for (const artifact of signed.artifacts) await verifyArtifact(path.join(directory, artifact.name), artifact);

  // Signing is deterministic, so a run that has to sign again after a failure writes the same bytes.
  const bytes = fs.readFileSync(manifestOf(directory));
  const signature = fs.readFileSync(manifestOf(directory) + '.sig');
  await signRelease({ ...options, commit: COMMIT });
  assert.deepEqual(fs.readFileSync(manifestOf(directory)), bytes);
  assert.deepEqual(fs.readFileSync(manifestOf(directory) + '.sig'), signature);
});

test('without a commit the manifest is the one clients have always read', async (t) => {
  const { options } = release(t);
  const signed = await signRelease(options);
  assert.deepEqual(Object.keys(signed), ['schema', 'version', 'agentVersion', 'artifacts']);
});

test('a commit that is not a full SHA, or a missing artifact, signs nothing', async (t) => {
  const { directory, options } = release(t);
  for (const commit of ['', 'HEAD', COMMIT.slice(0, 12), COMMIT.toUpperCase(), COMMIT + '\n']) {
    await assert.rejects(signRelease({ ...options, commit }), /--commit takes the full SHA of the release commit/);
  }
  fs.rmSync(path.join(directory, 'Legion-Control-linux-arm64.tar.gz'));
  await assert.rejects(signRelease({ ...options, commit: COMMIT }), /Legion-Control-linux-arm64\.tar\.gz/);
  assert.equal(fs.existsSync(manifestOf(directory)), false);
});

test('the command takes the commit after the version and the directory', (t) => {
  const { root, directory } = release(t);
  // The command signs with the key in ~/.legion-control and checks against the repository's own
  // public key, so it runs in a copy of the scripts with a home and a key of its own.
  const checkout = path.join(root, 'checkout');
  fs.mkdirSync(path.join(checkout, 'scripts'), { recursive: true });
  fs.mkdirSync(path.join(checkout, 'agent'));
  fs.mkdirSync(path.join(checkout, 'contract'));
  for (const script of ['sign-release.mjs', 'release-integrity.mjs']) fs.copyFileSync(path.join(repository, 'scripts', script), path.join(checkout, 'scripts', script));
  fs.writeFileSync(path.join(checkout, 'agent', 'package.json'), '{"version":"3.1.0"}\n');
  const home = path.join(root, 'home');
  createSigningKey(path.join(home, '.legion-control', 'release-signing.key'), path.join(checkout, 'contract', 'release-public-key.pem'));
  const run = (...args) => spawnSync(process.execPath, [path.join(checkout, 'scripts', 'sign-release.mjs'), ...args], { encoding: 'utf8', env: { ...process.env, HOME: home, USERPROFILE: home } });

  const signed = run('1.4.0', directory, '--commit', COMMIT);
  assert.equal(signed.status, 0, signed.stderr);
  assert.equal(signed.stdout, `Verified 6 signed release artifacts built from ${COMMIT}.\n`);
  assert.equal(JSON.parse(fs.readFileSync(manifestOf(directory), 'utf8')).commit, COMMIT);

  const bare = run('1.4.0', directory, '--commit');
  assert.equal(bare.status, 1);
  assert.match(bare.stderr, /--commit takes the full SHA/);
});
