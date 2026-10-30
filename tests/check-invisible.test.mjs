import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { checkInvisible } from '../scripts/check-invisible.mjs';

const repository = path.resolve(import.meta.dirname, '..');
const script = path.join(repository, 'scripts', 'check-invisible.mjs');

// The characters are made from their code points, so this file holds none of them itself.
const RLO = String.fromCodePoint(0x202e);
const ZWSP = String.fromCodePoint(0x200b);
const BOM = String.fromCodePoint(0xfeff);

// A git checkout with nothing committed: the check reads what git would track, not what it has.
function checkout(t, files) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-invisible-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  assert.equal(spawnSync('git', ['init', '-q', root]).status, 0);
  for (const [file, content] of Object.entries(files)) {
    fs.mkdirSync(path.dirname(path.join(root, file)), { recursive: true });
    fs.writeFileSync(path.join(root, file), content);
  }
  return root;
}

test('an invisible or bidirectional character anywhere in the checkout is named by file, line and code point', (t) => {
  const root = checkout(t, {
    'desktop/Tests/PeerTests.cs': `var id = "pi${RLO}exe.txt";\nvar fine = "pi\\u202Eexe.txt";\n`,
    'contract/notes.md': `one\ntwo${ZWSP}words\n`,
    'agent/install/signed.ps1': `${BOM}Write-Output 'a script Windows PowerShell 5.1 reads as UTF-8'\n`,
    'agent/install/late.ps1': `Write-Output 'a mark half way'${BOM}\n`,
    'mac/Resources/icon.bin': Buffer.concat([Buffer.from([0x89, 0x50, 0x00]), Buffer.from(RLO)]),
  });
  const result = checkInvisible(root);
  assert.equal(result.files, 4, 'the binary file is skipped');
  assert.deepEqual(result.problems, [
    { file: 'agent/install/late.ps1', line: 1, points: ['U+FEFF'] },
    { file: 'contract/notes.md', line: 2, points: ['U+200B'] },
    { file: 'desktop/Tests/PeerTests.cs', line: 1, points: ['U+202E'] },
  ]);

  const run = spawnSync(process.execPath, [script, root], { encoding: 'utf8' });
  assert.equal(run.status, 1);
  assert.match(run.stderr, /desktop\/Tests\/PeerTests\.cs:1: U\+202E; write it as an escape\./);
  assert.match(run.stderr, /3 lines in 3 of 4 files hold invisible or bidirectional characters\./);
});

test('a checkout without them passes, and says how many files it read', (t) => {
  const root = checkout(t, { 'README.md': 'plain text\n', 'scripts/a.mjs': 'export const a = 1;\n' });
  const run = spawnSync(process.execPath, [script, root], { encoding: 'utf8' });
  assert.equal(run.status, 0, run.stderr);
  assert.match(run.stdout, /No invisible or bidirectional characters in 2 files\./);
});
