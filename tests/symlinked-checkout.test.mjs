import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { createSigningKey, signManifest } from '../scripts/release-integrity.mjs';

const repository = path.resolve(import.meta.dirname, '..');

// A checkout reached through a symlink, as one under a symlinked directory is, or any under /tmp
// on macOS. Node resolves the symlinks in import.meta.url but not in argv[1], so a script that
// compares the two as typed takes itself for imported, does nothing and exits 0.
function linkedCheckout(t) {
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-linked-'));
  t.after(() => fs.rmSync(scratch, { recursive: true, force: true }));
  const root = path.join(scratch, 'checkout');
  fs.mkdirSync(path.join(root, 'scripts'), { recursive: true });
  for (const script of ['checkout-files.mjs', 'check-syntax.mjs', 'release-integrity.mjs']) {
    fs.copyFileSync(path.join(repository, 'scripts', script), path.join(root, 'scripts', script));
  }
  const link = path.join(scratch, 'link');
  fs.symlinkSync(root, link, 'junction');
  return { scratch, root, scripts: path.join(link, 'scripts') };
}
const run = (script, args = [], env = {}) => spawnSync(process.execPath, [script, ...args], { encoding: 'utf8', env: { ...process.env, ...env } });

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

test('check-syntax run through a symlink checks the checkout and fails on invalid syntax', (t) => {
  const { root, scripts } = linkedCheckout(t);
  assert.equal(spawnSync('git', ['init', '-q', root]).status, 0);
  const ok = run(path.join(scripts, 'check-syntax.mjs'));
  assert.equal(ok.status, 0, ok.stderr);
  assert.match(ok.stdout, /^Syntax checked 3 JavaScript files\.\n$/);
  fs.writeFileSync(path.join(root, 'scripts', 'broken.mjs'), 'export const x = (;\n');
  const bad = run(path.join(scripts, 'check-syntax.mjs'));
  assert.equal(bad.status, 1);
  assert.match(bad.stderr, /scripts\/broken\.mjs does not parse:/);
});
