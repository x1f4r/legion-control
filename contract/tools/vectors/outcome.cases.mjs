// The outcome reducer of v3.md section 5.6, as cases.
//
// `reduce` feeds the function the fields it reads, one combination per case,
// including the shapes 3.0.x and 2.x agents really sent: a conflict with ok
// false, a deferral with no reason code at all. `records` feeds it whole
// operation records and summaries, the way a client meets them inside `op`,
// `history` and `status`, so a port that reduces a record from the wrong field
// fails here rather than in front of somebody whose update it misreported.

import { at } from '../documents.mjs';

const EVERYONE = ['agent', 'mac', 'desktop', 'android'];
const CLIENTS = ['mac', 'desktop', 'android'];

const reduce = (name, input, outcome, purpose, tags = []) => ({
  name, purpose, tags: ['finding:c5', ...tags], input, want: { outcome },
});

const SUCCESS_ACTIONS = ['updated', 'restarted', 'ran', 'rebooted', 'slept', 'armed', 'installed', 'cycled', 'checked', 'skipped', 'stored', 'replaced'];

// A whole operation record and a whole summary, so a client can hand the value
// to the decoder it uses in production rather than to a test-only shape.
function record(overrides) {
  return {
    id: 'unset',
    kind: 'update',
    service: 'demo',
    target: null,
    actionId: null,
    mode: 'manual',
    initiator: { client: 'mac', device: 'laptop', user: 'me' },
    state: 'finished',
    phase: 'done',
    progress: null,
    requestedAt: at(-10),
    startedAt: at(-10),
    updatedAt: at(-9),
    finishedAt: at(-9),
    expiresAt: null,
    pid: null,
    detached: false,
    result: null,
    log: [],
    agentVersion: '3.1.0',
    systemId: 'linux',
    ...overrides,
  };
}

function summary(overrides) {
  return {
    id: 'unset',
    kind: 'update',
    service: 'demo',
    target: null,
    actionId: null,
    mode: 'manual',
    state: 'finished',
    phase: 'done',
    action: null,
    reasonCode: null,
    requestedAt: at(-10),
    updatedAt: at(-9),
    expiresAt: null,
    ...overrides,
  };
}

const RUNNING = { state: 'running', phase: 'installing', finishedAt: null, pid: 24817, detached: true, result: null };

