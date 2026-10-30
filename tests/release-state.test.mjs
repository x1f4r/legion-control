import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { assetVerdict, ciVerdict, newerVerdict, releaseState, uploadedVerdict } from '../scripts/release-state.mjs';

const repository = path.resolve(import.meta.dirname, '..');
const SHA = 'a'.repeat(40);
const run = (fields) => ({ workflowName: 'Checks', headSha: SHA, status: 'completed', conclusion: 'success', createdAt: '2026-10-01T10:00:00Z', ...fields });

test('CI is green only when every workflow that ran on the commit last passed', () => {
  assert.deepEqual(ciVerdict([run()], SHA), { ok: true, message: `CI passed on ${SHA.slice(0, 12)} (Checks).` });
  // A failed run that was run again and passed counts as passed; the other way round it does not.
  const failedThenPassed = [run({ conclusion: 'failure' }), run({ createdAt: '2026-10-01T11:00:00Z' })];
  assert.equal(ciVerdict(failedThenPassed, SHA).ok, true);
  const passedThenFailed = [run(), run({ conclusion: 'failure', createdAt: '2026-10-01T11:00:00Z' })];
  assert.match(ciVerdict(passedThenFailed, SHA).message, /^CI did not pass on a{12} \(Checks: failure\)/);

  for (const [runs, message] of [
    [[], /^CI has not run on a{12}\. .*push it to main/],
    [[run({ headSha: 'b'.repeat(40) })], /^CI has not run on/],
    [[run({ status: 'in_progress', conclusion: '' })], /^CI is still running on a{12} \(Checks\)/],
    [[run({ status: 'queued', conclusion: null })], /^CI is still running/],
    [[run({ conclusion: 'cancelled' })], /\(Checks: cancelled\)/],
    [[run({ conclusion: 'skipped' })], /\(Checks: skipped\)/],
    [[run(), run({ workflowName: 'CodeQL', conclusion: 'failure' })], /\(CodeQL: failure\)/],
  ]) {
    const verdict = ciVerdict(runs, SHA);
    assert.equal(verdict.ok, false, JSON.stringify(runs));
    assert.match(verdict.message, message);
  }
});

test("a run GitHub starts on its own, such as Dependabot's, neither holds a release back nor stands in for CI", () => {
  const dependabot = (fields) => run({ workflowName: 'Dependabot Updates', event: 'dynamic', ...fields });
  const checks = run({ event: 'push' });
  for (const other of [dependabot({ status: 'in_progress', conclusion: '' }), dependabot({ conclusion: 'failure' })]) {
    assert.deepEqual(ciVerdict([checks, other], SHA), { ok: true, message: `CI passed on ${SHA.slice(0, 12)} (Checks).` });
  }
  assert.match(ciVerdict([dependabot({})], SHA).message, /^CI has not run on a{12}\./);
  // A workflow of the repository's own still counts, whatever started it.
  assert.match(ciVerdict([checks, run({ workflowName: 'CodeQL', event: 'schedule', conclusion: 'failure' })], SHA).message, /\(CodeQL: failure\)/);
});

test('a release of the version is none, a draft or published', () => {
  const releases = [{ tagName: 'v1.3.4', isDraft: false }, { tagName: 'v1.4.0', isDraft: true }];
  assert.equal(releaseState(releases, '1.4.0'), 'draft');
  assert.equal(releaseState(releases, '1.3.4'), 'published');
  assert.equal(releaseState(releases, '1.4.1'), 'none');
  assert.equal(releaseState([], '1.4.0'), 'none');
});

test('a version has to be above every release and every version tag on origin', () => {
  const releases = [
    { tagName: 'v1.3.4', isDraft: false, isPrerelease: false },
    { tagName: 'v1.3.10', isDraft: false, isPrerelease: false },
    // Neither of these is out: a draft nobody can see, a prerelease no client takes.
    { tagName: 'v2.0.0', isDraft: true, isPrerelease: false },
    { tagName: 'v1.9.0', isDraft: false, isPrerelease: true },
  ];
  const tags = `${'1'.repeat(40)}\trefs/tags/v1.3.10\n${'2'.repeat(40)}\trefs/tags/v1.3.10^{}\n${'3'.repeat(40)}\trefs/tags/v1.0.0\n`;
  // Compared number by number, never as text: 1.3.10 is above 1.3.4.
  assert.deepEqual(newerVerdict('1.4.0', releases, tags), { ok: true, newest: '1.3.10' });
  assert.deepEqual(newerVerdict('1.3.11', releases, tags), { ok: true, newest: '1.3.10' });
  const same = newerVerdict('1.3.10', releases, tags);
  assert.equal(same.ok, false);
  assert.match(same.message, /^1\.3\.10 is not above 1\.3\.10, the newest version already released\./);
  assert.match(newerVerdict('1.3.9', releases, tags).message, /^1\.3\.9 is not above 1\.3\.10/);

  // A tag pushed without a release still claims its version.
  const tagged = newerVerdict('1.4.0', releases, `${tags}${'4'.repeat(40)}\trefs/tags/v1.5.0\n`);
  assert.match(tagged.message, /^1\.4\.0 is not above 1\.5\.0, the newest version already tagged on origin\./);
  assert.deepEqual(newerVerdict('1.0.0', [], ''), { ok: true, newest: '' });
});

