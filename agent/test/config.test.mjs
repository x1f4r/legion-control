// Config normalization, including the legacy synthesis a machine that predates
// "services" still relies on.

import assert from 'node:assert/strict';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import {
  compilePattern,
  DEFAULT_VERSION_PATTERN,
  LEGACY_DEFAULTS,
  normalizeConfig,
} from '../src/config.mjs';

const BASE = '/tmp/legionctl-test-base';

function load(raw, platform) {
  return normalizeConfig(raw, { platform, base: BASE });
}

test('an empty config on Linux is one T3 Code npm service', () => {
  const config = load({}, 'linux');
  assert.equal(config.legacyServices, true);
  assert.deepEqual(config.system, { id: 'linux', name: 'Linux' });
  assert.equal(config.autoUpdate, true);
  assert.equal(config.services.length, 1);

  const [service] = config.services;
  assert.equal(service.id, 't3');
  assert.equal(service.name, 'T3 Code');
  assert.equal(service.kind, 'npm');
  assert.equal(service.package, 't3');
  assert.equal(service.channel, 'nightly');
  assert.deepEqual(service.process, { type: 'systemd-user', unit: 't3-code.service' });
  assert.deepEqual(service.health, { type: 'http', host: '127.0.0.1', port: 3773, path: '/' });
  assert.equal(service.busy.type, 't3-sqlite');
  assert.equal(service.busy.staleHours, 6);
  assert.deepEqual(service.relay, { type: 'cloudflared' });
  assert.deepEqual(service.allowScripts, LEGACY_DEFAULTS.allowScripts);
  assert.equal(service.lockFile, path.join(BASE, 'update.lock'));
});

test('an empty config on Windows uses the scheduled task and the watchdog lock', () => {
  const config = load({}, 'windows');
  const [service] = config.services;
  assert.deepEqual(config.system, { id: 'windows', name: 'Windows' });
  assert.deepEqual(service.process, { type: 'scheduled-task', task: 'T3 Code Connect', match: null });
  assert.match(service.lockFile, /T3Code[\\/]update\.lock$/);
});

test('an empty config on the Mac is the app, with no relay', () => {
  const config = load({}, 'mac');
  const [service] = config.services;
  assert.deepEqual(config.system, { id: 'mac', name: 'macOS' });
  assert.equal(service.kind, 'app');
  assert.equal(service.path, LEGACY_DEFAULTS.t3AppPath);
  assert.equal(service.bundleId, LEGACY_DEFAULTS.t3AppBundleId);
  assert.equal(service.updaterCacheDir, path.join(os.homedir(), 'Library', 'Caches', 't3code-updater'));
  assert.deepEqual(service.process, { type: 'app' });
  assert.deepEqual(service.latest, { type: 'github-releases', repo: 'pingdotgg/t3code', prerelease: true });
  assert.equal(service.relay, null);
});

test('the legacy top-level keys still describe the service', () => {
  const config = load(
    {
      port: 4000,
      channel: 'stable',
      staleTurnHours: 2,
      allowScripts: ['just-this-one'],
      npmPrefix: '/opt/npm',
      autoUpdate: false,
    },
    'linux',
  );
  const [service] = config.services;
  assert.equal(config.autoUpdate, false);
  assert.equal(service.health.port, 4000);
  assert.equal(service.channel, 'stable');
  assert.equal(service.busy.staleHours, 2);
  assert.deepEqual(service.allowScripts, ['just-this-one']);
  assert.equal(service.npmPrefix, '/opt/npm');
});

test('a legacy nightly config keeps the stricter nightly version pattern', () => {
  const nightly = compilePattern(load({}, 'linux').services[0].versionPattern, null);
  assert.ok(nightly.test('0.0.36-nightly.20260827.1206'));
  assert.ok(!nightly.test('0.0.36'));
  assert.ok(!nightly.test('; rm -rf /'));

  // Any other channel gets the ordinary semver shape instead.
  const stable = compilePattern(load({ channel: 'stable' }, 'linux').services[0].versionPattern, null);
  assert.ok(stable.test('1.2.3'));
  assert.ok(stable.test('1.2.3-rc.1'));
  assert.ok(!stable.test('not a version'));
});

test('an unusable version pattern falls back instead of throwing', () => {
  const compiled = compilePattern('([unclosed', DEFAULT_VERSION_PATTERN);
  assert.ok(compiled.test('1.0.0'));
  assert.equal(compilePattern('([unclosed', null), null);
});

test('nonsense values fall back to their defaults rather than propagating', () => {
  const config = load({ port: 'ninety', staleTurnHours: -4, autoUpdate: 'yes', system: { id: '' } }, 'linux');
  const [service] = config.services;
  assert.equal(service.health.port, 3773);
  assert.equal(service.busy.staleHours, 6);
  assert.equal(config.autoUpdate, true);
  assert.equal(config.system.id, 'linux');
});

