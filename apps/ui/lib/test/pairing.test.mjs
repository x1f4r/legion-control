import assert from 'node:assert/strict';
import test from 'node:test';

import { classifyPairError, createPairingController, inviteTargetNode } from '../pairing.js';

function makeFakeInvite(node = 'tower') {
  const json = JSON.stringify({ v: 1, id: 'inv-1', node });
  const b64 = Buffer.from(json, 'utf8')
    .toString('base64')
    .replace(/\+/g, '-')
    .replace(/\//g, '_')
    .replace(/=+$/g, '');
  return `legion-invite-1.${b64}`;
}

test('inviteTargetNode parses node name from invite token', () => {
  assert.equal(inviteTargetNode(makeFakeInvite('tower')), 'tower');
  assert.equal(inviteTargetNode('not-an-invite'), '');
  assert.equal(inviteTargetNode(''), '');
});

test('cancel before send stops quietly without error or saved node', async () => {
  const invite = makeFakeInvite('tower');
  let onPairedCalls = 0;
  let onBgCalls = 0;
  let rejectPair;

  const ctrl = createPairingController({
    link: {
      pair(_inv, _name, { signal }) {
        return new Promise((_resolve, reject) => {
          rejectPair = reject;
          signal.addEventListener('abort', () => {
            reject({
              code: 'cancelled',
              message: 'Enrolment with tower was cancelled.',
              details: { sent: false },
            });
          });
        });
      },
    },
    isOnPairingScreen: () => true,
    onPaired: () => {
      onPairedCalls += 1;
    },
    onBackgroundOutcome: () => {
      onBgCalls += 1;
    },
  });

  const pending = ctrl.startPair({ invite, deviceName: 'phone', source: 'scan', screen: 'start' });
  assert.equal(ctrl.getState().inFlight, true);
  ctrl.cancelPair();
  const res = await pending;
  assert.equal(res.ok, false);
  assert.equal(res.outcome.kind, 'cancelled_before_send');
  assert.equal(ctrl.getState().inFlight, false);
  assert.equal(ctrl.getState().error, null);
  assert.equal(ctrl.getState().notice, null);
  assert.equal(ctrl.getState().invite, '');
  assert.equal(onPairedCalls, 0);
  assert.equal(onBgCalls, 0);
  void rejectPair;
});

test('cancel after send when confirmation already arrived saves pairing and reports Paired with node', async () => {
  const invite = makeFakeInvite('tower');
  const pairedEvents = [];
  let resolvePair;

  const ctrl = createPairingController({
    link: {
      pair() {
        return new Promise((resolve) => {
          resolvePair = resolve;
        });
      },
    },
    isOnPairingScreen: () => true,
    onPaired: (ev) => pairedEvents.push(ev),
  });

  const pending = ctrl.startPair({ invite, deviceName: 'phone', source: 'paste', screen: 'settings' });
  ctrl.cancelPair();
  assert.equal(ctrl.getState().cancelling, true);

  resolvePair({
    ready: true,
    nodes: [{ node: 'tower' }],
    last_paired: { node: 'tower', device: 'phone', role: 'operator', synced: [], not_synced: [] },
  });
  const res = await pending;
  assert.equal(res.ok, true);
  assert.equal(pairedEvents.length, 1);
  assert.equal(pairedEvents[0].node, 'tower');
  assert.equal(pairedEvents[0].wasCancelling, true);
  assert.equal(ctrl.getState().invite, '');
  assert.equal(ctrl.getState().inFlight, false);
});

test('cancel after send when machine had not finished yet stays honest, keeps invite, and allows Try again', async () => {
  const invite = makeFakeInvite('tower');
  const seenInvites = [];
  let attempt = 0;
  const ctrl = createPairingController({
    link: {
      pair(inv, _name, { signal }) {
        seenInvites.push(inv);
        attempt += 1;
        if (attempt === 1) {
          return new Promise((_resolve, reject) => {
            signal.addEventListener('abort', () => {
              reject({
                code: 'cancelled',
                message:
                  'Stopped waiting. tower had not finished pairing when Legion checked. If it finishes, pairing again picks it up.',
                machine: 'tower',
                details: { sent: true, outcome: 'not_finished' },
              });
            });
          });
        }
        return Promise.resolve({
          ready: true,
          nodes: [{ node: 'tower' }],
          last_paired: { node: 'tower', device: 'phone', role: 'operator', synced: [], not_synced: [] },
        });
      },
    },
    isOnPairingScreen: () => true,
  });

  const pending = ctrl.startPair({ invite, deviceName: 'phone', source: 'scan', screen: 'start' });
  ctrl.cancelPair();
  const res = await pending;
  assert.equal(res.ok, false);
  assert.equal(res.outcome.kind, 'not_finished');
  assert.equal(
    ctrl.getState().notice?.title,
    'Stopped waiting. tower had not finished pairing when Legion checked. If it finishes, pairing again picks it up.',
  );
  assert.equal(ctrl.getState().notice?.title.toLowerCase().includes('nothing was saved'), false);
  assert.equal(ctrl.getState().notice?.retryable, true);
  assert.equal(ctrl.getState().invite, invite);

  const retry = await ctrl.retryPair();
  assert.equal(retry.ok, true);
  assert.deepEqual(seenInvites, [invite, invite]);
});

test('cancel after send when machine unreachable keeps scanned invite and Try again reuses it', async () => {
  const invite = makeFakeInvite('tower');
  const seenInvites = [];
  let attempt = 0;

  const ctrl = createPairingController({
    link: {
      pair(inv, _name, { signal }) {
        seenInvites.push(inv);
        attempt += 1;
        if (attempt === 1) {
          return new Promise((_resolve, reject) => {
            signal.addEventListener('abort', () => {
              reject({
                code: 'cancelled',
                message: 'Pairing may have finished on tower. Try again to check.',
                machine: 'tower',
                details: { sent: true, outcome: 'uncertain' },
              });
            });
          });
        }
        return Promise.resolve({
          ready: true,
          nodes: [{ node: 'tower' }],
          last_paired: { node: 'tower', device: 'phone', role: 'operator', synced: [], not_synced: [] },
        });
      },
    },
    isOnPairingScreen: () => true,
  });

  const first = ctrl.startPair({ invite, deviceName: 'phone', source: 'scan', screen: 'start' });
  ctrl.cancelPair();
  const firstRes = await first;
  assert.equal(firstRes.ok, false);
  assert.equal(firstRes.outcome.kind, 'uncertain');
  assert.equal(
    ctrl.getState().notice?.title,
    'Pairing may have finished on tower. Try again to check.',
  );
  assert.equal(ctrl.getState().notice?.retryable, true);
  assert.equal(ctrl.getState().invite, invite);

  // Try again reuses the exact kept scanned invite and completes cleanly.
  const secondRes = await ctrl.retryPair();
  assert.equal(secondRes.ok, true);
  assert.deepEqual(seenInvites, [invite, invite]);
  assert.equal(ctrl.getState().invite, '');
  assert.equal(ctrl.getState().notice, null);
});

test('leaving the page mid-pairing does not abort and surfaces completion or failure where the user is', async () => {
  const invite = makeFakeInvite('tower');
  let onScreen = true;
  let aborted = false;
  let resolveFirst;
  const paired = [];
  const bgOutcomes = [];

  const ctrl = createPairingController({
    link: {
      pair(_inv, _name, { signal }) {
        signal.addEventListener('abort', () => {
          aborted = true;
        });
        return new Promise((resolve, reject) => {
          resolveFirst = { resolve, reject };
        });
      },
    },
    isOnPairingScreen: () => onScreen,
    onPaired: (ev) => paired.push(ev),
    onBackgroundOutcome: (ev) => bgOutcomes.push(ev),
  });

  // 1. User leaves pairing screen mid-pairing; pairing completes in background.
  const p1 = ctrl.startPair({ invite, deviceName: 'phone', source: 'paste', screen: 'settings' });
  onScreen = false;
  assert.equal(aborted, false);
  assert.equal(ctrl.getState().inFlight, true);

  resolveFirst.resolve({
    ready: true,
    nodes: [{ node: 'tower' }],
    last_paired: { node: 'tower', device: 'phone', role: 'operator', synced: [], not_synced: [] },
  });
  await p1;
  assert.equal(aborted, false);
  assert.equal(paired.length, 1);
  assert.equal(paired[0].node, 'tower');
  assert.equal(paired[0].onPairingScreen, false);

  // 2. User leaves pairing screen mid-pairing; pairing fails in background -> onBackgroundOutcome fires.
  onScreen = true;
  const p2 = ctrl.startPair({ invite, deviceName: 'phone', source: 'scan', screen: 'settings' });
  onScreen = false;
  resolveFirst.reject({
    code: 'timeout',
    message: 'Timed out waiting for tower to confirm enrolment after 45s.',
    machine: 'tower',
  });
  await p2;
  assert.equal(bgOutcomes.length, 1);
  assert.equal(bgOutcomes[0].kind, 'error');
  assert.equal(bgOutcomes[0].retryable, true);
  assert.equal(ctrl.getState().invite, invite);
});

test('classifyPairError never swallows post-send cancelled without details.sent=false', () => {
  assert.equal(
    classifyPairError({ code: 'cancelled', details: { sent: false } }, 'tower').kind,
    'cancelled_before_send',
  );
  assert.equal(
    classifyPairError({ code: 'cancelled', details: { sent: true, outcome: 'uncertain' } }, 'tower')
      .kind,
    'uncertain',
  );
  assert.equal(
    classifyPairError({ code: 'cancelled', details: null }, 'tower').kind,
    'not_finished',
  );
  assert.equal(
    classifyPairError({ code: 'cancelled', details: null }, 'tower').retryable,
    true,
  );
  assert.equal(
    classifyPairError({ code: 'cancelled', details: null }, 'tower')
      .message.toLowerCase()
      .includes('nothing was saved'),
    false,
  );
});

test('confirmed conflict detail offers restore pairing via pinned-key probe while older conflict stays plain error', async () => {
  const invite = makeFakeInvite('tower');
  const restoreCalls = [];
  const pairedEvents = [];

  const ctrl = createPairingController({
    link: {
      pair() {
        return Promise.reject({
          code: 'conflict',
          message: 'This device is already paired with tower as device:my-phone.',
          machine: 'tower',
          details: {
            existing_principal: 'device:my-phone',
            existing_device: 'my-phone',
            node: 'tower',
          },
        });
      },
      restorePairing(inv, existingPrincipal, name) {
        restoreCalls.push({ inv, existingPrincipal, name });
        return Promise.resolve({
          ready: true,
          nodes: [{ node: 'tower' }],
          last_paired: { node: 'tower', device: 'my-phone', role: 'operator', synced: [], not_synced: [] },
        });
      },
    },
    getDeviceNoun: () => 'phone',
    isOnPairingScreen: () => true,
    onPaired: (ev) => pairedEvents.push(ev),
  });

  const first = await ctrl.startPair({ invite, deviceName: 'phone', source: 'scan', screen: 'start' });
  assert.equal(first.ok, false);
  assert.equal(first.outcome.kind, 'already_paired');
  assert.equal(
    ctrl.getState().notice?.title,
    'This phone is already paired with tower as device:my-phone.',
  );
  assert.equal(ctrl.getState().notice?.restorable, true);
  assert.equal(ctrl.getState().invite, invite);

  const restored = await ctrl.restorePair();
  assert.equal(restored.ok, true);
  assert.equal(restored.restored, true);
  assert.deepEqual(restoreCalls, [{ inv: invite, existingPrincipal: 'device:my-phone', name: 'phone' }]);
  assert.equal(pairedEvents.length, 1);
  assert.equal(pairedEvents[0].node, 'tower');
  assert.equal(ctrl.getState().invite, '');

  // Conflict marked `restorable: true` without `existing_principal` still offers Restore pairing
  // with a clean sentence (details only improve the sentence).
  const probeOnlyOutcome = classifyPairError(
    {
      code: 'conflict',
      message: 'That public key is already present in authorized_keys.',
      machine: 'tower',
      details: { restorable: true, node: 'tower' },
    },
    'tower',
    'phone',
  );
  assert.equal(probeOnlyOutcome.kind, 'already_paired');
  assert.equal(probeOnlyOutcome.restorable, true);
  assert.equal(probeOnlyOutcome.message, 'This phone is already paired with tower.');

  // Conflict WITHOUT `details` or `restorable` (for example a foreign SSH key) stays a normal error.
  const olderOutcome = classifyPairError(
    {
      code: 'conflict',
      message: 'That public key is already present in authorized_keys.',
      machine: 'tower',
      details: null,
    },
    'tower',
    'phone',
  );
  assert.equal(olderOutcome.kind, 'error');
  assert.equal(olderOutcome.restorable, false);
});
