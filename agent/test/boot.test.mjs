// Reading efibootmgr, picking the entry a target names, and choosing the
// command that actually suspends a machine.

import assert from 'node:assert/strict';
import test from 'node:test';
import { entryDescription, matchEfiEntry, parseEfiBootManager, selectSleepCommand } from '../src/boot.mjs';

// Real efibootmgr output, including the device path glued to the description
// that the parser has to cut away.
const EFIBOOTMGR = `BootCurrent: 0002
Timeout: 0 seconds
BootOrder: 0002,0000,0001,2001,2002
Boot0000* Windows Boot Manager	HD(1,GPT,1e2c8b7a-1c2f-4a6e-9d1e-7e6a5c4b3a21,0x800,0x32000)/\\EFI\\Microsoft\\Boot\\bootmgfw.efi
Boot0001  Windows Recovery Environment	HD(4,GPT,9f8e7d6c-5b4a-3928-1706-fedcba987654,0x1d1c000,0x100000)
Boot0002* Limine	HD(1,GPT,1e2c8b7a-1c2f-4a6e-9d1e-7e6a5c4b3a21,0x800,0x32000)/\\EFI\\limine\\BOOTX64.EFI
Boot2001* EFI USB Device	RC
`;

test('efibootmgr output becomes entries and the armed BootNext', () => {
  const listing = parseEfiBootManager(`BootNext: 0000\n${EFIBOOTMGR}`);
  assert.equal(listing.bootNext, '0000');
  assert.deepEqual(
    listing.entries.map((entry) => entry.number),
    ['0000', '0001', '0002', '2001'],
  );
  assert.deepEqual(
    listing.entries.map((entry) => entry.description),
    ['Windows Boot Manager', 'Windows Recovery Environment', 'Limine', 'EFI USB Device'],
  );
});

test('nothing armed reads as nothing armed, not as an error', () => {
  assert.equal(parseEfiBootManager(EFIBOOTMGR).bootNext, null);
  assert.deepEqual(parseEfiBootManager('').entries, []);
});

test('the device path is cut off the description', () => {
  assert.equal(entryDescription('Limine\tHD(1,GPT,abc,0x800,0x32000)/\\EFI\\limine\\BOOTX64.EFI'), 'Limine');
  assert.equal(entryDescription('EFI Network 0 PciRoot(0x0)/Pci(0x1c,0x4)'), 'EFI Network 0');
  assert.equal(entryDescription('Plain Entry'), 'Plain Entry');
});

test('an anchored match picks the boot manager and never the recovery entry', () => {
  const { entries } = parseEfiBootManager(EFIBOOTMGR);
  const picked = matchEfiEntry(entries, '^Windows Boot Manager\\b');
  assert.equal(picked.ok, true);
  assert.equal(picked.entry.number, '0000');
  assert.equal(picked.ambiguous, false);

  // The pattern that would have been a trip to the machine itself: "windows"
  // alone also matches the recovery environment.
  const loose = matchEfiEntry(entries, 'windows');
  assert.equal(loose.ok, true);
  assert.equal(loose.ambiguous, true);
});

test('a match that resolves to nothing says what it did find', () => {
  const { entries } = parseEfiBootManager(EFIBOOTMGR);
  const missing = matchEfiEntry(entries, '^No Such Thing$');
  assert.equal(missing.ok, false);
  assert.match(missing.message, /Windows Boot Manager, Windows Recovery Environment, Limine/);

  const broken = matchEfiEntry(entries, '([unclosed');
  assert.equal(broken.ok, false);
  assert.match(broken.message, /not a valid regular expression/);
});

const NOTHING = { exists: () => false, onPath: () => null };

test('each platform has a suspend command that needs no configuration', () => {
  assert.deepEqual(selectSleepCommand('linux', {}, NOTHING).command, ['sudo', '-n', 'systemctl', 'suspend']);
  assert.deepEqual(selectSleepCommand('mac', {}, NOTHING).command, ['pmset', 'sleepnow']);
});

test('a configured command wins over every default', () => {
  const chosen = selectSleepCommand('linux', { command: ['systemctl', 'hibernate'] }, NOTHING);
  assert.deepEqual(chosen.command, ['systemctl', 'hibernate']);
});

test('Windows takes the first tool that is really there, with -t 3 and not -t 0', () => {
  const probe = {
    exists: (candidate) => candidate === 'C:\\Tools\\PSTools\\psshutdown.exe',
    onPath: () => null,
  };
  const chosen = selectSleepCommand(
    'windows',
    { tools: ['psshutdown64.exe', 'C:\\Tools\\PSTools\\psshutdown64.exe', 'C:\\Tools\\PSTools\\psshutdown.exe'] },
    probe,
  );
  assert.deepEqual(chosen.command, ['C:\\Tools\\PSTools\\psshutdown.exe', '-d', '-t', '3', '-accepteula']);
});

test('a bare tool name is resolved on PATH', () => {
  const probe = { exists: () => false, onPath: (name) => (name === 'psshutdown.exe' ? 'C:\\bin\\psshutdown.exe' : null) };
  const chosen = selectSleepCommand('windows', { tools: ['psshutdown64.exe', 'psshutdown.exe'] }, probe);
  assert.deepEqual(chosen.command, ['C:\\bin\\psshutdown.exe', '-d', '-t', '3', '-accepteula']);
});

test('no tool at all refuses rather than pretending the machine will sleep', () => {
  const chosen = selectSleepCommand('windows', { tools: ['psshutdown.exe'] }, NOTHING);
  assert.equal(chosen.ok, false);
  assert.equal(chosen.command, null);
  assert.match(chosen.message, /no suspend tool found/);
});