const records = [
  {
    name: 'record-running-without-result',
    purpose: 'A running record with no result yet and no stated outcome. The state alone makes it pending, so the client keeps polling.',
    tags: ['finding:c5'],
    input: { kind: 'record', value: record({ id: '0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d', ...RUNNING }) },
    want: { outcome: 'pending' },
  },
  {
    name: 'record-queued',
    purpose: 'A queued record: no pid, no start, an expiry. Work the agent still owns is pending.',
    tags: ['finding:c5'],
    input: {
      kind: 'record',
      value: record({
        id: '1b2c3d4e-5f6a-4b7c-9d8e-0f1a2b3c4d5e', kind: 'restart', mode: 'queued', state: 'queued', phase: 'queued',
        startedAt: null, finishedAt: null, expiresAt: at(230), result: null,
      }),
    },
    want: { outcome: 'pending' },
  },
  {
    name: 'record-finished-before-3-1',
    purpose: 'A finished update from an agent before 3.1.0, with no outcome anywhere. It is reduced from its result: updated is a success.',
    tags: ['finding:c5', 'agent-3.0'],
    input: {
      kind: 'record',
      value: record({
        id: '2c3d4e5f-6a7b-4c8d-ae9f-1a2b3c4d5e6f', agentVersion: '3.0.1', from: '1.4.1', to: '1.4.2',
        result: { ok: true, action: 'updated', reasonCode: null, message: 'updated demo from 1.4.1 to 1.4.2', from: '1.4.1', to: '1.4.2', exitCode: 0, output: null },
      }),
    },
    want: { outcome: 'succeeded' },
  },
  {
    name: 'record-finished-cancelled',
    purpose: 'A queued request cancelled before it ran. The request was deferred by a person, which is neither a success nor a failure of the work.',
    tags: ['finding:c5'],
    input: {
      kind: 'record',
      value: record({
        id: '3d4e5f6a-7b8c-4d9e-bf0a-2b3c4d5e6f7a', kind: 'boot', service: null, target: 'windows', mode: 'queued', startedAt: null,
        expiresAt: at(40), result: { ok: false, action: 'cancelled', reasonCode: 'cancelled', message: 'the queued boot into windows was cancelled before it ran', exitCode: null, output: null },
      }),
    },
    want: { outcome: 'deferred' },
  },
  {
    name: 'record-finished-interrupted',
    purpose: 'The worker died and recovery closed the record. Whether the work took effect is not known, so nothing claims it either way.',
    tags: ['finding:c5'],
    input: {
      kind: 'record',
      value: record({
        id: '4e5f6a7b-8c9d-4eaf-8a1b-3c4d5e6f7a8b', from: '1.4.1', to: '1.4.1', agentVersion: '3.0.1',
        result: { ok: false, action: 'interrupted', reasonCode: 'interrupted', message: 'the worker holding this operation is gone', from: '1.4.1', to: '1.4.1', exitCode: null, output: null },
      }),
    },
    want: { outcome: 'unknown' },
  },
  {
    name: 'record-outcome-beats-result-outcome',
    purpose: 'The record says unknown and its result says failed. The record is read first: a producer that knows the failure may have changed the machine lowers it to unknown on the record.',
    tags: ['finding:c5'],
    input: {
      kind: 'record',
      value: record({
        id: '5f6a7b8c-9d0e-4fa1-9b2c-4d5e6f7a8b9c', outcome: 'unknown',
        result: { ok: false, action: 'failed', reasonCode: 'apply-failed', message: 'the new build would not apply', exitCode: 1, output: null, outcome: 'failed' },
      }),
    },
    want: { outcome: 'unknown' },
  },
  {
    name: 'record-result-outcome-only',
    purpose: 'Only the result states an outcome. A result\'s stated outcome is used when the record states none.',
    tags: ['finding:c5'],
    input: {
      kind: 'record',
      value: record({
        id: '6a7b8c9d-0e1f-4ab2-8c3d-5e6f7a8b9c0d', from: '1.4.2', to: '1.4.2',
        result: { ok: false, action: 'failed', reasonCode: 'apply-failed', message: 'the new build would not apply and 1.4.2 is still installed', from: '1.4.2', to: '1.4.2', exitCode: 1, output: null, outcome: 'failed' },
      }),
    },
    want: { outcome: 'failed' },
  },
  {
    name: 'summary-rolled-back',
    purpose: 'A summary carries no ok. A rolled-back summary with its reason code is a failure: the old version runs, the update did not happen.',
    tags: ['finding:c5'],
    input: { kind: 'summary', value: summary({ id: '7b8c9d0e-1f2a-4bc3-9d4e-6f7a8b9c0d1e', action: 'rolled-back', reasonCode: 'rolled-back' }) },
    want: { outcome: 'failed' },
  },
  {
    name: 'summary-running',
    purpose: 'A running summary, as status.operations.running lists it. Pending, whatever action it is heading for.',
    tags: ['finding:c5'],
    input: { kind: 'summary', value: summary({ id: '8c9d0e1f-2a3b-4cd4-ae5f-7a8b9c0d1e2f', service: 't3', mode: 'force', state: 'running', phase: 'installing', updatedAt: at(0, 47) }) },
    want: { outcome: 'pending' },
  },
  {
    name: 'record-unknown-outcome-value',
    purpose: 'An outcome this version has never heard of, from a later agent. It is not a reason to believe anything, so it reduces to unknown.',
    tags: ['finding:c5'],
    input: {
      kind: 'record',
      value: record({
        id: '9d0e1f2a-3b4c-4de5-bf6a-8b9c0d1e2f3a', outcome: 'partial',
        result: { ok: true, action: 'updated', reasonCode: null, message: 'updated demo', exitCode: 0, output: null },
      }),
    },
    want: { outcome: 'unknown' },
  },
  {
    name: 'record-finished-stated-succeeded',
    purpose: 'A finished 3.1.0 record that states succeeded on the record and on its result, the way the agent writes them.',
    tags: ['finding:c5'],
    input: {
      kind: 'record',
      value: record({
        id: 'ae1f2a3b-4c5d-4ef6-8a7b-9c0d1e2f3a4b', kind: 'restart', outcome: 'succeeded',
        result: { ok: true, action: 'restarted', reasonCode: null, message: 'demo restarted and answered', exitCode: 0, output: null, outcome: 'succeeded' },
      }),
    },
    want: { outcome: 'succeeded' },
  },
  {
    name: 'summary-stated-deferred',
    purpose: 'A finished summary that states deferred, as a busy update the agent declined reads in history.',
    tags: ['finding:c5'],
    input: { kind: 'summary', value: summary({ id: 'bf2a3b4c-5d6e-4f07-9b8c-0d1e2f3a4b5c', action: 'deferred', reasonCode: 'busy', outcome: 'deferred' }) },
    want: { outcome: 'deferred' },
  },
];

