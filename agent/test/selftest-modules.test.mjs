// `version --check` is what a staged agent has to pass before it replaces a
// working one, and it can only vouch for the modules it loads. Its list was
// once written out by hand in index.mjs, and every module added after that
// was missing from it. These tests hold the list to the source tree, so a new
// module that nobody adds to it fails here rather than going unchecked.

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { SELF_TEST_MODULES, WORKER_ENTRIES, WORKER_IMPORTS } from '../src/selftest-modules.mjs';
import { SRC } from './helpers.mjs';

/** Every module under src, as the lists name them. */
function sourceModules() {
  return fs.readdirSync(SRC, { recursive: true })
    .filter((file) => file.endsWith('.mjs'))
    .map((file) => file.split(path.sep).join('/'))
    .sort();
}

test('the self-test loads every module but the entry point, each exactly once', () => {
  const listed = [...SELF_TEST_MODULES, ...WORKER_ENTRIES];
  assert.equal(new Set(listed).size, listed.length, 'a module is listed twice');
  // index.mjs is the self-test, so it has loaded before the lists are read.
  assert.deepEqual([...listed, 'index.mjs'].sort(), sourceModules());
});

test('every module a worker imports only when it runs is a root of the worker check', () => {
  for (const worker of WORKER_ENTRIES) {
    const text = fs.readFileSync(path.join(SRC, worker), 'utf8');
    for (const [, specifier] of text.matchAll(/\bimport\(\s*['"`](\.[^'"`]+)['"`]\s*\)/g)) {
      const module = path.posix.normalize(path.posix.join(path.posix.dirname(worker), specifier));
      assert.ok(WORKER_IMPORTS.includes(module), `${worker} imports ${module}, which the worker check does not walk`);
    }
  }
  for (const module of WORKER_IMPORTS) assert.ok(fs.existsSync(path.join(SRC, module)), `${module} does not exist`);
});

test('a staged tree without a module nothing else imports at start-up fails its self-test', () => {
  // config.mjs is kept for the release tooling, which reads the version there.
  // No module of the agent imports it any more, so only the list notices it gone.
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-module-list-'));
  try {
    fs.cpSync(SRC, path.join(root, 'src'), { recursive: true });
    fs.copyFileSync(path.join(SRC, '..', 'package.json'), path.join(root, 'package.json'));
    const check = () => spawnSync(process.execPath, [path.join(root, 'src', 'index.mjs'), 'version', '--check'], {
      encoding: 'utf8', env: { ...process.env, LEGIONCTL_HOME: path.join(root, 'data') }, timeout: 20_000,
    });
    const whole = JSON.parse(check().stdout);
    assert.equal(whole.selfTest.ok, true, whole.selfTest.failed.join('; '));
    assert.equal(whole.selfTest.modules, SELF_TEST_MODULES.length + WORKER_ENTRIES.length);

    fs.unlinkSync(path.join(root, 'src', 'config.mjs'));
    const result = check();
    assert.equal(result.status, 1, result.stdout);
    const reply = JSON.parse(result.stdout);
    assert.equal(reply.selfTest.ok, false);
    assert.match(reply.selfTest.failed.join(' '), /^config\.mjs: /);
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});
