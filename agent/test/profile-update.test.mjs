// Updates of the shape the service profiles create (service-profiles.mjs): a
// CLI with no process of its own and no health check, guarded by its busy
// probe alone, which says whether any related process is still running.
//
// Every one of these updates used to fail 20 s in. The stop step asked a
// process that does not exist to stop, then waited for the process probe to
// say it had, and for "none" that probe always answers running. npm was never
// asked to install anything.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import {
  cli, commandService, fakeNpm, fixtureConfig, fixtureData, npmCalls, npmPackageVersion, npmService, readFixture, SRC, withHome, writeConfig, writeFixture,
} from './helpers.mjs';

const profileShape = { process: { type: 'none' }, health: { type: 'none' } };

/** A home with one npm CLI on 1.0.0 and a registry offering 2.0.0. */
async function withNpmCli(body, initial = {}) {
  return withHome(async (home) => {
    const dir = fixtureData(home, { latest: '2.0.0', ...initial });
    const npm = fakeNpm(home, dir);
    writeConfig(home, fixtureConfig(dir, { services: [npmService(dir, npm, profileShape)] }));
    const run = (argv) => cli(argv, { home, nodeArgs: npm.nodeArgs });
    return body({ dir, run, installed: () => npmPackageVersion(npm.prefix, npm.pkg) });
  });
}

/** A home with one command CLI, the native Claude Code shape: a version on each side and an updater, nothing to roll back with. */
async function withCommandCli(body, initial = {}) {
  return withHome(async (home) => {
    const dir = fixtureData(home, { latest: '2.0.0', ...initial });
    writeConfig(home, fixtureConfig(dir, { services: [commandService(dir, { ...profileShape, rollback: undefined })] }));
    return body({ dir, run: (argv) => cli(argv, { home }) });
  });
}

const installs = (dir) => npmCalls(dir).filter((call) => call.startsWith('install '));

test('an npm CLI with nothing running is updated, with no stop to wait for', async () => {
  await withNpmCli(async ({ dir, run, installed }) => {
    const { payload, exitCode } = run(['update']);
    assert.equal(exitCode, 0, JSON.stringify(payload));
    assert.equal(payload.action, 'updated');
    assert.equal(payload.to, '2.0.0');
    assert.equal(installed(), '2.0.0');
    assert.deepEqual(installs(dir).map((call) => call.split(' ').pop()), ['demo-pkg@2.0.0']);
  });
});

test('a related process defers an npm CLI update, with nothing installed', async () => {
  await withNpmCli(async ({ dir, run, installed }) => {
    const { payload, exitCode } = run(['update']);
    assert.equal(exitCode, 0);
    assert.equal(payload.action, 'deferred');
    assert.equal(payload.reasonCode, 'busy');
    assert.equal(payload.outcome, 'deferred');
    assert.deepEqual(installs(dir), []);
    assert.equal(installed(), '1.0.0');
  }, { busy: 'on' });
});

test('a forced npm CLI update defers, not fails, while a related process stays running', async () => {
  await withNpmCli(async ({ dir, run, installed }) => {
    // --force passes the gate, and the stop step is then what keeps the
    // installer off files a session still has open. There is nothing it can
    // stop, so it waits, and when the session outlasts it the update waits too.
    const { payload, exitCode } = run(['update', '--force']);
    assert.equal(exitCode, 0, JSON.stringify(payload));
    assert.equal(payload.action, 'deferred');
    assert.equal(payload.reasonCode, 'busy');
    assert.equal(payload.outcome, 'deferred');
    assert.match(payload.message, /Demo has nothing of its own to stop, and 20 s later something related was still running/);
    assert.match(payload.message, /nothing was installed, and 2\.0\.0 will be installed at the next idle window/);
    assert.deepEqual(installs(dir), []);
    assert.equal(installed(), '1.0.0');
    assert.equal(readFixture(dir, 'stop-runs'), '', 'there was nothing to ask to stop');
  }, { busy: 'on' });
});

test('a command CLI with nothing running is updated, and confirmed by its version', async () => {
  await withCommandCli(async ({ dir, run }) => {
    const { payload, exitCode } = run(['update']);
    assert.equal(exitCode, 0, JSON.stringify(payload));
    assert.equal(payload.action, 'updated');
    assert.equal(payload.to, '2.0.0');
    assert.equal(payload.verified, true);
    assert.equal(readFixture(dir, 'update-runs'), '1');
  });
});

test('a related process defers a command CLI update, with nothing run', async () => {
  await withCommandCli(async ({ dir, run }) => {
    const { payload, exitCode } = run(['update']);
    assert.equal(exitCode, 0);
    assert.equal(payload.action, 'deferred');
    assert.equal(payload.reasonCode, 'busy');
    assert.equal(readFixture(dir, 'update-runs'), '');
    assert.equal(readFixture(dir, 'installed'), '1.0.0');
  }, { busy: 'on' });
});

test('a forced command CLI update waits for a related process to exit, then goes ahead', async () => {
  await withCommandCli(async ({ dir, run }) => {
    // The session ends by itself a few seconds in, inside the stop step's budget.
    const sessionEnds = Date.now() + 4000;
    writeFixture(dir, 'busy-until', sessionEnds);
    const { payload, exitCode } = run(['update', '--force']);
    assert.equal(exitCode, 0, JSON.stringify(payload));
    assert.equal(payload.action, 'updated');
    assert.equal(payload.to, '2.0.0');
    assert.equal(readFixture(dir, 'update-runs'), '1');
    assert.ok(Number(readFixture(dir, 'update-at')) >= sessionEnds, 'the updater ran only once the session had ended');
    assert.equal(readFixture(dir, 'stop-runs'), '');
  });
});