export default {
  family: 'outcome',
  section: '5.6',
  reference: 'tools/outcome.mjs',
  notes: [
    'reduce: the input holds only the fields the reducer reads; a key that is absent is absent from the reply. Feed the object to the reducer exactly as it is.',
    'records: kind "record" is reduced from { ...value.result, state: value.state, outcome: value.outcome ?? value.result?.outcome }, where result is null until the record is finished; kind "summary" is reduced from the summary itself. Decode value with the decoder used for op records and summaries in production.',
    'A stated outcome this version does not know reduces to unknown; it is never passed through.',
  ],
  consumers: {
    reduce: EVERYONE,
    records: CLIENTS,
  },
  groups: {
    reduce: [
      reduce('explicit-succeeded-beats-failed', { outcome: 'succeeded', ok: false, action: 'failed', reasonCode: 'apply-failed' }, 'succeeded',
        'A stated outcome is read before anything else, even when the other fields would reduce to something different.'),
      reduce('explicit-unknown-value', { outcome: 'partial', ok: true, action: 'updated', reasonCode: null }, 'unknown',
        'An outcome this version does not know is not passed through and not ignored in favour of the action: it is unknown.'),
      reduce('state-queued', { state: 'queued' }, 'pending',
        'Queued work is pending: the agent still owns it, and the client polls.'),
      reduce('state-running-with-updated', { state: 'running', ok: true, action: 'updated', reasonCode: null }, 'pending',
        'A running state beats an action, so a record that already names where it is heading is still pending.'),
      reduce('accepted', { ok: true, action: 'accepted', reasonCode: null }, 'pending',
        'accepted is the answer to --detach: the work is running and the client polls op ID --wait 20.'),
      reduce('accepted-timed-out', { ok: true, action: 'accepted', reasonCode: 'timed-out' }, 'pending',
        'A 3.0.x shape: accepted with a code beside it. The action decides, and the work is still the agent\'s.', ['agent-3.0']),
      reduce('already-running', { ok: true, action: 'already-running', reasonCode: 'already-running' }, 'pending',
        'A replay of an id whose work is still running, as 3.1.0 answers it. The client follows it exactly as it follows accepted.', ['finding:c2']),
      reduce('action-queued', { ok: true, action: 'queued', reasonCode: null }, 'pending',
        'The answer to --when-idle: a record waiting for an idle cycle is pending.'),
      reduce('action-rebooting', { ok: true, action: 'rebooting', reasonCode: null }, 'pending',
        'A reboot was scheduled; only a later reading of the record says whether it happened.'),
      reduce('action-sleeping', { ok: true, action: 'sleeping', reasonCode: null }, 'pending',
        'A suspend was accepted; only recovery ever writes slept, so until then it is pending.'),
      ...SUCCESS_ACTIONS.map((action) => reduce(`success-${action}`, { ok: true, action, reasonCode: null }, 'succeeded',
        `${action} is one of the success actions: the requested state holds.`)),
      reduce('noop-ok', { ok: true, action: 'noop', reasonCode: 'no-update' }, 'succeeded',
        'Nothing to install: the requested state already holds, which is a success with a code that says why nothing ran.'),
      reduce('noop-not-ok-already-on-target', { ok: false, action: 'noop', reasonCode: 'already-on-target' }, 'succeeded',
        'A noop with ok false still means the machine is where it was asked to be; the action decides, not ok.'),
      reduce('deferred-busy', { ok: true, action: 'deferred', reasonCode: 'busy' }, 'deferred',
        'The busy gate held. Finished without acting, by design; a later attempt uses a new id.'),
      reduce('deferred-not-ok-policy-off', { ok: false, action: 'deferred', reasonCode: 'policy-off' }, 'deferred',
        'A deferral with ok false is still a deferral: the action decides.'),
      reduce('conflict-ok', { ok: true, action: 'conflict', reasonCode: 'operation-in-progress' }, 'deferred',
        'Another operation holds the lock, as 3.1.0 answers it with ok true. Nothing was started; try later with a new id.', ['finding:c2']),
      reduce('conflict-not-ok-lock-held', { ok: false, action: 'conflict', reasonCode: 'lock-held' }, 'deferred',
        'The same conflict as 3.0.x sent it, with ok false. The action decides, so the answer does not change.', ['agent-3.0', 'finding:c2']),
      reduce('conflict-already-running', { ok: true, action: 'conflict', reasonCode: 'already-running' }, 'deferred',
        'A cancel of a running operation, which cannot be cancelled: nothing changed, and that is a deferral rather than a failure.', ['agent-3.0', 'finding:c2']),
      reduce('expired', { ok: false, action: 'expired', reasonCode: 'expired' }, 'deferred',
        'A queued request whose expiry passed before an idle cycle came. It never ran, by design.'),
      reduce('cancelled-request', { ok: false, action: 'cancelled', reasonCode: 'cancelled' }, 'deferred',
        'The cancelled request itself: deferred by a person, never run.'),
      reduce('cancelled-cancel', { ok: true, action: 'cancelled', reasonCode: null }, 'succeeded',
        'The reply to cancel: the cancel did what was asked, so it succeeded.'),
      reduce('rolled-back-not-ok', { ok: false, action: 'rolled-back', reasonCode: 'rolled-back' }, 'failed',
        'The new version would not come up and the old one is back: the update failed and the state is known.'),
      reduce('rolled-back-ok', { ok: true, action: 'rolled-back', reasonCode: null }, 'succeeded',
        'An agent rollback that was itself the request: the previous signed tree is restored as asked.'),
      reduce('summary-rolled-back-without-ok', { state: 'finished', action: 'rolled-back', reasonCode: 'rolled-back' }, 'failed',
        'A summary carries no ok. The reason code alone decides a rollback, so the summary agrees with its record.'),
      reduce('interrupted', { ok: false, action: 'interrupted', reasonCode: 'interrupted' }, 'unknown',
        'The worker died mid-way. Nobody knows whether the work took effect, so it is never retried automatically.'),
      reduce('failed-apply-failed', { ok: false, action: 'failed', reasonCode: 'apply-failed' }, 'failed',
        'A failure whose state is known: the staged build would not apply and the old one is still installed.'),
      reduce('failed-internal', { ok: false, action: 'failed', reasonCode: 'internal' }, 'failed',
        'A failed action with internal: the action says the work finished and failed, so the result is known.'),
      reduce('failed-signature-invalid', { ok: false, action: 'failed', reasonCode: 'signature-invalid' }, 'failed',
        'A refused signature: nothing was extracted, and the state is known.'),
      reduce('failed-timed-out', { ok: false, action: 'failed', reasonCode: 'timed-out' }, 'unknown',
        'A command killed at its timeout may have done anything before it died, so failed with timed-out is unknown.'),
      reduce('failed-interrupted', { ok: false, action: 'failed', reasonCode: 'interrupted' }, 'unknown',
        'failed with interrupted is unknown for the same reason as the interrupted action.'),
      reduce('no-action-ok', { ok: true, reasonCode: null }, 'succeeded',
        'A mutating reply with no action and no code, such as policy set or service-config set: ok means it did what was asked.'),
      reduce('no-action-ok-with-code', { ok: true, reasonCode: 'busy' }, 'unknown',
        'ok true with a reason code and no action says two things at once, so it reduces to unknown rather than to a guess.'),
      reduce('no-action-bad-argument', { ok: false, reasonCode: 'bad-argument' }, 'failed',
        'A refusal before anything ran: nothing is in effect and the state is known.'),
      reduce('no-action-stale-revision', { ok: false, reasonCode: 'stale-revision' }, 'failed',
        'A refused setup push from a device that is behind: nothing was written.'),
      reduce('no-action-restricted', { ok: false, reasonCode: 'restricted' }, 'failed',
        'A restricted session refused: nothing ran.'),
      reduce('no-action-config-invalid', { ok: false, reasonCode: 'config-invalid' }, 'failed',
        'A document or configuration refused as invalid: nothing was written.'),
      reduce('no-action-operation-in-progress', { ok: false, reasonCode: 'operation-in-progress' }, 'deferred',
        'A config set refused because an operation holds the lock: nothing is wrong with the request, so it is a deferral.', ['finding:c2']),
      reduce('no-action-lock-held', { ok: false, reasonCode: 'lock-held' }, 'deferred',
        'The same deferral under the lock-held code.', ['finding:c2']),
      reduce('no-action-internal', { ok: false, reasonCode: 'internal' }, 'unknown',
        'An internal error with no action: the agent failed somewhere it did not expect, so nobody can say what changed.'),
      reduce('no-action-timed-out', { ok: false, reasonCode: 'timed-out' }, 'unknown',
        'Timed out with no action: unknown, never retried automatically.'),
      reduce('no-action-no-ok', { reasonCode: null }, 'unknown',
        'Neither an action nor ok: there is nothing to reduce from.'),
      reduce('unknown-action-ok', { ok: true, action: 'teleported', reasonCode: null }, 'unknown',
        'An action this version does not know, with ok true. ok alone is not evidence of what happened.'),
      reduce('unknown-action-not-ok', { ok: false, action: 'teleported', reasonCode: 'internal' }, 'unknown',
        'An action this version does not know, with ok false: still unknown, never failed.'),
      reduce('empty', {}, 'unknown',
        'An empty object, which is what a decoder hands over when nothing it knows was present.'),
      reduce('legacy-2x-deferred', { ok: true, action: 'deferred' }, 'deferred',
        'A 2.x reply: an action and ok, and no reason code at all. The action still decides.', ['agent-3.0']),
      reduce('null-action-ok', { ok: true, action: null, reasonCode: null }, 'succeeded',
        'An explicit null action is the same as no action: ok with no code succeeded.'),
      reduce('null-action-ok-with-code', { ok: true, action: null, reasonCode: 'busy' }, 'unknown',
        'An explicit null action with ok true and a code is the same as no action: unknown.'),
      reduce('null-action-no-ok', { action: null, reasonCode: null }, 'unknown',
        'An explicit null action and no ok: nothing to reduce from.'),
    ],
    records,
  },
};
