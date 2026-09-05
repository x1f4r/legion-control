// Arming the next boot: parsing what the firmware says, and refusing to reboot
// on anything less than proof that the arming took.

import assert from 'node:assert/strict';
import test from 'node:test';
import { armNextBoot, bootTargets, entryDescription, matchEfiEntry, parseEfiBootManager, selectSleepCommand, suspendAccepted } from '../src/boot.mjs';
import { normalizeConfig } from '../src/config.mjs';
import { withHome } from './helpers.mjs';

// Portable stand-ins for true and false: /bin/true does not exist everywhere.
const SUCCEEDS = [process.execPath, '-e', 'process.exit(0)'];
const FAILS = [process.execPath, '-e', 'process.exit(1)'];

const EFI_OUTPUT = [
  'BootCurrent: 0001',
  'Timeout: 1 seconds',
  'BootOrder: 0001,0002,0000',
  'Boot0000* CachyOS\tHD(1,GPT,abc)/File(\\EFI\\cachyos\\grubx64.efi)',
  'Boot0001* Windows Boot Manager\tHD(1,GPT,def)/File(\\EFI\\Microsoft\\Boot\\bootmgfw.efi)',
  'Boot0002* Windows Recovery Environment\tHD(4,GPT,ghi)/File(\\EFI\\Microsoft\\Recovery\\x.efi)',
  'BootNext: 0001',
].join('\n');

test('efibootmgr output is parsed into entries and the armed BootNext', () => {
  const parsed = parseEfiBootManager(EFI_OUTPUT);
  assert.equal(parsed.bootNext, '0001');
  assert.equal(parsed.entries.length, 3);
  assert.deepEqual(parsed.entries[1], { number: '0001', description: 'Windows Boot Manager' });
});

test('the device path is stripped from an entry description', () => {
  assert.equal(entryDescription('Windows Boot Manager\tHD(1,GPT,x)/File(y)'), 'Windows Boot Manager');
  assert.equal(entryDescription('Fedora  PciRoot(0x0)/Pci(0x1)'), 'Fedora');
});

test('a match has to be anchored, or it picks the recovery entry', () => {
  const { entries } = parseEfiBootManager(EFI_OUTPUT);
  const anchored = matchEfiEntry(entries, '^Windows Boot Manager\\b');
  assert.equal(anchored.ok, true);
  assert.equal(anchored.entry.number, '0001');
  assert.equal(anchored.ambiguous, false);

  // A loose pattern matches the recovery environment too, and a BootNext into
  // that comes up in a menu with no network and no ssh.
  const loose = matchEfiEntry(entries, 'Windows');
  assert.equal(loose.ambiguous, true, 'an ambiguous match has to be reported as one');

  assert.equal(matchEfiEntry(entries, 'Haiku').ok, false);
  assert.match(matchEfiEntry(entries, '(unclosed').message, /not a valid regular expression/);
});

test('a command boot target with no verify cannot be armed for a reboot', async () => {
  await withHome(async () => {
    const config = normalizeConfig(
      { services: [], boot: { targets: { other: { method: 'command', arm: SUCCEEDS } } } },
      { platform: 'linux', base: '/tmp/x' },
    ).config;

    // A reboot is going to follow, and nothing here can prove the arming took.
    const refused = armNextBoot(config, 'other', { requireVerification: true });
    assert.equal(refused.ok, false);
    assert.equal(refused.verified, false);
    assert.match(refused.message, /no verify command/);
    assert.match(refused.message, /--force overrides the busy gate, not the evidence/);

    // Arming alone, with nobody about to lose their session over it, is allowed.
    const armed = armNextBoot(config, 'other', { requireVerification: false });
    assert.equal(armed.ok, true);
    assert.equal(armed.verified, false);
  });
});

test('a command boot target with a verify command is armed and read back', async () => {
  await withHome(async () => {
    const config = normalizeConfig(
      { services: [], boot: { targets: { other: { method: 'command', arm: SUCCEEDS, verify: SUCCEEDS } } } },
      { platform: 'linux', base: '/tmp/x' },
    ).config;
    const armed = armNextBoot(config, 'other', { requireVerification: true });
    assert.equal(armed.ok, true);
    assert.equal(armed.verified, true);
  });
});

test('an arm command that fails is never followed by a reboot', async () => {
  await withHome(async () => {
    const config = normalizeConfig(
      { services: [], boot: { targets: { other: { method: 'command', arm: FAILS, verify: SUCCEEDS } } } },
      { platform: 'linux', base: '/tmp/x' },
    ).config;
    const armed = armNextBoot(config, 'other', { requireVerification: true });
    assert.equal(armed.ok, false);
    assert.match(armed.message, /arm command failed/);
  });
});

test('an arm that runs but does not verify is a failure', async () => {
  await withHome(async () => {
    const config = normalizeConfig(
      { services: [], boot: { targets: { other: { method: 'command', arm: SUCCEEDS, verify: FAILS } } } },
      { platform: 'linux', base: '/tmp/x' },
    ).config;
    const armed = armNextBoot(config, 'other', { requireVerification: true });
    assert.equal(armed.ok, false);
    assert.match(armed.message, /verification failed/);
  });
});

test('boot targets are listed from the config, and an unknown one is refused', () => {
  const config = normalizeConfig(
    { services: [], boot: { targets: { windows: { method: 'efi-bootnext', match: '^Windows', name: 'Windows 11' } } } },
    { platform: 'linux', base: '/tmp/x' },
  ).config;
  assert.deepEqual(bootTargets(config), [{ id: 'windows', name: 'Windows 11' }]);
  assert.equal(armNextBoot(config, 'haiku').ok, false);
});

test('the suspend command is chosen per platform, and Windows resolves a real tool', () => {
  assert.deepEqual(selectSleepCommand('linux', {}).command, ['sudo', '-n', 'systemctl', 'suspend']);
  assert.deepEqual(selectSleepCommand('mac', {}).command, ['pmset', 'sleepnow']);

  // The built-in Windows suspend calls are silently vetoed on some machines: they
  // exit 0 and the machine stays awake. So the tool is resolved, never assumed.
  const found = selectSleepCommand('windows', { tools: ['psshutdown64.exe'] }, { exists: () => false, onPath: () => 'C:\\Tools\\psshutdown64.exe' });
  assert.deepEqual(found.command, ['C:\\Tools\\psshutdown64.exe', '-d', '-t', '3', '-accepteula']);

  const missing = selectSleepCommand('windows', { tools: ['nope.exe'] }, { exists: () => false, onPath: () => null });
  assert.equal(missing.ok, false);
  assert.match(missing.message, /no suspend tool found/);

  assert.deepEqual(selectSleepCommand('linux', { command: ['my-suspend'] }).command, ['my-suspend']);
});

test('a suspend that was accepted is never described as one that happened', () => {
  assert.match(suspendAccepted('pmset'), /accepted, not confirmed/);
});
