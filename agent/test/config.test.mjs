// Configuration loading, and the rule that shapes all of it:
// no file at all is INERT; a file that is present and wrong is FATAL.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { DEFAULT_VERSION_PATTERN, LEGACY_DEFAULTS, normalizeConfig } from '../src/config.mjs';
import { fresh, withHome } from './helpers.mjs';

const BASE = '/tmp/legionctl-test-base';
const load = (raw, platform, present = true) => normalizeConfig(raw, { platform, base: BASE, present });

test('a machine with no config looks after nothing and does not update itself', () => {
  const { ok, config } = load({}, 'linux', false);
  assert.equal(ok, true);
  assert.equal(config.inert, true);
  assert.deepEqual(config.services, [], 'an unconfigured machine must not invent a service to look after');
  assert.equal(config.updates.automatic, false, 'an unconfigured machine must not enable its own scheduler');
  assert.equal(config.autoUpdate, false);
  assert.deepEqual(config.boot.targets, {}, 'a boot target is never guessed from the platform');
});

test('a modern config with no boot block gets no boot targets', () => {
  const { config } = load({ services: [], boot: undefined }, 'linux');
  assert.deepEqual(config.boot.targets, {}, 'only a legacy config may inherit the old platform assumptions');
});

test('a config still using the pre-services keys keeps working, with a migration note', () => {
  const { ok, config, migrations } = load({ port: 3773, channel: 'nightly' }, 'linux');
  assert.equal(ok, true);
  assert.equal(config.legacyServices, true);
  assert.equal(config.services.length, 1);
  assert.equal(config.services[0].package, 't3');
  assert.equal(config.services[0].versionPattern, LEGACY_DEFAULTS.nightlyPattern);
  // ...and only a legacy config gets the old boot assumption.
  assert.equal(config.boot.targets.windows.method, 'efi-bootnext');
  assert.match(migrations.map((entry) => entry.message).join(' '), /predates the "services" list/);
});

test('a service that does not say how to tell it is busy is not treated as idle', () => {
  const { config, migrations } = load(
    { services: [{ id: 'x', kind: 'command', update: ['/bin/true'], process: { type: 'command', running: ['/bin/true'] } }] },
    'linux',
  );
  assert.equal(config.services[0].busy.type, 'unmonitored');
  assert.match(migrations.map((entry) => entry.message).join(' '), /does not say how to tell whether it is busy/);
});

test('an explicit busy type of none is a decision, and is not warned about', () => {
  const { config, migrations, warnings } = load(
    {
      services: [
        { id: 'x', kind: 'command', update: ['/bin/true'], verify: ['/bin/true'], busy: { type: 'none' }, process: { type: 'command', running: ['/bin/true'] } },
      ],
    },
    'linux',
  );
  assert.equal(config.services[0].busy.type, 'none');
  assert.equal([...migrations, ...warnings].some((entry) => /busy/.test(entry.message)), false);
});

test('an unknown probe type is a configuration error, never a silent fallback', () => {
  for (const [block, where] of [
    [{ busy: { type: 'comand' } }, 'busy'],
    [{ health: { type: 'htp' } }, 'health'],
    [{ process: { type: 'systemd' } }, 'process'],
  ]) {
    const { ok, errors } = load({ services: [{ id: 'x', kind: 'command', ...block }] }, 'linux');
    assert.equal(ok, false, `${where} should have been rejected`);
    assert.match(errors.map((entry) => entry.path).join(' '), new RegExp(where));
  }
});

test('a command given as a string says how to write it as an array', () => {
  const { ok, errors } = load({ services: [{ id: 'x', kind: 'command', update: 'systemctl restart x' }] }, 'linux');
  assert.equal(ok, false);
  const problem = errors.find((entry) => entry.path.endsWith('.update'));
  assert.match(problem.message, /must be an array/);
  assert.match(problem.fix, /\["systemctl", "restart", "x"\]/);
});

test('a typo in a key is refused rather than ignored', () => {
  const { ok, errors } = load({ services: [{ id: 'x', kind: 'npm', pakage: 't3' }] }, 'linux');
  assert.equal(ok, false);
  assert.match(errors.map((entry) => entry.message).join(' '), /unknown setting "pakage"/);
});

test('autoUpdate and updates.automatic cannot disagree', () => {
  const conflicting = load({ autoUpdate: false, updates: { automatic: true }, services: [] }, 'linux');
  assert.equal(conflicting.ok, false);
  assert.match(conflicting.errors.map((entry) => entry.message).join(' '), /same setting/);

  // The 2.x spelling on its own still works, and sets the same thing.
  const legacy = load({ autoUpdate: false, services: [] }, 'linux');
  assert.equal(legacy.ok, true);
  assert.equal(legacy.config.updates.automatic, false);
});

test('a maintenance window is validated, including its days and times', () => {
  const good = load({ updates: { maintenanceWindows: [{ days: ['Mon', 'tue'], from: '02:00', to: '06:00' }] }, services: [] }, 'linux');
  assert.equal(good.ok, true);
  assert.deepEqual(good.config.updates.maintenanceWindows[0], { days: ['mon', 'tue'], from: '02:00', to: '06:00' });

  for (const window of [{ days: ['funday'] }, { from: '25:00' }, { to: 'noon' }]) {
    const bad = load({ updates: { maintenanceWindows: [window] }, services: [] }, 'linux');
    assert.equal(bad.ok, false, `${JSON.stringify(window)} should have been rejected`);
  }
});

