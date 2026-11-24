import assert from 'node:assert/strict';
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

test('scripts are written out with LF line endings in every checkout', () => {
  // `git ls-files --eol` gives, for each file, the endings the index holds, the ones the working
  // tree has, and the attributes that decide what a checkout makes of them, whatever core.autocrlf
  // says on the machine.
  const files = new Map(git('ls-files', '--eol', '--', '*.sh', '*.ps1', '*.mjs').split('\n').filter(Boolean).map((line) => {
    const [, index, attributes, file] = /^i\/(\S*)\s+w\/\S*\s+attr\/(.*?)\s*\t(.+)$/.exec(line);
    return [file, { index, attributes }];
  }));
  for (const file of ['scripts/check.sh', 'install/legion.sh', 'install/legion.ps1', 'scripts/check-powershell.ps1', 'scripts/sign-legion-release.mjs']) {
    assert.ok(files.has(file), `${file} is not in the checkout`);
  }
  for (const [file, { index, attributes }] of files) {
    assert.equal(attributes, 'text eol=lf', `${file} has no LF rule`);
    assert.equal(index, 'lf', `${file} is stored with ${index} line endings`);
  }
  // And a script added later, anywhere, under the same rules.
  const later = git('check-attr', 'text', 'eol', '--', 'somewhere/new.sh', 'somewhere/new.ps1', 'somewhere/new.mjs');
  assert.equal(later, [
    'somewhere/new.sh: text: set', 'somewhere/new.sh: eol: lf',
    'somewhere/new.ps1: text: set', 'somewhere/new.ps1: eol: lf',
    'somewhere/new.mjs: text: set', 'somewhere/new.mjs: eol: lf',
  ].join('\n') + '\n');
});

test('Android Gradle wrapper scripts keep LF for gradlew and CRLF for gradlew.bat', () => {
  const files = new Map(git('ls-files', '--eol', '--', '*gradlew', '*gradlew.bat').split('\n').filter(Boolean).map((line) => {
    const [, index, attributes, file] = /^i\/(\S*)\s+w\/\S*\s+attr\/(.*?)\s*\t(.+)$/.exec(line);
    return [file, { index, attributes }];
  }));
  const unixWrapper = 'apps/legion-app/src-tauri/gen/android/gradlew';
  const winWrapper = 'apps/legion-app/src-tauri/gen/android/gradlew.bat';
  assert.ok(files.has(unixWrapper), `${unixWrapper} is not in the checkout`);
  assert.ok(files.has(winWrapper), `${winWrapper} is not in the checkout`);
  for (const [file, { index, attributes }] of files) {
    if (file.endsWith('.bat')) {
      assert.equal(attributes, 'text eol=crlf', `${file} has no CRLF rule`);
      assert.equal(index, 'crlf', `${file} is stored with ${index} line endings`);
    } else {
      assert.equal(attributes, 'text eol=lf', `${file} has no LF rule`);
      assert.equal(index, 'lf', `${file} is stored with ${index} line endings`);
    }
  }
});
