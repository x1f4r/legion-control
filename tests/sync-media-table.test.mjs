import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { MIRROR_REL_PATHS, syncMediaTables } from '../scripts/sync-media-table.mjs';

const repository = path.resolve(import.meta.dirname, '..');
const script = path.join(repository, 'scripts', 'sync-media-table.mjs');
const ALL_FILES = Object.values(MIRROR_REL_PATHS);

function toCrlf(text) {
  return text.replace(/\r\n/g, '\n').replace(/\n/g, '\r\n');
}

function toLf(text) {
  return text.replace(/\r\n/g, '\n');
}

function copyMediaFiles(t, { eol = '\n' } = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-media-sync-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const convert = eol === '\r\n' ? toCrlf : toLf;
  for (const rel of ALL_FILES) {
    const src = fs.readFileSync(path.join(repository, rel), 'utf8');
    const dst = path.join(root, rel);
    fs.mkdirSync(path.dirname(dst), { recursive: true });
    fs.writeFileSync(dst, convert(src), 'utf8');
  }
  return root;
}

test('sync-media-table --check passes against CRLF copies of the canonical table and all four mirrors', (t) => {
  const root = copyMediaFiles(t, { eol: '\r\n' });
  const beforeSnapshots = Object.fromEntries(
    ALL_FILES.map((rel) => [rel, fs.readFileSync(path.join(root, rel), 'utf8')]),
  );

  const checkResult = syncMediaTables({ repoRoot: root, checkOnly: true });
  assert.deepEqual(checkResult.drifted, []);
  assert.ok(checkResult.entriesCount >= 150);

  const cliCheck = spawnSync(process.execPath, [script, '--check', root], { encoding: 'utf8' });
  assert.equal(cliCheck.status, 0, cliCheck.stderr);
  assert.match(cliCheck.stdout, /All 4 media table mirrors match crates\/legion-core\/src\/media\.rs/);

  // Write mode on a clean CRLF checkout must neither report updates nor rewrite any file.
  const writeResult = syncMediaTables({ repoRoot: root, checkOnly: false });
  assert.deepEqual(writeResult.updated, []);
  for (const rel of ALL_FILES) {
    assert.equal(fs.readFileSync(path.join(root, rel), 'utf8'), beforeSnapshots[rel]);
  }
});

test('sync-media-table reports a real one-row difference on CRLF files and preserves CRLF when regenerating', (t) => {
  const root = copyMediaFiles(t, { eol: '\r\n' });
  const uiPath = path.join(root, MIRROR_REL_PATHS.uiFiles);
  const original = fs.readFileSync(uiPath, 'utf8');
  const mutated = original.replace(
    "  flac: ['audio', 'audio/flac', 'inline', true],",
    "  flac: ['audio', 'audio/x-drifted', 'inline', true],",
  );
  assert.notEqual(mutated, original, 'expected flac row in apps/ui/lib/files.js');
  fs.writeFileSync(uiPath, mutated, 'utf8');

  const cliCheck = spawnSync(process.execPath, [script, '--check', root], { encoding: 'utf8' });
  assert.equal(cliCheck.status, 1, 'real one-row drift must fail --check');
  assert.match(cliCheck.stderr, /apps[\\/]ui[\\/]lib[\\/]files\.js/);
  assert.doesNotMatch(cliCheck.stderr, /legion-content/);
  assert.doesNotMatch(cliCheck.stderr, /file_policy\.rs/);
  assert.doesNotMatch(cliCheck.stderr, /TransferPathPolicy\.kt/);

  const cliWrite = spawnSync(process.execPath, [script, root], { encoding: 'utf8' });
  assert.equal(cliWrite.status, 0, cliWrite.stderr);
  const regenerated = fs.readFileSync(uiPath, 'utf8');
  assert.equal(regenerated, original, 'regenerated CRLF file must restore the exact CRLF bytes');
  assert.equal(regenerated.includes('\r\n'), true, 'CRLF line endings must be preserved');
  assert.equal(/(^|[^\r])\n/.test(regenerated), false, 'regenerated CRLF file must not mix bare LF lines');

  const recheck = spawnSync(process.execPath, [script, '--check', root], { encoding: 'utf8' });
  assert.equal(recheck.status, 0, recheck.stderr);
});

test('sync-media-table preserves LF line endings when regenerating an LF mirror', (t) => {
  const root = copyMediaFiles(t, { eol: '\n' });
  const contentPath = path.join(root, MIRROR_REL_PATHS.content);
  const original = fs.readFileSync(contentPath, 'utf8');
  const mutated = original.replace(
    '    ("flac", "audio/flac"),',
    '    ("flac", "audio/x-drifted"),',
  );
  assert.notEqual(mutated, original, 'expected flac row in crates/legion-content/src/media.rs');
  fs.writeFileSync(contentPath, mutated, 'utf8');

  const writeResult = syncMediaTables({ repoRoot: root, checkOnly: false });
  assert.equal(writeResult.updated.length, 1);
  const regenerated = fs.readFileSync(contentPath, 'utf8');
  assert.equal(regenerated, original);
  assert.equal(regenerated.includes('\r\n'), false, 'LF file must remain LF-only after regeneration');
});