test('the draft holds every asset only when each is there at the size it was built', () => {
  const files = [{ name: 'a.zip', size: 3 }, { name: 'b.apk', size: 5 }];
  assert.deepEqual(assetVerdict({ assets: [{ name: 'a.zip', size: 3 }, { name: 'b.apk', size: 5 }] }, files), { ok: true, message: 'The draft release holds all 2 assets.' });
  assert.match(assetVerdict({ assets: [{ name: 'a.zip', size: 3 }] }, files).message, /does not hold b\.apk as built/);
  assert.match(assetVerdict({ assets: [{ name: 'a.zip', size: 3 }, { name: 'b.apk', size: 4 }] }, files).message, /does not hold b\.apk/);
  assert.match(assetVerdict({ assets: [{ name: 'a.zip', size: 3, state: 'starter' }, { name: 'b.apk', size: 5 }] }, files).message, /does not hold a\.zip/);
  assert.match(assetVerdict({}, files).message, /does not hold a\.zip, b\.apk/);
});

test('without the files to measure, the draft holds every asset only when each name is there, uploaded in full', () => {
  const names = ['a.zip', 'b.apk'];
  assert.deepEqual(uploadedVerdict({ assets: [{ name: 'a.zip', size: 3, state: 'uploaded' }, { name: 'b.apk', size: 5 }] }, names), { ok: true, message: 'The draft release holds all 2 assets.' });
  assert.deepEqual(uploadedVerdict({ assets: [{ name: 'a.zip', size: 3 }] }, names), { ok: false, message: 'the draft release does not hold b.apk.' });
  // An upload that broke off is still on the draft, in the state GitHub keeps it in until it is whole.
  assert.match(uploadedVerdict({ assets: [{ name: 'a.zip', size: 3, state: 'starter' }, { name: 'b.apk', size: 5 }] }, names).message, /does not hold a\.zip\.$/);
  assert.match(uploadedVerdict({}, names).message, /does not hold a\.zip, b\.apk\.$/);
});

test('the command answers on one line and exits 1 when the release has to stop', (t) => {
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-release-state-'));
  t.after(() => fs.rmSync(scratch, { recursive: true, force: true }));
  fs.writeFileSync(path.join(scratch, 'a.zip'), 'abc');
  const ask = (input, ...args) => spawnSync(process.execPath, [path.join(repository, 'scripts', 'release-state.mjs'), ...args], { input, encoding: 'utf8' });
  const releases = JSON.stringify([{ tagName: 'v1.3.4', isDraft: false, isPrerelease: false }]);

  assert.deepEqual([ask(JSON.stringify([run()]), 'ci', SHA).status, ask(JSON.stringify([run()]), 'ci', SHA).stdout], [0, `CI passed on ${SHA.slice(0, 12)} (Checks).\n`]);
  assert.equal(ask('[]', 'ci', SHA).status, 1);
  assert.equal(ask(releases, 'state', '1.3.4').stdout, 'published\n');
  assert.deepEqual([ask(releases, 'newer', '1.4.0', '').status, ask(releases, 'newer', '1.4.0', '').stdout], [0, '1.3.4\n']);
  assert.deepEqual([ask('[]', 'newer', '1.4.0').status, ask('[]', 'newer', '1.4.0').stdout], [0, '\n']);
  assert.equal(ask(releases, 'newer', '1.3.4').status, 1);
  const assets = ask(JSON.stringify({ assets: [{ name: 'a.zip', size: 3 }] }), 'assets', path.join(scratch, 'a.zip'));
  assert.deepEqual([assets.status, assets.stdout], [0, 'The draft release holds all 1 assets.\n']);
  // By name, nothing is read from disk: the file named need not be there at all.
  const uploaded = ask(JSON.stringify({ assets: [{ name: 'gone.zip', size: 9, state: 'uploaded' }] }), 'uploaded', 'gone.zip');
  assert.deepEqual([uploaded.status, uploaded.stdout], [0, 'The draft release holds all 1 assets.\n']);
  const short = ask(JSON.stringify({ assets: [] }), 'uploaded', 'gone.zip');
  assert.deepEqual([short.status, short.stdout], [1, 'the draft release does not hold gone.zip.\n']);
  const garbled = ask('release not found', 'state', '1.4.0');
  assert.equal(garbled.status, 1);
  assert.match(garbled.stdout, /^gh printed something that is not JSON/);
});
