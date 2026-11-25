import assert from 'node:assert/strict';
import test from 'node:test';

import {
  containsTechnicalText,
  formatErrorForDisplay,
  isTerminalHint,
  plainErrorMessage,
  plainMessageForCode,
} from '../errors.js';

test('formatErrorForDisplay produces the exact plain sentences and hides reasons/codes/hints when technical is off', () => {
  // Unreachable / timeout
  for (const code of ['unreachable', 'timeout']) {
    const plain = formatErrorForDisplay({
      error: {
        code,
        message: 'TCP connect to 192.0.2.9:22 timed out.',
        hint: 'Check Tailscale or run `legion fleet.status`.',
        machine: 'studio',
        details: { reason: 'timeout' },
      },
      technical: false,
    });
    assert.equal(plain.heading, "Can't reach studio right now.");
    assert.equal(plain.detail, null);
    assert.equal(plain.machinePrefix, '');

    const tech = formatErrorForDisplay({
      error: {
        code,
        message: 'TCP connect to 192.0.2.9:22 timed out.',
        hint: 'Check Tailscale or run `legion fleet.status`.',
        machine: 'studio',
        details: { reason: 'timeout' },
      },
      technical: true,
    });
    assert.equal(tech.heading, 'TCP connect to 192.0.2.9:22 timed out.');
    assert.match(tech.detail, /code: (?:unreachable|timeout)/);
    assert.match(tech.detail, /reason: timeout/);
    assert.match(tech.detail, /legion fleet\.status/);
    assert.equal(tech.machinePrefix, 'studio: ');
  }

  // Relay too old
  const oldRelay = {
    code: 'unsupported',
    message: 'tower needs an update before it can pass files along from studio.',
    hint: 'Update Legion on tower (or learn a direct route to studio).',
    machine: 'tower',
    details: { reason: 'relay_too_old', relay: 'tower', target: 'studio' },
  };
  const plainRelay = formatErrorForDisplay({ error: oldRelay, technical: false });
  assert.equal(
    plainRelay.heading,
    'tower needs an update before it can pass files along from studio.',
  );
  assert.equal(plainRelay.detail, null);
  assert.equal(plainRelay.machinePrefix, '');

  const techRelay = formatErrorForDisplay({ error: oldRelay, technical: true });
  assert.match(techRelay.detail, /code: unsupported/);
  assert.match(techRelay.detail, /reason: relay_too_old/);

  // Unknown machine
  const unknown = {
    code: 'not_found',
    message: "This device doesn't know a machine called studio.",
    hint: 'Check fleet.toml on the relaying machine.',
    machine: 'tower',
    details: { reason: 'unknown_machine', target: 'studio' },
  };
  const plainUnknown = formatErrorForDisplay({ error: unknown, technical: false });
  assert.equal(plainUnknown.heading, "This device doesn't know a machine called studio.");
  assert.equal(plainUnknown.detail, null);
  assert.equal(plainUnknown.machinePrefix, '');
  assert.ok(!plainUnknown.heading.includes('not paired'));

  const techUnknown = formatErrorForDisplay({ error: unknown, technical: true });
  assert.match(techUnknown.detail, /code: not_found/);
  assert.match(techUnknown.detail, /reason: unknown_machine/);
  assert.match(techUnknown.detail, /fleet\.toml/);

  // Exists (for example atomic `put` with `if_exists: "fail"`)
  const exists = {
    code: 'exists',
    message: '/home/me/report.pdf already exists.',
    hint: 'Pass if_exists="replace", "skip", or "rename", or choose another path.',
    machine: 'studio',
  };
  const plainExists = formatErrorForDisplay({ error: exists, technical: false });
  assert.equal(plainExists.heading, 'Something with that name already exists.');
  assert.equal(plainExists.detail, null);
  assert.equal(plainExists.machinePrefix, '');

  const techExists = formatErrorForDisplay({ error: exists, technical: true });
  assert.equal(techExists.heading, '/home/me/report.pdf already exists.');
  assert.match(techExists.detail, /if_exists/);
  assert.equal(techExists.machinePrefix, 'studio: ');
  assert.equal(isTerminalHint(exists.hint), true);

  // Device removed or unknown
  const removed = {
    code: 'denied',
    message: 'SSH authentication rejected.',
    machine: 'studio',
    details: { reason: 'device_removed_or_unknown' },
  };
  const plainRemoved = formatErrorForDisplay({
    error: removed,
    technical: false,
    deviceNoun: 'phone',
  });
  assert.equal(
    plainRemoved.heading,
    'studio no longer lets this phone in. It may have been removed there.',
  );
  assert.equal(plainRemoved.canRepair, true);
  assert.equal(plainRemoved.detail, null);
});

test('plainErrorMessage strips technical hints and jargon in plain mode and preserves raw text in technical mode', () => {
  const err = {
    code: 'denied',
    message: 'Principal "device:pixel" (role "observer") is not allowed to call `fs.write`.',
    hint: 'Edit policy.toml on studio.',
    machine: 'studio',
  };
  assert.equal(
    plainErrorMessage(err, 'Fallback.', { technical: false, deviceNoun: 'phone' }),
    'This phone is not allowed to do that here.',
  );
  assert.equal(
    plainErrorMessage(err, 'Fallback.', { technical: true, deviceNoun: 'phone' }),
    'studio: Principal "device:pixel" (role "observer") is not allowed to call `fs.write`.',
  );

  assert.equal(isTerminalHint('Run `legion fleet.sync` to refresh.'), true);
  assert.equal(isTerminalHint('Try again in a moment.'), false);
  assert.equal(containsTechnicalText('Unknown field `foo` in policy.toml'), true);
  assert.equal(containsTechnicalText('File could not be opened.'), false);
  assert.equal(plainMessageForCode('unreachable', { machine: 'studio' }), "Can't reach studio right now.");
});
