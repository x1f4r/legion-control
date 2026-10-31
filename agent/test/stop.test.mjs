// What a stop that did not complete says, and the code it says it with.
//
// Every one of them used to be reported alike. An update answered not-running,
// which every client words as the opposite of what happened, and for a while
// busy, which they word as nothing having been touched, though a stop request
// had been sent. What may follow is what tells them apart: a stop nothing could
// ask for, or one refused while the service kept running, leaves it up as it
// was; one asked for and not seen through may still be under way, so its
// outcome is unknown and no client sends the update again.
//
// The budgets here are a couple of seconds, not the 20 s an update gives a
// stop, so nothing in this file waits that out.

import assert from 'node:assert/strict';
import path from 'node:path';
import test from 'node:test';
import { quitAndWait } from '../src/providers/app.mjs';
import { stopAndWait } from '../src/service.mjs';
import { cli, commandService, FIXTURE, fixtureConfig, fixtureData, readFixture, standInApp, withHome, writeConfig } from './helpers.mjs';

const mac = process.platform === 'darwin';
const call = (dir, verb) => [process.execPath, FIXTURE, verb, dir];

test('a stop nothing could ask for is not configured, and is not waited out', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home);
    const service = commandService(dir, { process: { type: 'command', running: call(dir, 'running'), start: call(dir, 'start') } });
    const startedAt = Date.now();
    const stopped = await stopAndWait(service, 5000);
    assert.equal(stopped.ok, false);
    assert.equal(stopped.reasonCode, 'not-configured');
    assert.equal(stopped.message, 'Demo has no stop command configured, so it could not be stopped');
    assert.ok(Date.now() - startedAt < 4000, 'nothing was asked to stop, so nothing is going to');
  });
});

test('a stop with nothing to ask for still goes ahead when the service is already down', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home, { running: 'off' });
    const service = commandService(dir, { process: { type: 'command', running: call(dir, 'running'), start: call(dir, 'start') } });
    assert.deepEqual(await stopAndWait(service, 5000), { ok: true, message: null });
  });
});

test('a stop request that failed while the service kept running is a refusal, not a timeout', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home, { 'stop-refuses': 'on' });
    const stopped = await stopAndWait(commandService(dir), 2000);
    assert.equal(stopped.ok, false);
    assert.equal(stopped.reasonCode, 'internal');
    assert.equal(stopped.message, `the stop request failed (${path.basename(process.execPath)} failed (exit 1)), and Demo was still running 2 s later`);
    assert.equal(readFixture(dir, 'stop-runs'), '1');
  });
});

test('a stop that was asked for and not seen through has timed out, since it may still be under way', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home, { 'stop-ignored': 'on' });
    const stopped = await stopAndWait(commandService(dir), 2000);
    assert.equal(stopped.reasonCode, 'timed-out');
    assert.equal(stopped.message, 'Demo was still running 2 s after the stop request');
  });

  await withHome(async (home) => {
    // A process nothing here can read: whether it stopped is not known either.
    const dir = fixtureData(home);
    const stopped = await stopAndWait(commandService(dir, { process: { type: 'command', start: call(dir, 'start'), stop: call(dir, 'stop') } }), 2000);
    assert.equal(stopped.reasonCode, 'timed-out');
    assert.match(stopped.message, /^Demo was asked to stop, and 2 s later whether it had could not be established: Demo has no running command/);
  });
});

test('a service with nothing to stop whose busy probe stays unreadable says so, not that something ran', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home, { 'busy-unknown': 'on' });
    const service = commandService(dir, { process: { type: 'none' }, health: { type: 'none' } });
    const stopped = await stopAndWait(service, 2000);
    assert.equal(stopped.ok, false);
    assert.equal(stopped.busy.unknown, true);
    assert.equal(stopped.message, 'Demo has nothing of its own to stop, and 2 s later whether anything related was still running could not be established: the session list could not be read');
  });
});

test('a restart whose stop could not be asked for is not configured, not a service that is not running', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home);
    const service = commandService(dir, { process: { type: 'command', running: call(dir, 'running'), start: call(dir, 'start') } });
    writeConfig(home, fixtureConfig(dir, { services: [service] }));
    const { payload, exitCode } = cli(['restart'], { home });
    assert.equal(exitCode, 1, JSON.stringify(payload));
    assert.equal(payload.action, 'failed');
    assert.equal(payload.reasonCode, 'not-configured');
    assert.equal(payload.outcome, 'failed');
    assert.equal(payload.message, 'Demo has no stop command configured, so it could not be stopped');
    assert.equal(readFixture(dir, 'start-runs'), '', 'it was running all along');
  });
});

/** Run `body` with the stand-in osascript first on this process's own PATH. */
async function withStandInApp(home, options, body) {
  const app = standInApp(home, options);
  const previous = process.env.PATH;
  process.env.PATH = app.env.PATH;
  try {
    await new Promise((resolve) => setTimeout(resolve, 300));
    return await body(app);
  } finally {
    process.env.PATH = previous;
    app.stop();
  }
}

const appService = (home, overrides = {}) => ({ id: 'demo', name: 'Demo', kind: 'app', path: path.join(home, 'Demo.app'), bundleId: 'com.example.demo', ...overrides });

test('an app quit says which of the three it was', { skip: !mac && 'ps reports a process by its bundle path only on a Mac' }, async () => {
  await withHome(async (home) => {
    await withStandInApp(home, { code: 1 }, async (app) => {
      const refused = await quitAndWait(appService(home), 10000);
      assert.equal(refused.reasonCode, 'internal');
      assert.match(refused.message, /^the quit request failed \(.*\) and Demo is still running; it was left alone rather than forced$/);

      const unasked = await quitAndWait(appService(home, { bundleId: null }), 10000);
      assert.equal(unasked.reasonCode, 'not-configured');
      assert.equal(unasked.message, 'Demo could not be asked to quit: no bundle id is configured for this service; it was left running');
      assert.equal(app.calls().split('\n').length, 1, 'the app with no bundle id was never asked');
    });
  });

  await withHome(async (home) => {
    await withStandInApp(home, { code: 0 }, async () => {
      // Asked, and the app has not gone: a dialog of its own may be waiting
      // for somebody, and the quit can still come.
      const pending = await quitAndWait(appService(home), 2000);
      assert.equal(pending.reasonCode, 'timed-out');
      assert.match(pending.message, /^Demo did not quit within 2 s; it was left running on purpose/);
    });
  });
});