test('a services array replaces the legacy synthesis entirely', () => {
  const config = load(
    {
      system: { id: 'cachyos', name: 'CachyOS' },
      services: [
        {
          id: 'sunshine',
          name: 'Sunshine',
          kind: 'command',
          installedVersion: ['sunshine', '--version'],
          update: ['paru', '-S', 'sunshine'],
          process: { type: 'systemd-user', unit: 'sunshine.service' },
          health: { type: 'http', port: 47990 },
          busy: { type: 'none' },
        },
        { id: 'broken', kind: 'nonsense' },
        { id: 'sunshine', name: 'A duplicate id', kind: 'command' },
      ],
    },
    'linux',
  );
  assert.equal(config.legacyServices, false);
  assert.equal(config.services.length, 1);
  const [service] = config.services;
  assert.equal(service.kind, 'command');
  assert.deepEqual(service.installedVersion, ['sunshine', '--version']);
  assert.equal(service.latestVersion, null);
  assert.deepEqual(service.health, { type: 'http', host: '127.0.0.1', port: 47990, path: '/' });
  assert.deepEqual(config.system, { id: 'cachyos', name: 'CachyOS' });
});

test('a command given as a shell string is refused rather than split', () => {
  const config = load(
    { services: [{ id: 'x', kind: 'command', installedVersion: 'x --version', update: ['x', 'up'] }] },
    'linux',
  );
  assert.equal(config.services[0].installedVersion, null);
  assert.deepEqual(config.services[0].update, ['x', 'up']);
});

test('boot targets default per platform and validate their own fields', () => {
  assert.deepEqual(load({}, 'linux').boot.targets, {
    windows: { id: 'windows', name: null, method: 'efi-bootnext', match: '^Windows Boot Manager\\b' },
  });
  assert.deepEqual(load({}, 'windows').boot.targets, {
    linux: { id: 'linux', name: null, method: 'clear-bootsequence' },
  });
  assert.deepEqual(load({}, 'mac').boot.targets, {});

  const configured = load(
    {
      boot: {
        reboot: ['sudo', 'reboot'],
        targets: {
          good: { method: 'grub-reboot', entry: 'CachyOS', name: 'Cachy' },
          missingEntry: { method: 'grub-reboot' },
          unknownMethod: { method: 'wishful-thinking' },
        },
      },
    },
    'linux',
  );
  assert.deepEqual(Object.keys(configured.boot.targets), ['good']);
  assert.deepEqual(configured.boot.reboot, ['sudo', 'reboot']);

  // A targets block with nothing usable in it is the same as none at all, so the
  // system is not left with no way back to the other side.
  const empty = load({ boot: { targets: { bad: { method: 'nope' } } } }, 'linux');
  assert.deepEqual(Object.keys(empty.boot.targets), ['windows']);
});

test('sleep defaults name tools rather than one assumed path', () => {
  const config = load({}, 'windows');
  assert.deepEqual(config.sleep.before, []);
  assert.equal(config.sleep.settle, 2);
  assert.equal(config.sleep.command, null);
  assert.ok(config.sleep.tools.includes('psshutdown64.exe'));
  assert.ok(config.sleep.tools.every((tool) => !tool.toLowerCase().includes('setup')));

  const custom = load(
    { sleep: { before: [['a', 'b'], 'not an argv'], settle: 5, command: ['zzz'], tools: ['D:\\psshutdown.exe'] } },
    'windows',
  );
  assert.deepEqual(custom.sleep.before, [['a', 'b']]);
  assert.equal(custom.sleep.settle, 5);
  assert.deepEqual(custom.sleep.command, ['zzz']);
  assert.deepEqual(custom.sleep.tools, ['D:\\psshutdown.exe']);
});

test('actions need an id and an argv array, and get a default timeout', () => {
  const config = load(
    {
      actions: [
        { id: 'one', name: 'One', command: ['echo', 'hi'] },
        { id: 'two', command: ['echo'], confirm: 'Sure?', busyGated: true, timeoutSeconds: 5 },
        { id: 'three' },
        { command: ['echo'] },
        { id: 'one', command: ['echo', 'again'] },
      ],
    },
    'linux',
  );
  assert.deepEqual(config.actions.map((action) => action.id), ['one', 'two']);
  assert.equal(config.actions[0].timeoutSeconds, 60);
  assert.equal(config.actions[0].busyGated, false);
  assert.equal(config.actions[0].confirm, null);
  assert.equal(config.actions[1].name, 'two');
  assert.equal(config.actions[1].timeoutSeconds, 5);
  assert.equal(config.actions[1].busyGated, true);
});
