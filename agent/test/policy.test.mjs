// The three separate questions: may the scheduler act, did a person ask, and may
// this run interrupt work in progress.

import assert from 'node:assert/strict';
import test from 'node:test';
import { normalizeConfig } from '../src/config.mjs';
import {
  describeServiceUpdates,
  effectivePolicy,
  inAnyWindow,
  inWindow,
  nextWindowStart,
  orderServices,
  scheduledEligibility,
  validatePolicyPatch,
} from '../src/policy.mjs';
import { withHome } from './helpers.mjs';

const service = (id, updates) => ({
  id,
  kind: 'command',
  update: ['/bin/true'],
  verify: ['/bin/true'],
  busy: { type: 'none' },
  process: { type: 'command', running: ['/bin/true'] },
  ...(updates ? { updates } : {}),
});

const build = (document) => normalizeConfig(document, { platform: 'linux', base: '/tmp/x' }).config;

// Monday 2026-09-07 at 03:00 local, inside an 02:00-06:00 window.
const insideWindow = new Date(2026, 8, 7, 3, 0, 0);
const outsideWindow = new Date(2026, 8, 7, 12, 0, 0);

test('a window matches only on its own days and hours', () => {
  const window = { days: ['mon'], from: '02:00', to: '06:00' };
  assert.equal(inWindow(window, insideWindow), true);
  assert.equal(inWindow(window, outsideWindow), false);
  assert.equal(inWindow(window, new Date(2026, 8, 8, 3, 0, 0)), false, 'Tuesday is not Monday');
});

test('a window that crosses midnight belongs to the day it started on', () => {
  const window = { days: ['fri'], from: '22:00', to: '04:00' };
  assert.equal(inWindow(window, new Date(2026, 8, 4, 23, 0, 0)), true, 'Friday evening');
  assert.equal(inWindow(window, new Date(2026, 8, 5, 2, 0, 0)), true, 'the early hours of Saturday');
  assert.equal(inWindow(window, new Date(2026, 8, 4, 2, 0, 0)), false, "Friday's own small hours are not in it");
});

test('no windows at all means any time', () => {
  assert.equal(inAnyWindow([], outsideWindow), true);
  assert.equal(inAnyWindow(null, outsideWindow), true);
});

test('the next window is reported when the current time is outside every one', () => {
  const windows = [{ days: ['mon'], from: '02:00', to: '06:00' }];
  assert.equal(nextWindowStart(windows, insideWindow), null, 'inside a window there is nothing to wait for');
  const next = nextWindowStart(windows, outsideWindow);
  assert.ok(next, 'a machine outside its window should be able to say when it opens');
  assert.equal(new Date(next).getDay(), 1);
});

test('a service inherits the machine policy until it states its own', async () => {
  await withHome(async () => {
    const config = build({ updates: { automatic: true }, services: [service('a'), service('b', { automatic: false })] });
    const inherited = effectivePolicy(config, config.services[0]);
    assert.equal(inherited.automatic, true);
    assert.equal(inherited.inherited, true);

    const own = effectivePolicy(config, config.services[1]);
    assert.equal(own.automatic, false);
    assert.equal(own.inherited, false);
  });
});

test('the machine switch is a master stop the scheduler cannot be talked out of', async () => {
  await withHome(async () => {
    // A service that turns its own automatic ON cannot re-enable a schedule the
    // machine has turned off: "off" on the machine has to mean off.
    const config = build({ updates: { automatic: false }, services: [service('a', { automatic: true })] });
    const eligibility = scheduledEligibility(config, config.services[0], {});
    assert.equal(eligibility.eligible, false);
    assert.equal(eligibility.reasonCode, 'policy-off');
  });
});

test('a manual update stays available while the scheduler is off', async () => {
  await withHome(async () => {
    const config = build({ updates: { automatic: false }, services: [service('a')] });
    const described = describeServiceUpdates(config, config.services[0], {});
    assert.equal(described.eligibleNow, false);
    assert.equal(described.deferredReason, 'policy-off');
    assert.equal(described.manualAvailable, true, 'turning the schedule off must not remove the safe manual path');
  });
});