test('a wake action is validated, and cannot also be a command', () => {
  const good = load({ actions: [{ id: 'wake', wol: { mac: 'aa-bb-cc-dd-ee-ff', broadcast: ['10.0.0.255'] } }], services: [] }, 'linux');
  assert.equal(good.ok, true);
  assert.equal(good.config.actions[0].kind, 'wol');
  assert.equal(good.config.actions[0].wol.mac, 'AA:BB:CC:DD:EE:FF');
  assert.deepEqual(good.config.actions[0].wol.ports, [9, 7]);

  for (const action of [
    { id: 'a', wol: { mac: 'nope', broadcast: ['10.0.0.255'] } },
    { id: 'a', wol: { mac: 'aa:bb:cc:dd:ee:ff' } },
    { id: 'a', wol: { mac: 'aa:bb:cc:dd:ee:ff', broadcast: ['10.0.0.255'] }, command: ['/bin/true'] },
    { id: 'a' },
  ]) {
    assert.equal(load({ actions: [action], services: [] }, 'linux').ok, false, `${JSON.stringify(action)} should have been rejected`);
  }
});

test('an id that could not travel over ssh is refused', () => {
  const { ok, errors } = load({ services: [{ id: 'a b; rm -rf ~', kind: 'command', update: ['/bin/true'] }] }, 'linux');
  assert.equal(ok, false);
  assert.match(errors.map((entry) => entry.message).join(' '), /cannot be passed safely over SSH/);
});

test('a command service whose update cannot be confirmed is warned about', () => {
  const { warnings } = load(
    { services: [{ id: 'x', kind: 'command', update: ['/bin/true'], busy: { type: 'none' }, process: { type: 'command', running: ['/bin/true'] } }] },
    'linux',
  );
  assert.match(warnings.map((entry) => entry.message).join(' '), /cannot be checked/);
});

test('requireVersionMatch defaults on only when both versions can be read', () => {
  const both = load(
    { services: [{ id: 'x', kind: 'command', installedVersion: ['/bin/true'], latestVersion: ['/bin/true'], update: ['/bin/true'] }] },
    'linux',
  );
  assert.equal(both.config.services[0].requireVersionMatch, true);
  const neither = load({ services: [{ id: 'x', kind: 'command', update: ['/bin/true'], verify: ['/bin/true'] }] }, 'linux');
  assert.equal(neither.config.services[0].requireVersionMatch, false);
});

test('a config from a newer agent is refused rather than half understood', () => {
  const { ok, errors } = load({ configVersion: 99, services: [] }, 'linux');
  assert.equal(ok, false);
  assert.match(errors[0].fix, /update the agent/);
});

test('a truncated config does not load as the enabled defaults', async () => {
  await withHome(async (home) => {
    fs.writeFileSync(path.join(home, 'config.json'), '{"autoUpdate":false');
    const config = await fresh('config.mjs');
    const loaded = config.loadConfig();
    assert.equal(loaded.ok, false);
    assert.equal(loaded.config, null, 'a broken config must not become a working one');
    assert.match(loaded.error, /does not parse/);
  });
});

test('a config that loads cleanly is kept as the last good copy, but not by a read-only load', async () => {
  await withHome(async (home) => {
    const document = { configVersion: 3, updates: { automatic: false }, services: [], boot: { targets: {} } };
    fs.writeFileSync(path.join(home, 'config.json'), JSON.stringify(document));
    const config = await fresh('config.mjs');

    config.loadConfig({ readOnly: true });
    assert.equal(fs.existsSync(config.lastGoodConfigPath()), false, 'a read-only load must change nothing');

    config.loadConfig();
    assert.equal(fs.existsSync(config.lastGoodConfigPath()), true);

    // ...and a later broken edit points at it rather than guessing.
    fs.writeFileSync(path.join(home, 'config.json'), '{ broken');
    const broken = config.loadConfig();
    assert.equal(broken.ok, false);
    assert.equal(broken.source, 'last-known-good');
    assert.match(broken.errors[0].fix, /loaded cleanly/);
  });
});

test('saveConfig refuses to write over a file it could not read', async () => {
  await withHome(async (home) => {
    fs.writeFileSync(path.join(home, 'config.json'), '{ broken');
    const config = await fresh('config.mjs');
    const saved = config.saveConfig({ autoUpdate: true });
    assert.equal(saved.ok, false);
    assert.match(saved.error, /refusing to overwrite/);
    assert.match(fs.readFileSync(path.join(home, 'config.json'), 'utf8'), /broken/);
  });
});

test('saveConfig refuses a change that would make the config unusable', async () => {
  await withHome(async (home) => {
    fs.writeFileSync(path.join(home, 'config.json'), JSON.stringify({ services: [], boot: { targets: {} } }));
    const config = await fresh('config.mjs');
    const saved = config.saveConfig({ updates: { automatic: 'yes please' } });
    assert.equal(saved.ok, false);
    assert.match(saved.error, /nothing was written/);
  });
});

test('the platform defaults still describe each system when a legacy config asks for them', () => {
  assert.equal(load({ port: 3773 }, 'windows').config.services[0].process.type, 'scheduled-task');
  assert.equal(load({ port: 3773 }, 'mac').config.services[0].kind, 'app');
  assert.equal(load({ port: 3773 }, 'mac').config.services[0].versionPattern, DEFAULT_VERSION_PATTERN);
  assert.match(load({ port: 3773 }, 'windows').config.services[0].lockFile, /T3Code[\\/]update\.lock$/);
});
