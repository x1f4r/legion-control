import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';

const repository = path.resolve(import.meta.dirname, '..');

// A checkout reached through a symlink, as one under a symlinked directory is, or any under /tmp
// on macOS. Node resolves the symlinks in import.meta.url but not in argv[1], so a script that
// compares the two as typed takes itself for imported, does nothing and exits 0.
function linkedCheckout(t) {
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-linked-'));
  t.after(() => fs.rmSync(scratch, { recursive: true, force: true }));
  const root = path.join(scratch, 'checkout');
  fs.mkdirSync(path.join(root, 'scripts'), { recursive: true });
  for (const script of ['checkout-files.mjs', 'check-syntax.mjs', 'sign-legion-release.mjs']) {
    fs.copyFileSync(path.join(repository, 'scripts', script), path.join(root, 'scripts', script));
  }
  const link = path.join(scratch, 'link');
  fs.symlinkSync(root, link, 'junction');
  return { scratch, root, scripts: path.join(link, 'scripts') };
}
const run = (script, args = [], env = {}) => spawnSync(process.execPath, [script, ...args], { encoding: 'utf8', env: { ...process.env, ...env } });

test('sign-legion-release run through a symlink runs its CLI entrypoint and rejects an invalid tag', (t) => {
  const { scripts } = linkedCheckout(t);
  const bad = run(path.join(scripts, 'sign-legion-release.mjs'), ['not-a-tag']);
  assert.equal(bad.status, 1);
  assert.match(bad.stderr, /Usage: node scripts\/sign-legion-release\.mjs/);
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