/**
 * What a version of opencode-ai holds once npm has unpacked it: a bin that
 * prints its version, or, when the postinstall that puts the real binary in
 * place never ran, the placeholder that only says so.
 */
function opencodeTree(dir, version, { postinstallRan, runCountsAsSession = false }) {
  const tree = path.join(dir, `npm-tree-${version}`);
  fs.mkdirSync(path.join(tree, 'bin'), { recursive: true });
  fs.writeFileSync(path.join(tree, 'package.json'), JSON.stringify({ name: 'opencode-ai', version, bin: { opencode: 'bin/opencode.js' } }));
  // A run of it that the busy probe takes for a session left open, the way a
  // probe that matches by command line can.
  const session = runCountsAsSession ? `require('node:fs').writeFileSync(${JSON.stringify(path.join(dir, 'busy'))}, 'on');\n` : '';
  fs.writeFileSync(path.join(tree, 'bin', 'opencode.js'), session + (postinstallRan
    ? `console.log(${JSON.stringify(version)});\n`
    : 'console.error("Error: opencode-ai\'s postinstall script was not run."); process.exit(1);\n'));
}

/** OpenCode as its profile describes it, verify included, at the fake npm's prefix. */
async function withOpencode(body, { postinstallRan, runCountsAsSession }) {
  return withHome(async (home) => {
    const dir = fixtureData(home, { latest: '2.0.0' });
    const npm = fakeNpm(home, dir, { pkg: 'opencode-ai' });
    opencodeTree(dir, '1.0.0', { postinstallRan: true });
    opencodeTree(dir, '2.0.0', { postinstallRan, runCountsAsSession });
    const verify = [process.execPath, path.join(SRC, 'service-profile-probe.mjs'), 'npm-verify', 'opencode', npm.prefix];
    writeConfig(home, fixtureConfig(dir, { services: [npmService(dir, npm, { ...profileShape, id: 'opencode', name: 'OpenCode', verify })] }));
    return body({ dir, run: (argv) => cli(argv, { home, nodeArgs: npm.nodeArgs }), installed: () => npmPackageVersion(npm.prefix, npm.pkg) });
  });
}

test('an npm CLI install whose command does not run is rolled back, not reported as updated', async () => {
  await withOpencode(async ({ dir, run, installed }) => {
    const { payload, exitCode } = run(['update']);
    assert.equal(exitCode, 1, JSON.stringify(payload));
    assert.equal(payload.action, 'rolled-back');
    assert.match(payload.message, /2\.0\.0 was installed but does not run: .*opencode --version exited 1: Error: opencode-ai's postinstall script was not run\./);
    assert.match(payload.message, /rolled back to 1\.0\.0$/);
    assert.equal(installed(), '1.0.0');
    assert.deepEqual(installs(dir).map((call) => call.split(' ').pop()), ['opencode-ai@2.0.0', 'opencode-ai@1.0.0']);
  }, { postinstallRan: false });
});

test('an npm CLI install whose command runs is updated, and is then current', async () => {
  await withOpencode(async ({ run, installed }) => {
    const { payload, exitCode } = run(['update']);
    assert.equal(exitCode, 0, JSON.stringify(payload));
    assert.equal(payload.action, 'updated');
    assert.equal(installed(), '2.0.0');

    const again = run(['update']).payload;
    assert.equal(again.action, 'noop');
    assert.equal(again.reasonCode, 'already-on-target');
    assert.equal(again.message, 'already on 2.0.0 and healthy');
  }, { postinstallRan: true });
});

test('an npm CLI install that does not run and could not be rolled back is never reported current after', async () => {
  await withOpencode(async ({ dir, run, installed }) => {
    // The broken command's own run reads as a session, so the rollback waits
    // its 20 s for the session to end and is skipped when it does not.
    const first = run(['update']);
    assert.equal(first.exitCode, 1, JSON.stringify(first.payload));
    assert.equal(first.payload.action, 'failed');
    assert.equal(first.payload.reasonCode, 'apply-failed');
    assert.match(first.payload.message, /^2\.0\.0 was installed but does not run: .*postinstall script was not run\.; rollback skipped: OpenCode has nothing of its own to stop, and 20 s later something related was still running: /);
    assert.equal(installed(), '2.0.0');

    // Nothing in the health of a CLI can tell, so it is asked again, and the
    // answer is the same failure rather than "already on 2.0.0 and healthy".
    writeFixture(dir, 'busy', 'off');
    const second = run(['update']);
    assert.equal(second.exitCode, 1, JSON.stringify(second.payload));
    assert.equal(second.payload.action, 'failed');
    assert.equal(second.payload.reasonCode, 'apply-failed');
    assert.equal(second.payload.outcome, 'failed');
    assert.match(second.payload.message, /^already on 2\.0\.0, but it does not run: .*opencode --version exited 1: Error: opencode-ai's postinstall script was not run\.$/);
    assert.deepEqual(installs(dir).map((call) => call.split(' ').pop()), ['opencode-ai@2.0.0'], 'nothing was installed the second time');
  }, { postinstallRan: false, runCountsAsSession: true });
});

