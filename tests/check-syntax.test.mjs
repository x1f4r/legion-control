import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { checkSyntax } from '../scripts/check-syntax.mjs';

const repository = path.resolve(import.meta.dirname, '..');
const script = path.join(repository, 'scripts', 'check-syntax.mjs');

// A git checkout with nothing committed: the check reads what git would track, not what it has.
function checkout(t, files) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-syntax-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  assert.equal(spawnSync('git', ['init', '-q', root]).status, 0);
  for (const [file, text] of Object.entries(files)) {
    fs.mkdirSync(path.dirname(path.join(root, file)), { recursive: true });
    fs.writeFileSync(path.join(root, file), text);
  }
  return root;
}

test('every JavaScript file of the checkout is parsed, wherever it is, and the one that does not parse is named', async (t) => {
  const root = checkout(t, {
    'scripts/release-step.mjs': "export const step = () => 'fine';\n",
    'contract/bootstrap/install.cjs': "module.exports = { ok: true };\n",
    'tests/deep/nested/broken.test.mjs': "import test from 'node:test';\ntest('x', () => { if (true) {;\n",
    'docs/notes.md': '{ not javascript\n',
  });
  const result = await checkSyntax(root);
  assert.equal(result.files, 3);
  assert.deepEqual(result.problems.map(problem => problem.file), ['tests/deep/nested/broken.test.mjs']);
  assert.match(result.problems[0].message, /SyntaxError/);

  const run = spawnSync(process.execPath, [script, root], { encoding: 'utf8' });
  assert.equal(run.status, 1);
  assert.match(run.stderr, /tests\/deep\/nested\/broken\.test\.mjs does not parse:/);
  assert.match(run.stderr, /1 of 3 JavaScript files do not parse\./);
});

test('build output git ignores is not the checkout, and a clean checkout passes', async (t) => {
  const root = checkout(t, {
    '.gitignore': 'build/\n',
    'agent/src/index.mjs': "export default 1;\n",
    'build/generated.mjs': 'this is not javascript at all\n',
  });
  const run = spawnSync(process.execPath, [script, root], { encoding: 'utf8' });
  assert.equal(run.status, 0, run.stderr);
  assert.equal(run.stdout, 'Syntax checked 1 JavaScript files.\n');
});

test('outside a git checkout the check says why in one line, not in a stack trace', (t) => {
  // A source archive has no .git, and git is kept from finding one further up.
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'legion-syntax-archive-')));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  fs.writeFileSync(path.join(root, 'index.mjs'), 'export default 1;\n');
  const run = spawnSync(process.execPath, [script, root], { encoding: 'utf8', env: { ...process.env, GIT_CEILING_DIRECTORIES: path.dirname(root), LC_ALL: 'C' } });
  assert.equal(run.status, 2);
  assert.equal(run.stdout, '');
  assert.match(run.stderr, /^git could not list the files of .+: fatal: not a git repository/);
  assert.equal(run.stderr.trim().split('\n').length, 1, run.stderr);
});
