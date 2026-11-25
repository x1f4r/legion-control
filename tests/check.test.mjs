import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';

const repository = path.resolve(import.meta.dirname, '..');

// scripts/check.sh in a scratch checkout that holds only what a test puts there, run with a node
// and a cargo that note what they were asked to do. node runs the scratch test files for real so
// failure propagation through scripts/check.sh is exercised directly.
function scratch(t) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-check-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const write = (file, text, mode) => {
    fs.mkdirSync(path.dirname(path.join(root, file)), { recursive: true });
    fs.writeFileSync(path.join(root, file), text, { mode });
  };
  write('scripts/check.sh', fs.readFileSync(path.join(repository, 'scripts', 'check.sh')), 0o755);
  const note = (name) => `printf '%s\\n' "${name} $*" >> "$LEGION_TEST_CALLS"\n`;
  write('bin/cargo', `#!/bin/sh\n${note('cargo')}`, 0o755);
  write('bin/node', `#!/bin/sh\n${note('node')}case "$1 $2" in\n  "--test tests/"*|"--test apps/ui/lib/test/"*) exec "$LEGION_TEST_NODE" "$@" ;;\nesac\n`, 0o755);
  const calls = path.join(root, 'calls');
  // Without NODE_TEST_CONTEXT, which this test's own runner sets: a node --test that inherits it
  // reports to that runner instead of through its exit status, and then passes whatever failed.
  const inherited = Object.fromEntries(Object.entries(process.env).filter(([name]) => name !== 'NODE_TEST_CONTEXT'));
  const run = (component, env = {}) => {
    fs.rmSync(calls, { force: true });
    const args = component === undefined ? [path.join(root, 'scripts', 'check.sh')] : [path.join(root, 'scripts', 'check.sh'), component];
    const result = spawnSync('bash', args, {
      encoding: 'utf8',
      env: { ...inherited, PATH: `${path.join(root, 'bin')}${path.delimiter}${process.env.PATH}`, LEGION_TEST_CALLS: calls, LEGION_TEST_NODE: process.execPath, ...env },
    });
    return { ...result, calls: fs.existsSync(calls) ? fs.readFileSync(calls, 'utf8').trim().split('\n') : [] };
  };
  return { root, write, run };
}

const PASSING = "import test from 'node:test';\ntest('holds', () => {});\n";
const FAILING = "import assert from 'node:assert/strict';\nimport test from 'node:test';\ntest('a planted failure', () => assert.fail('planted in tests'));\n";

test('the scripts leg checks syntax, invisible characters, media table sync, script and UI tests, and private data in order', { skip: process.platform === 'win32' }, (t) => {
  const { write, run } = scratch(t);
  write('tests/passing.test.mjs', PASSING);
  write('apps/ui/lib/test/passing.test.mjs', PASSING);

  const green = run('scripts');
  assert.equal(green.status, 0, green.stderr);
  assert.deepEqual(green.calls, [
    'node scripts/check-syntax.mjs',
    'node scripts/check-invisible.mjs',
    'node scripts/sync-media-table.mjs --check',
    'node --test tests/passing.test.mjs',
    'node --test apps/ui/lib/test/passing.test.mjs',
    'node scripts/check-private-data.mjs',
  ]);

  // One failing test stops the scripts leg and prevents later legs from running under `all`.
  write('tests/planted.test.mjs', FAILING);
  const red = run('scripts');
  assert.notEqual(red.status, 0, red.stdout);
  assert.match(red.stdout, /planted in tests/);
  const all = run('all');
  assert.notEqual(all.status, 0, all.stdout);
  assert.equal(all.calls.some((call) => call.startsWith('cargo ')), false, 'nothing after the failing leg runs');
});

test('the rust leg runs fmt, clippy, workspace tests, and the generated tool reference check', { skip: process.platform === 'win32' }, (t) => {
  const { run } = scratch(t);
  const green = run('rust');
  assert.equal(green.status, 0, green.stderr);
  assert.deepEqual(green.calls, [
    'cargo fmt --all --check',
    'cargo clippy --workspace --all-targets --locked -- -D warnings',
    'cargo test --workspace --locked --no-fail-fast',
    'node scripts/legion-tools-doc.mjs --bin target/debug/legion --check',
  ]);
});

test('an unknown component name is refused with usage and exit status 2', { skip: process.platform === 'win32' }, (t) => {
  const { run } = scratch(t);
  const bad = run('unknown');
  assert.equal(bad.status, 2);
  assert.match(bad.stderr, /Usage: .* \[all\|scripts\|rust\]/);
  assert.deepEqual(bad.calls, []);
});
