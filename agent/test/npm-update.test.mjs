// The npm path of the updater, end to end: the agent the way a client runs it,
// a service it can start and stop, and an npm that installs into the test's
// own prefix (fake-npm-cli.mjs).
//
// Every updater test used to drive the command-kind fixture, and the npm path
// went untested. It had drifted from the command path in the one place the
// order matters: a drain the service refused, or a stop it did not complete,
// was answered with a rollback, which stopped the service that had just said
// it was not ready and reinstalled the version it was still running. These
// hold every ending it can reach to what it leaves behind.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { pathToFileURL } from 'node:url';
import {
  cli, fakeNpm, FIXTURE, fixtureConfig, fixtureData, npmCalls, npmPackageVersion, npmService, readFixture, withHome, writeConfig,
} from './helpers.mjs';

/** A home with one npm service on 1.0.0, a registry offering `latest`, and whatever the scenario sets. */
async function withNpmService(body, { initial = {}, service = () => ({}) } = {}) {
  return withHome(async (home) => {
    const dir = fixtureData(home, { latest: '2.0.0', ...initial });
    const npm = fakeNpm(home, dir);
    writeConfig(home, fixtureConfig(dir, { services: [npmService(dir, npm, service(dir))] }));
    const run = (argv) => cli(argv, { home, nodeArgs: npm.nodeArgs });
    return body({ home, dir, npm, run, installed: () => npmPackageVersion(npm.prefix, npm.pkg) });
  });
}

const drain = (dir) => ({ drain: { command: [process.execPath, FIXTURE, 'drain', dir], timeoutSeconds: 30 } });
const installs = (dir) => npmCalls(dir).filter((call) => call.startsWith('install '));

test('an npm update installs, verifies and starts the version it resolved', async () => {
  await withNpmService(async ({ dir, run, installed }) => {
    const { payload, exitCode } = run(['update']);
    assert.equal(exitCode, 0, JSON.stringify(payload));
    assert.equal(payload.action, 'updated');
    assert.equal(payload.from, '1.0.0');
    assert.equal(payload.to, '2.0.0');
    assert.equal(payload.verified, true);
    assert.equal(installed(), '2.0.0');
    assert.deepEqual(installs(dir).map((call) => call.split(' ').pop()), ['demo-pkg@2.0.0']);
    assert.ok(npmCalls(dir).includes('cache add demo-pkg@2.0.0'), 'the download was prepared before the stop');
    assert.equal(readFixture(dir, 'drain-runs'), '1');
    assert.equal(readFixture(dir, 'stop-runs'), '1');
    assert.equal(readFixture(dir, 'start-runs'), '1');
    assert.equal(readFixture(dir, 'running'), 'on');
  }, { service: drain });
});

test('a drain the service refuses ends an npm update with nothing stopped, installed or rolled back', async () => {
  await withNpmService(async ({ dir, run, installed }) => {
    const { payload, exitCode } = run(['update']);
    assert.equal(exitCode, 1);
    assert.equal(payload.action, 'failed');
    assert.equal(payload.reasonCode, 'busy');
    assert.equal(payload.outcome, 'failed');
    assert.match(payload.message, /the drain command failed/);
    // The service said it was not ready. It is left exactly as it was: still
    // running, on the version it had, with no second install over it.
    assert.equal(readFixture(dir, 'stop-runs'), '', 'nothing stopped the service');
    assert.deepEqual(installs(dir), [], 'nothing was installed, not even the version it was running');
    assert.equal(readFixture(dir, 'start-runs'), '');
    assert.equal(readFixture(dir, 'running'), 'on');
    assert.equal(installed(), '1.0.0');
  }, { initial: { 'drain-fails': 'on' }, service: drain });
});