test('a pause and a window each decline with their own reason', async () => {
  await withHome(async () => {
    const paused = build({ updates: { automatic: true }, services: [service('a', { pauseUntil: '2030-01-01T00:00:00Z' })] });
    assert.equal(scheduledEligibility(paused, paused.services[0], {}).reasonCode, 'policy-paused');

    const windowed = build({
      updates: { automatic: true, maintenanceWindows: [{ days: ['mon'], from: '02:00', to: '06:00' }] },
      services: [service('a')],
    });
    const declined = scheduledEligibility(windowed, windowed.services[0], { now: outsideWindow });
    assert.equal(declined.reasonCode, 'outside-window');
    assert.ok(declined.nextWindow);
    assert.equal(scheduledEligibility(windowed, windowed.services[0], { now: insideWindow }).eligible, true);
  });
});

test('a pause that has passed stops mattering', async () => {
  await withHome(async () => {
    const config = build({ updates: { automatic: true }, services: [service('a', { pauseUntil: '2020-01-01T00:00:00Z' })] });
    assert.equal(scheduledEligibility(config, config.services[0], {}).eligible, true);
  });
});

test('a service with nothing to update is declined before any policy question', async () => {
  await withHome(async () => {
    const config = build({
      updates: { automatic: true },
      services: [{ id: 'a', kind: 'command', busy: { type: 'none' }, process: { type: 'command', running: ['/bin/true'] } }],
    });
    assert.equal(scheduledEligibility(config, config.services[0], {}).reasonCode, 'no-update');
  });
});

test('a busy service is eligible in policy but not eligible now', async () => {
  await withHome(async () => {
    const config = build({ updates: { automatic: true }, services: [service('a')] });
    const described = describeServiceUpdates(config, config.services[0], {
      busy: { busy: true, unknown: false },
    });
    assert.equal(described.eligibleNow, false);
    assert.equal(described.deferredReason, 'busy');

    const unknown = describeServiceUpdates(config, config.services[0], { busy: { busy: true, unknown: true } });
    assert.equal(unknown.deferredReason, 'busy-unknown');
  });
});

test('services are walked in dependency order, and a cycle is reported not resolved', () => {
  const config = build({
    services: [service('web', { after: ['db'], order: 1 }), service('db', { order: 5 }), service('cache')],
  });
  const { ordered, cycles } = orderServices(config.services);
  assert.deepEqual(cycles, []);
  assert.ok(ordered.findIndex((s) => s.id === 'db') < ordered.findIndex((s) => s.id === 'web'));

  const circular = [
    { id: 'a', updates: { after: ['b'], order: 0 } },
    { id: 'b', updates: { after: ['a'], order: 0 } },
  ];
  assert.ok(orderServices(circular).cycles.length > 0, 'a cycle has no correct order, so it is reported');
});

test('a policy patch is validated before anything is written', () => {
  assert.deepEqual(validatePolicyPatch({ automatic: false }, { perService: false }).patch, { automatic: false });
  assert.equal(validatePolicyPatch({ automatic: null }, { perService: true }).patch.automatic, null, 'a service may inherit');
  assert.equal(validatePolicyPatch({ automatic: null }, { perService: false }).ok, false, 'the machine has nothing to inherit from');
  assert.equal(validatePolicyPatch({ automatic: 'yes' }, { perService: false }).ok, false);
  assert.equal(validatePolicyPatch({ pauseUntil: 'soon' }, { perService: false }).ok, false);
  assert.equal(validatePolicyPatch({ nonsense: 1 }, { perService: false }).ok, false);
  assert.equal(validatePolicyPatch({ maintenanceWindows: [{ days: ['xyz'] }] }, { perService: false }).ok, false);
  assert.equal(validatePolicyPatch({ maintenanceWindows: [] }, { perService: false }).ok, true);
  // A patch names only what it changes, so a pause does not silently clear the windows.
  assert.deepEqual(Object.keys(validatePolicyPatch({ pauseUntil: '2030-01-01T00:00:00Z' }, { perService: true }).patch), ['pauseUntil']);
});
