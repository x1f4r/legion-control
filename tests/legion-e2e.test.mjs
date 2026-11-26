import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import path from 'node:path';
import test from 'node:test';
import {
  blockerOf,
  buildOpenSshEd25519Key,
  buildTestPng,
  decodeInvite,
  emergencyAbort,
  hasImageMagic,
  realPowerRefusal,
  redactSecrets,
  shouldRunStep,
  windowsForcedCommandError,
} from '../scripts/legion-e2e.mjs';

// scripts/legion-e2e.mjs restarts the machine it runs on if calling the restart off fails, so the
// guards around its real power steps are tested here, where nothing is scheduled at all.

test('--real-power is refused unless CI=true says this is a throwaway runner', () => {
  for (const env of [{}, { CI: 'false' }, { CI: '1' }, { CI: 'TRUE' }]) {
    assert.match(realPowerRefusal({ realPower: true }, env), /runs only where CI=true/, JSON.stringify(env));
  }
  assert.equal(realPowerRefusal({ realPower: true }, { CI: 'true' }), null);
  assert.equal(realPowerRefusal({ realPower: false }, {}), null);
});

test('the emergency abort is the operating system\'s own, and on Unix kills only the far node\'s runner', () => {
  assert.deepEqual(emergencyAbort(true, 'C:\\a\\far'), ['shutdown', ['/a']]);
  const farHome = path.join(path.sep, 'home', 'runner', 'work', '_temp', 'legion-e2e+1', 'far');
  const [program, args] = emergencyAbort(false, farHome);
  assert.equal(program, 'pkill');
  assert.deepEqual(args.slice(0, 2), ['-f', '--']);
  const pattern = new RegExp(args[2]);
  const runnerDir = path.join(farHome, 'state', 'power');
  assert.ok(pattern.test(`${path.join(path.sep, 'home', 'runner', '.local', 'bin', 'legion')} power-delay ${runnerDir} 19a2b-1f2e`));
  // Not the driver, whose command line names the far home but never next to the runner's mode.
  assert.ok(!pattern.test(`node scripts/legion-e2e.mjs --far-home ${farHome} --real-power`));
  // Not a runner of another node.
  assert.ok(!pattern.test(`legion power-delay ${path.join(path.sep, 'home', 'me', '.local', 'state', 'legion', 'power')} 1-2`));
});

test('a step whose prerequisite failed or did not run is skipped too', () => {
  const l1 = 'l1 without [power] sudo, a restart this account may not do is refused';
  const l2 = 'l2 a real restart is pending and power.cancel calls it off';
  // l1 failed: l2 waits on it, and so does l4 on l2, which never ran.
  assert.equal(blockerOf(['l1'], [l1], []), l1);
  assert.equal(blockerOf(['l2'], [l1], [l2]), l2);
  // Skipped as asked counts the same.
  assert.equal(blockerOf(['l2'], [], [l2]), l2);
  assert.equal(blockerOf(['l2'], [l1], []), undefined);
  assert.equal(blockerOf([], [l1], [l2]), undefined);
});

test('decodeInvite and buildOpenSshEd25519Key handle the pairing invite contract', () => {
  const seed = crypto.randomBytes(32);
  const payload = {
    v: 1,
    id: '0123456789abcdef0123456789abcdef',
    node: 'tower',
    hosts: ['tower.example'],
    port: 22,
    user: 'me',
    host_keys: ['ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl'],
    key: seed.toString('base64'),
    principal: 'phone',
    expires: '2026-11-24T12:15:00Z',
  };
  const encoded = `legion-invite-1.${Buffer.from(JSON.stringify(payload), 'utf8').toString('base64url')}`;
  assert.deepEqual(decodeInvite(encoded), payload);
  assert.throws(() => decodeInvite('not-an-invite'));

  const pem = buildOpenSshEd25519Key(seed, 'test-key');
  assert.ok(pem.startsWith('-----BEGIN OPENSSH PRIVATE KEY-----\n'));
  assert.ok(pem.endsWith('-----END OPENSSH PRIVATE KEY-----\n'));
  const raw = Buffer.from(
    pem.replace(/-----.*-----\n/g, '').replace(/\n/g, ''),
    'base64',
  );
  assert.equal(raw.subarray(0, 15).toString('ascii'), 'openssh-key-v1\0');

  // Redaction strips invite strings, the decoded base64 seed remembered by decodeInvite, and PEM keys.
  const leaked = `invite=${encoded}\npayload=${JSON.stringify(payload)}\npem=${pem}`;
  const redacted = redactSecrets(leaked);
  assert.ok(!redacted.includes(encoded), 'invite text leaked');
  assert.ok(!redacted.includes(payload.key), 'decoded seed leaked');
  assert.ok(!redacted.includes('BEGIN OPENSSH PRIVATE KEY'), 'private key leaked');
  assert.ok(redacted.includes('legion-invite-1.<redacted>'));
});

test('windowsForcedCommandError enforces PowerShell vs cmd forced-command quoting', () => {
  const key = 'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl';
  const psLine = `restrict,command="& 'C:\\Users\\me\\legion.exe' gate --as 'e2e:gate' --device e2e-phone" ${key} legion-device:e2e-phone\n`;
  const cmdLine = `restrict,command="C:\\Users\\me\\legion.exe gate --as e2e:gate --device e2e-phone" ${key} legion-device:e2e-phone\n`;

  assert.equal(windowsForcedCommandError(psLine, 'legion-device:e2e-phone', 'powershell'), null);
  assert.match(
    windowsForcedCommandError(cmdLine, 'legion-device:e2e-phone', 'powershell'),
    /expected PowerShell forced command/,
  );

  assert.equal(windowsForcedCommandError(cmdLine, 'legion-device:e2e-phone', 'cmd'), null);
  assert.match(
    windowsForcedCommandError(psLine, 'legion-device:e2e-phone', 'cmd'),
    /expected cmd\.exe forced command/,
  );
  assert.match(
    windowsForcedCommandError('# empty\n', 'legion-device:e2e-phone', 'powershell'),
    /does not contain/,
  );
});

test('shouldRunStep combines --only and --skip prefix filters', () => {
  assert.equal(shouldRunStep('b6 meta.doctor', [], []), true);
  assert.equal(shouldRunStep('b6 meta.doctor', ['a', 'b'], []), true);
  assert.equal(shouldRunStep('c1 meta.version', ['a', 'b'], []), false);
  assert.equal(shouldRunStep('b6 meta.doctor', ['a', 'b'], ['b6']), false);
});

test('buildTestPng produces a valid PNG recognized by hasImageMagic', () => {
  const png = buildTestPng(32, 24);
  assert.equal(hasImageMagic(png, 'image/png'), true);
  assert.equal(hasImageMagic(png, 'image/jpeg'), false);
  assert.equal(hasImageMagic(Buffer.from([0xff, 0xd8, 0xff, 0xe0, 0, 0, 0, 0]), 'image/jpeg'), true);
  assert.equal(hasImageMagic(Buffer.from('not an image'), 'image/png'), false);
});