test('work that starts during the drain defers an npm update before the stop', async () => {
  await withNpmService(async ({ dir, run, installed }) => {
    const { payload, exitCode } = run(['update']);
    assert.equal(exitCode, 0);
    assert.equal(payload.action, 'deferred');
    assert.equal(payload.reasonCode, 'busy');
    assert.match(payload.message, /work started while the update was being prepared/);
    assert.equal(readFixture(dir, 'drain-runs'), '1');
    assert.equal(readFixture(dir, 'stop-runs'), '', 'the check after the drain is the one immediately before the stop');
    assert.deepEqual(installs(dir), []);
    assert.equal(installed(), '1.0.0');
  }, { initial: { 'drain-makes-busy': 'on' }, service: drain });
});

test('an npm service that will not stop is left running, with nothing installed or rolled back', async () => {
  await withNpmService(async ({ dir, run, installed }) => {
    const { payload, exitCode } = run(['update']);
    assert.equal(exitCode, 1);
    assert.equal(payload.action, 'failed');
    assert.equal(payload.reasonCode, 'not-running');
    assert.match(payload.message, /would not stop/);
    // One stop was asked for. A rollback used to ask for a second one, wait
    // out its own budget, and start the service over itself.
    assert.equal(readFixture(dir, 'stop-runs'), '1');
    assert.equal(readFixture(dir, 'start-runs'), '');
    assert.deepEqual(installs(dir), []);
    assert.equal(readFixture(dir, 'running'), 'on');
    assert.equal(installed(), '1.0.0');
  }, { initial: { 'stop-refuses': 'on' } });
});

test('an npm install that fails is rolled back to the version that was running, and started on it', async () => {
  await withNpmService(async ({ dir, run, installed }) => {
    const { payload, exitCode } = run(['update']);
    assert.equal(exitCode, 1);
    assert.equal(payload.action, 'rolled-back');
    assert.equal(payload.reasonCode, 'rolled-back');
    assert.match(payload.message, /could not install demo-pkg@2\.0\.0.*rolled back to 1\.0\.0/);
    // The failed install took the old tree with it, so the rollback is what
    // put 1.0.0 back on disk.
    assert.deepEqual(installs(dir).map((call) => call.split(' ').pop()), ['demo-pkg@2.0.0', 'demo-pkg@1.0.0']);
    assert.equal(installed(), '1.0.0');
    assert.equal(readFixture(dir, 'running'), 'on');
    assert.equal(readFixture(dir, 'start-runs'), '1');
  }, { initial: { 'npm-install-fails': '2.0.0' } });
});

test('an npm update that cannot record its install starts the service again, and installs nothing', async () => {
  await withNpmService(async ({ home, dir, npm, installed }) => {
    // The disk fills up just as the record is about to say "installing": the
    // service is stopped by then, and nothing has been written over it.
    const stand = path.join(home, 'record-fills-up.mjs');
    fs.writeFileSync(stand, [
      "import fs from 'node:fs';",
      'const rename = fs.renameSync;',
      'let failed = false;',
      'fs.renameSync = (from, to) => {',
      "  if (!failed && String(to).endsWith('.json') && fs.readFileSync(from, 'utf8').includes('\"phase\": \"installing\"')) {",
      '    failed = true;',
      "    throw Object.assign(new Error('no space left on device'), { code: 'ENOSPC' });",
      '  }',
      '  return rename.call(fs, from, to);',
      '};',
      '',
    ].join('\n'));
    const { payload, exitCode } = cli(['update'], { home, nodeArgs: [...npm.nodeArgs, '--import', pathToFileURL(stand).href] });
    assert.equal(exitCode, 1);
    assert.equal(payload.action, 'failed');
    assert.equal(payload.reasonCode, 'internal');
    assert.match(payload.message, /nothing was installed, and Demo was started again/);
    // Only an install that began is rolled back; this one never did.
    assert.deepEqual(installs(dir), []);
    assert.equal(installed(), '1.0.0');
    assert.equal(readFixture(dir, 'stop-runs'), '1');
    assert.equal(readFixture(dir, 'start-runs'), '1');
    assert.equal(readFixture(dir, 'running'), 'on');
  });
});
