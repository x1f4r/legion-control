// Updates of the shape the service profiles create (service-profiles.mjs): a
// CLI with no process of its own and no health check, guarded by its busy
// probe alone, which says whether any related process is still running.
//
// Every one of these updates used to fail 20 s in. The stop step asked a
// process that does not exist to stop, then waited for the process probe to
// say it had, and for "none" that probe always answers running. npm was never
// asked to install anything.

import assert from 'node:assert/strict';
import test from 'node:test';
import {
  cli, commandService, fakeNpm, fixtureConfig, fixtureData, npmCalls, npmPackageVersion, npmService, readFixture, withHome, writeConfig, writeFixture,
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
    writeFixture(dir, 'busy-until', Date.now() + 4000);
    const { payload, exitCode } = run(['update', '--force']);
    assert.equal(exitCode, 0, JSON.stringify(payload));
    assert.equal(payload.action, 'updated');
    assert.equal(payload.to, '2.0.0');
    assert.equal(readFixture(dir, 'update-runs'), '1');
    assert.equal(readFixture(dir, 'stop-runs'), '');
  });
});
