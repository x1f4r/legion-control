// The self-updating app path of the updater, as far as it can be driven
// without an app: everything it decides before it would ask one to quit.
//
// Every updater test used to drive the command-kind fixture, so none of these
// endings was held to anything. The quit itself is AppleScript sent to a real
// app and is never sent from a test; what comes before it is the part that
// keeps a background timer from quitting the app somebody is working in, for
// a build that is not there, cannot be compared, or will never apply. Where a
// test goes as far as the quit, a stand-in osascript answers it (helpers.mjs).

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { cli, fixtureConfig, fixtureData, standInApp, withHome, writeConfig } from './helpers.mjs';

// The bundle's version is read with PlistBuddy, which only a Mac has.
const mac = process.platform === 'darwin';

/** An app-kind service whose bundle and updater cache live in the test's home. */
function appService(home, overrides = {}) {
  return {
    id: 'demo',
    name: 'Demo',
    kind: 'app',
    path: path.join(home, 'Demo.app'),
    bundleId: 'com.example.demo',
    updaterCacheDir: path.join(home, 'updater'),
    stagedFilePattern: '^Demo-(.+)\\.zip$',
    health: { type: 'none' },
    ...overrides,
  };
}

/** The bundle on disk, at `version`, the way the app's Info.plist says it. */
function bundleAt(home, version) {
  const contents = path.join(home, 'Demo.app', 'Contents');
  fs.mkdirSync(contents, { recursive: true });
  fs.writeFileSync(path.join(contents, 'Info.plist'), [
    '<?xml version="1.0" encoding="UTF-8"?>',
    '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">',
    '<plist version="1.0"><dict>',
    '<key>CFBundleIdentifier</key><string>com.example.demo</string>',
    `<key>CFBundleShortVersionString</key><string>${version}</string>`,
    '</dict></plist>',
    '',
  ].join('\n'));
}

/** A build the app's updater has downloaded and holds for its next quit. */
function staged(home, version) {
  const pending = path.join(home, 'updater', 'pending');
  fs.mkdirSync(pending, { recursive: true });
  fs.writeFileSync(path.join(pending, 'update-info.json'), JSON.stringify({ fileName: `Demo-${version}.zip` }));
}

function withApp(body) {
  return withHome(async (home) => {
    writeConfig(home, fixtureConfig(fixtureData(home), { services: [appService(home)] }));
    return body(home);
  });
}

test('an app with nothing staged is left alone, as a no-op', async () => {
  await withApp(async (home) => {
    const { payload, exitCode } = cli(['update'], { home });
    assert.equal(exitCode, 0);
    assert.equal(payload.action, 'noop');
    assert.equal(payload.reasonCode, 'no-update');
    assert.match(payload.message, /nothing is staged: Demo has not downloaded an update/);
  });
});

test('a staged build is never applied over a bundle whose version cannot be read', async () => {
  await withApp(async (home) => {
    staged(home, '2.0.0');
    const { payload, exitCode } = cli(['update'], { home });
    assert.equal(exitCode, 1);
    assert.equal(payload.action, 'failed');
    assert.equal(payload.reasonCode, 'not-installed');
    assert.equal(payload.from, null);
    assert.equal(payload.to, '2.0.0');
    assert.match(payload.message, /cannot tell whether 2\.0\.0 is waiting.*so Demo was left alone/);
  });
});

test('a staged build that is the one already running is nothing to apply', { skip: !mac && 'PlistBuddy reads the bundle version' }, async () => {
  await withApp(async (home) => {
    bundleAt(home, '2.0.0');
    staged(home, '2.0.0');
    const { payload } = cli(['update'], { home });
    assert.equal(payload.action, 'noop');
    assert.equal(payload.from, '2.0.0');
    assert.match(payload.message, /the downloaded build \(2\.0\.0\) is the one already running/);
  });
});

test('a build staged for a closed app waits for the app, and is never applied by opening it', { skip: !mac && 'PlistBuddy reads the bundle version' }, async () => {
  await withApp(async (home) => {
    bundleAt(home, '1.0.0');
    staged(home, '2.0.0');
    const { payload, exitCode } = cli(['update'], { home });
    assert.equal(exitCode, 0);
    assert.equal(payload.action, 'deferred');
    assert.equal(payload.reasonCode, 'app-closed');
    assert.equal(payload.outcome, 'deferred');
    assert.deepEqual([payload.from, payload.to], ['1.0.0', '2.0.0']);
  });
});

test('a build that failed to apply three times is not tried again unless forced', { skip: !mac && 'PlistBuddy reads the bundle version' }, async () => {
  await withApp(async (home) => {
    bundleAt(home, '1.0.0');
    staged(home, '2.0.0');
    fs.mkdirSync(path.join(home, 'state'), { recursive: true });
    fs.writeFileSync(path.join(home, 'state', 'service-demo.json'), JSON.stringify({
      applyFailures: { version: '2.0.0', count: 3, at: '2026-01-01T00:00:00.000Z' },
    }));
    const exhausted = cli(['update'], { home }).payload;
    assert.equal(exhausted.action, 'failed');
    assert.equal(exhausted.reasonCode, 'apply-attempts-exhausted');
    assert.match(exhausted.message, /2\.0\.0 failed to apply 3 times, so it will not be retried automatically/);
    // Forced, it goes past the count, as far as the app being closed.
    const forced = cli(['update', '--force'], { home }).payload;
    assert.equal(forced.reasonCode, 'app-closed');
  });
});

test('an app that will not quit has had nothing applied, and is not an update that failed while applied', { skip: !mac && 'PlistBuddy reads the bundle version' }, async () => {
  await withHome(async (home) => {
    bundleAt(home, '1.0.0');
    staged(home, '2.0.0');
    writeConfig(home, fixtureConfig(fixtureData(home), { services: [appService(home, { busy: { type: 'none' } })] }));
    const app = standInApp(home, { code: 1 });
    try {
      await new Promise((resolve) => setTimeout(resolve, 300));
      const { payload, exitCode } = cli(['update'], { home, env: app.env });
      assert.equal(exitCode, 1, JSON.stringify(payload));
      assert.equal(payload.action, 'failed');
      assert.equal(payload.reasonCode, 'internal');
      assert.equal(payload.outcome, 'failed');
      assert.match(payload.message, /^the quit request failed \(.*User canceled.*\) and Demo is still running; it was left alone rather than forced; nothing was applied, and 2\.0\.0 applies by itself the next time Demo quits$/);
      assert.deepEqual([payload.from, payload.to], ['1.0.0', '2.0.0']);
      assert.match(app.calls(), /quit app id "com\.example\.demo"/);
    } finally {
      app.stop();
    }
  });
});
