import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { createSigningKey, signManifest } from '../scripts/release-integrity.mjs';
import { edit, versionTree } from './fixtures/version-tree.mjs';

const repository = path.resolve(import.meta.dirname, '..');

// A checkout reached through a symlink, as one under a symlinked directory is, or any under /tmp
// on macOS. Node resolves the symlinks in import.meta.url but not in argv[1], so a script that
// compares the two as typed takes itself for imported, does nothing and exits 0.
function linkedCheckout(t) {
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-linked-'));
  t.after(() => fs.rmSync(scratch, { recursive: true, force: true }));
  const root = versionTree(path.join(scratch, 'checkout'));
  for (const script of ['check-versions.mjs', 'package-agent.mjs', 'release-integrity.mjs']) {
    fs.copyFileSync(path.join(repository, 'scripts', script), path.join(root, 'scripts', script));
  }
  const link = path.join(scratch, 'link');
  fs.symlinkSync(root, link, 'junction');
  return { scratch, root, scripts: path.join(link, 'scripts') };
}
const run = (script, args = [], env = {}) => spawnSync(process.execPath, [script, ...args], { encoding: 'utf8', env: { ...process.env, ...env } });

test('check-versions run through a symlink checks its checkout and fails on a stale copy', (t) => {
  const { root, scripts } = linkedCheckout(t);
  const agreeing = run(path.join(scripts, 'check-versions.mjs'));
  assert.equal(agreeing.status, 0, agreeing.stderr);
  assert.equal(agreeing.stdout, 'Versions agree: agent 3.1.0 (floor 3.1.0), app 1.3.4 (build 14).\n');
  edit(root, 'mac/Sources/LegionControlCore/AgentContract.swift', '"3.1.0"', '"3.0.1"');
  const stale = run(path.join(scripts, 'check-versions.mjs'));
  assert.equal(stale.status, 1);
  assert.match(stale.stderr, /bundledAgentVersion = "3\.0\.1", but agent\/package\.json says 3\.1\.0/);
});

test('release-integrity run through a symlink verifies, and refuses another signer', (t) => {
  const { scratch, scripts } = linkedCheckout(t);
  const file = (name) => path.join(scratch, name);
  createSigningKey(file('private.pem'), file('public.pem'));
  createSigningKey(file('other-private.pem'), file('other-public.pem'));
  fs.writeFileSync(file('manifest.json'), '{"version":"1.4.0"}\n');
  signManifest(file('manifest.json'), file('private.pem'), file('manifest.json.sig'));
  const trusted = run(path.join(scripts, 'release-integrity.mjs'), ['verify', file('manifest.json'), file('manifest.json.sig'), file('public.pem')]);
  assert.equal(trusted.status, 0, trusted.stderr);
  assert.equal(trusted.stdout, 'Release integrity check completed.\n');
  const other = run(path.join(scripts, 'release-integrity.mjs'), ['verify', file('manifest.json'), file('manifest.json.sig'), file('other-public.pem')]);
  assert.equal(other.status, 1);
  assert.match(other.stderr, /does not match the trusted key/);
});

test('package-agent run through a symlink packages, or says why it cannot', (t) => {
  const { scratch, root, scripts } = linkedCheckout(t);
  // No signing key in this home, so it stops there, before anything is written to dist/.
  const home = path.join(scratch, 'home');
  fs.mkdirSync(home);
  const result = run(path.join(scripts, 'package-agent.mjs'), [], { HOME: home, USERPROFILE: home });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /Release signing key missing/);
  assert.equal(fs.existsSync(path.join(root, 'dist')), false);
});
