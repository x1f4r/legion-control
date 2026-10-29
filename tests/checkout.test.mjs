import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';

// What a checkout of the repository holds, and how git writes it out, read from git itself.
const repository = path.resolve(import.meta.dirname, '..');
const git = (...args) => {
  const result = spawnSync('git', ['-C', repository, ...args], { encoding: 'utf8' });
  assert.equal(result.status, 0, result.stderr);
  return result.stdout;
};

test('every command of the Codex environments runs a script the checkout tracks as executable', () => {
  const directory = path.join(repository, '.codex', 'environments');
  const environments = fs.readdirSync(directory).filter(name => name.endsWith('.toml'));
  assert.ok(environments.length > 0, 'there is no Codex environment to check');
  let commands = 0;
  for (const name of environments) {
    // Only the two keys that name something to run, each a plain string on its own line, which is
    // how the Codex app writes them; an empty setup script runs nothing.
    for (const [, key, value] of fs.readFileSync(path.join(directory, name), 'utf8').matchAll(/^\s*(command|script)\s*=\s*"([^"]*)"\s*$/gm)) {
      if (value === '') continue;
      commands += 1;
      const file = value.split(/\s+/)[0].replace(/^\.\//, '');
      // The index rather than the file system, which on Windows has no executable bit to read.
      const entry = git('ls-files', '--stage', '--', file);
      assert.match(entry, /^100755 /, `${name}: the ${key} ${value} runs ${file}, which the checkout ${entry ? 'does not mark executable' : 'does not have'}`);
    }
  }
  assert.ok(commands > 0, 'no Codex environment runs anything');
});
