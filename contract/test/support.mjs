// What the contract tests share: a throwaway copy of the contract, the two
// commands that check it, and the two ways of proving a check bites.
//
// A check that passes on the committed tree proves nothing on its own: a
// validator that reported no problems whatever it was given would pass too. So
// the tests copy the contract, break one thing in the copy, and assert that the
// check FAILS, and fails for the reason the test names. A defect that is not
// caught here is a defect that reaches a client.

import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
export const CONTRACT = path.join(HERE, '..');
export const REPO = path.join(CONTRACT, '..');

export const readJson = (file) => JSON.parse(fs.readFileSync(file, 'utf8'));
export const writeJson = (file, value) => fs.writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`);

function run(script, args, { env } = {}) {
  try {
    const stdout = execFileSync(process.execPath, [script, ...args], {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
      env: env ?? process.env,
    });
    return { ok: true, output: stdout };
  } catch (error) {
    return { ok: false, output: `${error.stdout ?? ''}${error.stderr ?? ''}` };
  }
}

/** Run contract/validate.mjs in the contract directory `dir`. */
export function runValidate(dir) {
  return run(path.join(dir, 'validate.mjs'), []);
}

/** Run contract/tools/generate.mjs in the contract directory `dir`, with `args` such as ['--check']. */
export function runGenerate(dir, args = [], options = {}) {
  return run(path.join(dir, 'tools', 'generate.mjs'), args, options);
}

/**
 * A throwaway copy of contract/, so a broken fixture never touches the tree.
 *
 * The copy sits in a directory of its own beside a copy of agent/package.json,
 * because the fixture generator reads the agent version from there, relative
 * to itself, exactly as it does in the repository.
 */
export function scratch() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-contract-'));
  const dir = path.join(root, 'contract');
  fs.cpSync(CONTRACT, dir, { recursive: true });
  fs.mkdirSync(path.join(root, 'agent'));
  fs.copyFileSync(path.join(REPO, 'agent', 'package.json'), path.join(root, 'agent', 'package.json'));
  return dir;
}

/** Remove a copy made by scratch(). */
export function discard(dir) {
  fs.rmSync(path.dirname(dir), { recursive: true, force: true });
}

/**
 * Break one thing, and assert the check notices.
 *
 * `expect` is matched against the output so a test cannot pass because the
 * validator failed for some unrelated reason.
 */
export function defect(name, expect, breakIt) {
  test(`caught: ${name}`, () => {
    const dir = scratch();
    try {
      breakIt(dir);
      const result = runValidate(dir);
      assert.equal(result.ok, false, `nothing caught "${name}"`);
      assert.match(result.output, expect);
    } finally {
      discard(dir);
    }
  });
}

/**
 * Break a reference implementation, and assert that both checks notice: the
 * validator, because the committed vectors no longer follow from it, and the
 * generator, because a hand-written `want` or a committed byte now disagrees.
 *
 * The defect is a literal replacement of `find`, which must occur exactly once
 * in `file` (relative to contract/). A defect whose text has moved would
 * otherwise silently stop being a defect and pass.
 */
export function referenceDefect(name, file, find, replace, expect) {
  test(`caught: ${name}`, () => {
    const dir = scratch();
    try {
      const target = path.join(dir, file);
      const source = fs.readFileSync(target, 'utf8');
      assert.equal(source.split(find).length - 1, 1, `the defect "${name}" no longer applies: its text is not in ${file} exactly once`);
      fs.writeFileSync(target, source.replace(find, replace));
      const validated = runValidate(dir);
      assert.equal(validated.ok, false, `validate.mjs did not catch "${name}"`);
      assert.match(validated.output, expect);
      const generated = runGenerate(dir, ['--check']);
      assert.equal(generated.ok, false, `generate.mjs --check did not catch "${name}"`);
    } finally {
      discard(dir);
    }
  });
}
