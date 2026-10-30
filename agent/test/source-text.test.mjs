// No file a person reviews holds a character that reads differently from how
// it runs: zero-width, bidirectional or a byte order mark. The check that
// refuses them was first written with the very characters it looks for, in a
// directory it did not look at, so these tests hold it to the files it reads
// as well as to what it finds in them. Every such character here is made with
// String.fromCodePoint, for the same reason.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { INVISIBLE, invisibleCharacters, reviewedFiles } from '../scripts/invisible.mjs';
import { REPO } from './helpers.mjs';

const AGENT = path.join(REPO, 'agent');

/** Every code point the check refuses, range by range. */
const REFUSED = [[0x200B, 0x200F], [0x202A, 0x202E], [0x2060, 0x2069], [0xFEFF, 0xFEFF]]
  .flatMap(([first, last]) => Array.from({ length: last - first + 1 }, (_, index) => first + index));
const named = (point) => `U+${point.toString(16).toUpperCase().padStart(4, '0')}`;
const MARK = String.fromCodePoint(0xFEFF);

test('every file a person reviews is free of them, this check and these tests included', () => {
  const reviewed = reviewedFiles(AGENT);
  const names = reviewed.map((file) => path.relative(AGENT, file).split(path.sep).join('/'));
  for (const name of ['package.json', 'scripts/check-syntax.mjs', 'scripts/invisible.mjs', 'src/launch.mjs', 'install/dispatch.ps1', 'install/launcher.mjs', 'install/legion-control-update.timer', 'test/source-text.test.mjs']) {
    assert.ok(names.includes(name), `${name} is not checked`);
  }
  for (const file of reviewed) assert.deepEqual(invisibleCharacters(fs.readFileSync(file, 'utf8'), file), [], path.relative(AGENT, file));
  // The class says which ranges it covers in text a person can read.
  assert.ok([...INVISIBLE.source].every((character) => character >= ' ' && character <= '~'), INVISIBLE.source);
});

test('each one is found, named by its code point on its line', () => {
  for (const point of REFUSED) {
    assert.deepEqual(invisibleCharacters(`first line\nsecond ${String.fromCodePoint(point)} line\n`, 'any.mjs'), [{ line: 2, points: [named(point)] }], named(point));
  }
  assert.deepEqual(invisibleCharacters(`a${String.fromCodePoint(0x202E)}b${String.fromCodePoint(0x2066)}c\n`, 'any.sh'), [{ line: 1, points: ['U+202E', 'U+2066'] }]);
  // What sits beside each range, and text a person can see however far from ASCII, is left alone.
  for (const point of [0x200A, 0x2010, 0x2029, 0x202F, 0x205F, 0x206A, 0xFEFE, 0xFF00]) {
    assert.deepEqual(invisibleCharacters(`a${String.fromCodePoint(point)}b`, 'any.mjs'), [], named(point));
  }
  assert.deepEqual(invisibleCharacters('naïve — café → done\n', 'any.mjs'), []);
});

test('a byte order mark may open a PowerShell script, and nowhere else', () => {
  assert.deepEqual(invisibleCharacters(`${MARK}Write-Output 'ok'\r\n`, 'install-windows.ps1'), []);
  assert.deepEqual(invisibleCharacters(`${MARK}Write-Output 'ok'\r\n`, 'INSTALL.PS1'), []);
  assert.deepEqual(invisibleCharacters(`${MARK}${MARK}Write-Output 'ok'\r\n`, 'twice.ps1'), [{ line: 1, points: ['U+FEFF'] }]);
  assert.deepEqual(invisibleCharacters(`Write-Output 'ok'\r\n${MARK}exit 0\r\n`, 'later.ps1'), [{ line: 2, points: ['U+FEFF'] }]);
  assert.deepEqual(invisibleCharacters(`${MARK}export {};\n`, 'index.mjs'), [{ line: 1, points: ['U+FEFF'] }]);
  assert.deepEqual(invisibleCharacters(`${MARK}#!/bin/sh\n`, 'dispatch.sh'), [{ line: 1, points: ['U+FEFF'] }]);
});

test('npm run check fails on one in its own directory, and names the file and line', () => {
  const agent = fs.mkdtempSync(path.join(os.tmpdir(), 'source text '));
  try {
    for (const directory of ['src', 'install', 'scripts', 'test']) fs.mkdirSync(path.join(agent, directory));
    for (const name of ['check-syntax.mjs', 'invisible.mjs']) fs.copyFileSync(path.join(AGENT, 'scripts', name), path.join(agent, 'scripts', name));
    fs.writeFileSync(path.join(agent, 'package.json'), '{}\n');
    fs.writeFileSync(path.join(agent, 'src', 'index.mjs'), 'export {};\n');
    fs.writeFileSync(path.join(agent, 'install', 'launcher.ps1'), `${MARK}exit 0\r\n`);
    // Not text anyone reviews, whatever bytes it happens to hold.
    fs.writeFileSync(path.join(agent, 'src', '.DS_Store'), `Bud1${String.fromCodePoint(0x202E)}`);
    const check = () => spawnSync(process.execPath, [path.join(agent, 'scripts', 'check-syntax.mjs')], { encoding: 'utf8' });
    const clean = check();
    assert.equal(clean.status, 0, clean.stdout + clean.stderr);
    assert.match(clean.stdout, /No invisible or bidirectional characters in 5 reviewed files/);

    fs.writeFileSync(path.join(agent, 'scripts', 'planted.mjs'), `// fine\nconst allowed = 'user${String.fromCodePoint(0x202E)}';\n`);
    const planted = check();
    assert.equal(planted.status, 1, planted.stdout + planted.stderr);
    assert.match(planted.stderr, /scripts[/\\]planted\.mjs:2: U\+202E; write it as a \\u escape/);
  } finally { fs.rmSync(agent, { recursive: true, force: true }); }
});
