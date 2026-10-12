// Setup reconciliation, v3.md section 9.5, as cases.
//
// The decision cases walk the eleven rules in order, each with the inputs that
// make exactly that rule the first to match, plus the ones that test an order:
// another setup whose hash this device has seen is still another setup, a meta
// answer outranks what the status said, and two branches 33 revisions apart
// stop at a person. The admission cases are the ways a fetched document can
// fail to be the one the decision expected (C1), with the edit saved during the
// read first of all (M16, A15). The gate cases are the guardrails Android did
// not have (C7). The push replies are the config set fixtures themselves, so a
// client's afterPush reads exactly the replies its decoder is tested with.
//
// Hashes are sha256 of a label, so each one is a real 64-digit hash and the
// case still says which document it stands for.

import crypto from 'node:crypto';
import { H_GRANDPARENT, H_PARENT, OTHER_SETUP_ID, SETUP_ID } from '../documents.mjs';
import { buildFixtures } from '../make-fixtures.mjs';

const CLIENTS = ['mac', 'desktop', 'android'];
const C7 = ['finding:c7'];

/** A stand-in hash for the document a label names. */
const h = (label) => crypto.createHash('sha256').update(`setup-reconcile ${label}`).digest('hex');

// One history: this device applies revision 12 (Hd), whose ancestors are 11
// and 10. The machine may hold one of them, a revision 13 that descends from
// this device's 12, or a revision 12 of its own on another branch.
const R10 = h('revision 10');
const R11 = h('revision 11');
const HD = h('revision 12, this device');
const THEIRS_12 = h('revision 12, another branch');
const THEIRS_13 = h('revision 13, after this device');
const UNCLAIMED = h('a copy pushed by a 2.x client');

const LOCAL = Object.freeze({ hash: HD, id: SETUP_ID, lineage: [R11, R10] });
const status = (hash, id = SETUP_ID) => ({ contract: 3, controller: { hash, id } });
const meta = (hash, lineage, id = SETUP_ID) => ({ hash, id, lineage });

const decision = (name, purpose, input, want, tags = []) => ({ name, purpose, tags, input, want });
const r = (decided, reason = null, expect = null) => ({ decision: decided, reason, expect });

// Two branches more than 32 revisions apart: c0 is this device's head, c33
// the machine's. c33's lineage holds c32 to c1, which is all 32 it may carry,
// so c0 is in neither lineage.
const chain = Array.from({ length: 34 }, (_, index) => h(`chain ${index}`));

// The refusals R4 and R5 file, each decided on at once: D16 for the
// stale-revision one, D17 for the divergent one. The divergent push came from
// a revision 12 of this device's own, a sibling of the machine's: both have
// H_PARENT as their parent, so neither lineage holds the other.
const fixture = (name) => JSON.parse(buildFixtures().get(`fixtures/${name}`));
const STALE = fixture('config.set.stale-revision.json');
const DIVERGENT = fixture('config.set.conflict-divergent.json');
const SIBLING_12 = h('revision 12, a sibling of the machine\'s');

const decisions = [
  decision('unsupported-2x-agent', 'A 2.x agent sends no contract. It cannot carry the setup and is left out, never shown as a conflict.',
    { local: LOCAL, status: { contract: null, controller: { hash: R11, id: null } }, metaByHash: {} }, r('unsupported')),
  decision('unsupported-no-controller', 'A contract 3 status without a controller block says nothing about the setup, so the machine is left out of this round.',
    { local: LOCAL, status: { contract: 3, controller: null }, metaByHash: {} }, r('unsupported')),
  decision('unknown-no-local', 'This device applies no setup yet. Nothing is pushed or fetched automatically; enrolment is an explicit flow.',
    { local: null, status: status(THEIRS_13), metaByHash: {} }, r('unknown', 'no-local')),
  decision('unknown-local-unidentified', 'A document without a setup id gets an identity before it takes part, so until then rule 3 keeps it out.',
    { local: { hash: HD, id: null, lineage: [] }, status: status(THEIRS_13), metaByHash: {} }, r('unknown', 'local-unidentified')),
  decision('push-machine-empty', 'The machine holds nothing: push.',
    { local: LOCAL, status: { contract: 3, controller: { hash: null, id: null } }, metaByHash: {} }, r('push', 'machine-empty')),
  decision('in-sync', 'The machine holds exactly the applied document.',
    { local: LOCAL, status: status(HD), metaByHash: {} }, r('inSync')),
  decision('push-machine-unidentified', 'The machine holds a copy nobody claims, pushed without an identity: push over it.',
    { local: LOCAL, status: status(UNCLAIMED, null), metaByHash: {} }, r('push', 'machine-unidentified')),
  decision('different-setup-even-if-in-lineage', 'Another setup id wins over a hash this device has seen before: rule 7 comes before rule 8, so a person decides instead of a push replacing somebody else\'s fleet.',
    { local: LOCAL, status: status(R11, OTHER_SETUP_ID), metaByHash: {} }, r('differentSetup')),
  decision('push-machine-behind', 'The machine holds an ancestor of the applied document: push.',
    { local: LOCAL, status: status(R11), metaByHash: {} }, r('push', 'machine-behind')),
  decision('read-meta', 'The machine holds a revision of this setup that is not an ancestor, and nothing is filed for it: read config meta once, then decide again.',
    { local: LOCAL, status: status(THEIRS_13), metaByHash: {} }, r('readMeta')),
  decision('fetch', 'The filed meta says the machine\'s copy descends from the applied one: fetch, expecting exactly that hash.',
    { local: LOCAL, status: status(THEIRS_13), metaByHash: { [THEIRS_13]: meta(THEIRS_13, [HD, R11, R10]) } }, r('fetch', null, THEIRS_13)),
  decision('diverged', 'Neither copy descends from the other: stop at a person.',
    { local: LOCAL, status: status(THEIRS_12), metaByHash: { [THEIRS_12]: meta(THEIRS_12, [R11, R10]) } }, r('diverged')),
  decision('meta-for-another-hash', 'Meta filed for a hash the machine held earlier says nothing about what it holds now: read meta again.',
    { local: LOCAL, status: status(THEIRS_13), metaByHash: { [THEIRS_12]: meta(THEIRS_12, [R11, R10]) } }, r('readMeta')),
  decision('diverged-33-apart', 'The machine is 33 revisions ahead, one more than a lineage carries, so the two look unrelated: that fails toward a question, never toward a loss.',
    {
      local: { hash: chain[0], id: SETUP_ID, lineage: [] },
      status: status(chain[33]),
      metaByHash: { [chain[33]]: meta(chain[33], chain.slice(1, 33).reverse()) },
    }, r('diverged')),
  decision('meta-id-outranks-status', 'The status names this setup but the filed meta, which describes the same bytes, names another: the meta decides.',
    { local: LOCAL, status: status(THEIRS_13), metaByHash: { [THEIRS_13]: meta(THEIRS_13, [HD, R11, R10], OTHER_SETUP_ID) } }, r('differentSetup')),
  decision('filed-current-fetches', 'The current of the stale-revision refusal in push-stale-revision, filed and decided on at once with the status replaced by it: the machine is ahead, so fetch with no extra meta read.',
    {
      local: { hash: H_PARENT, id: SETUP_ID, lineage: [H_GRANDPARENT] },
      status: { contract: 3, controller: { hash: STALE.current.hash, id: STALE.current.id } },
      metaByHash: { [STALE.current.hash]: STALE.current },
    }, r('fetch', null, STALE.current.hash)),
  decision('filed-current-diverges', 'The current of the divergent refusal in push-divergent, filed and decided on at once: a sibling of the revision this device pushed, so neither descends from the other and a person decides.',
    {
      local: { hash: SIBLING_12, id: SETUP_ID, lineage: [H_PARENT, H_GRANDPARENT] },
      status: { contract: 3, controller: { hash: DIVERGENT.current.hash, id: DIVERGENT.current.id } },
      metaByHash: { [DIVERGENT.current.hash]: DIVERGENT.current },
    }, r('diverged')),
];

const EXPECTED = Object.freeze({ localHash: HD, localId: SETUP_ID, remoteHash: THEIRS_13 });
const FETCHED = Object.freeze({ hash: THEIRS_13, replyHash: THEIRS_13, id: SETUP_ID, lineage: [HD, R11, R10] });
const admission = (name, purpose, change, admitted, tags = []) => {
  const input = { expected: { ...EXPECTED }, currentLocalHash: HD, fetched: { ...FETCHED, lineage: [...FETCHED.lineage] }, accepted: true };
  change(input);
  return { name, purpose, tags, input, want: { admission: admitted } };
};

const admissions = [
  admission('adopt', 'The bytes hash to what the decision expected, pass section 9.7, carry this setup id and descend from the applied document.',
    () => {}, 'adopt'),
  admission('moved', 'The bytes hash to the reply\'s own hash but not the expected one: the machine changed between meta and fetch, so decide again on the next status.',
    (input) => { Object.assign(input.fetched, { hash: h('revision 14'), replyHash: h('revision 14') }); }, 'moved'),
  admission('unverifiable', 'The bytes match neither the expected hash nor the reply\'s: a rebuild from an agent that sends no raw text. Never adopted.',
    (input) => { input.fetched.hash = h('revision 13, re-serialised'); }, 'unverifiable', ['finding:c1']),
  admission('invalid', 'The right bytes, but section 9.7 refuses them under peer intake.',
    (input) => { input.accepted = false; }, 'invalid'),
  admission('inconsistent-lineage', 'The right bytes, whose own lineage does not contain the applied hash, although the meta said it did.',
    (input) => { input.fetched.lineage = [R11, R10]; }, 'inconsistent'),
  admission('inconsistent-id', 'The right bytes, whose own controller block names another setup.',
    (input) => { input.fetched.id = OTHER_SETUP_ID; }, 'inconsistent'),
  admission('head-moved-first', 'An edit was saved here while the read was suspended. Nothing is written, whatever the bytes are, and the next status decides from the new head.',
    (input) => {
      input.currentLocalHash = h('revision 13, saved here during the read');
      Object.assign(input.fetched, { hash: h('revision 13, re-serialised'), id: OTHER_SETUP_ID });
      input.accepted = false;
    }, 'headMoved', ['finding:m16', 'finding:a15']),
];

const NOW = 1_000_000;
const AGENT = '3.0.1';
const state = (changes = {}) => ({
  inFlight: false,
  failedAt: {},
  held: {},
  localInvalid: false,
  localMutation: false,
  runningOperations: 0,
  lockedAt: null,
  statusAt: NOW - 1_000,
  settled: [],
  ...changes,
});
const gated = (name, purpose, decided, changes, action, reason, { tags = [], agentVersion = AGENT, remoteHash = THEIRS_13 } = {}) => ({
  name, purpose, tags,
  input: { decision: decided, localHash: HD, remoteHash, agentVersion, now: NOW, state: state(changes) },
  want: { action, reason },
});
const SETTLED = `${HD} ${THEIRS_13} ${AGENT}`;

const gates = [
  gated('push-acts', 'A push with nothing in the way goes out.', 'push', {}, 'act', null),
  gated('push-held-running-operation', 'The machine\'s last status lists a running operation: the push waits.', 'push', { runningOperations: 1 }, 'hold', 'operation-running', { tags: C7 }),
  gated('push-held-local-mutation', 'This device has an action in flight on the machine: the push waits.', 'push', { localMutation: true }, 'hold', 'operation-running', { tags: C7 }),
  gated('push-held-until-later-status', 'A lock refusal at the same millisecond as the last status: no status read after the refusal has said nothing is running yet.', 'push', { lockedAt: NOW - 1_000, statusAt: NOW - 1_000 }, 'hold', 'operation-running', { tags: C7 }),
  gated('push-after-later-status', 'A status read after the lock refusal lists nothing running: the push goes out again.', 'push', { lockedAt: NOW - 2_000, statusAt: NOW - 1_000 }, 'act', null, { tags: C7 }),
  gated('push-backoff-59s', 'A failure 59 s ago for this applied hash holds everything automatic.', 'push', { failedAt: { [HD]: NOW - 59_000 } }, 'hold', 'backoff', { tags: C7 }),
  gated('push-backoff-over', 'A failure exactly 60 s ago no longer holds anything.', 'push', { failedAt: { [HD]: NOW - 60_000 } }, 'act', null, { tags: C7 }),
  gated('backoff-other-hash', 'A failure for another applied hash says nothing about this one: a new hash starts afresh.', 'push', { failedAt: { [R11]: NOW - 1_000 } }, 'act', null, { tags: C7 }),
  gated('read-meta-during-operation', 'Reads are never held for a running operation.', 'readMeta', { runningOperations: 2, localMutation: true }, 'act', null),
  gated('fetch-backoff', 'The back-off holds reads too.', 'fetch', { failedAt: { [HD]: NOW - 10_000 } }, 'hold', 'backoff'),
  gated('in-flight', 'One setup request per machine at a time.', 'push', { inFlight: true }, 'hold', 'in-flight'),
  gated('read-meta-in-flight', 'A read waits for the request in flight like anything else.', 'readMeta', { inFlight: true }, 'hold', 'in-flight'),
  gated('fetch-settled', 'This fetch was refused before for the same two hashes and the same agent: it is not read again.', 'fetch', { settled: [SETTLED] }, 'hold', 'settled'),
  gated('fetch-settled-agent-updated', 'The agent was updated since, perhaps to one that sends raw: read again.', 'fetch', { settled: [SETTLED] }, 'act', null, { agentVersion: '3.1.0' }),
  gated('in-sync-sends-nothing', 'A decision that calls for no request sends none, whatever the state.', 'inSync', { inFlight: true }, 'none', null),
  gated('diverged-sends-nothing', 'Divergence stops at a person; nothing automatic goes out.', 'diverged', {}, 'none', null),
  gated('push-held-after-refusal', 'The machine refused this applied hash with restricted: it is not sent again until the hash changes or a person asks.', 'push', { held: { [HD]: 'restricted' } }, 'hold', 'held', { tags: C7 }),
  gated('held-other-hash', 'A refusal of another applied hash does not hold this one.', 'push', { held: { [R11]: 'config-invalid' } }, 'act', null, { tags: C7 }),
  gated('push-held-local-invalid', 'The applied document has a section 9.7 error: it is not published.', 'push', { localInvalid: true }, 'hold', 'local-invalid', { tags: C7 }),
  gated('fetch-despite-local-invalid', 'A device whose own document has an error still reads and fetches, which is how it is repaired.', 'fetch', { localInvalid: true }, 'act', null, { tags: C7 }),
];

const replied = (name, purpose, reply, localHash, effect, metaHash = null) => ({
  name, purpose, tags: [], input: { reply, localHash, now: NOW }, want: { effect, metaHash },
});
const STORED = fixture('config.set.stored.json');
const LOCKED = fixture('config.set.locked.json');

/** The bad-argument refusal as an internal one, which says nothing about what the machine holds. */
const INTERNAL = { ...fixture('config.set.bad-argument.json'), reasonCode: 'internal', message: 'the setup could not be written; nothing was changed' };
delete INTERNAL.current;
delete INTERNAL.divergent;

/** The stale-revision refusal with a current that has no hash, which no agent sends and nothing can be filed under. */
const STALE_WITHOUT_HASH = { ...STALE, current: { ...STALE.current, hash: null } };

const replies = [
  replied('push-stored', 'config.set.stored.json for the hash this device pushed: the failure, if any, is forgotten.', STORED, STORED.hash, 'clear'),
  replied('push-noop', 'config.set.noop.json: the machine already held exactly these bytes.', fixture('config.set.noop.json'), STORED.hash, 'clear'),
  replied('push-replaced', 'config.set.replaced.json for the hash this device pushed.', fixture('config.set.replaced.json'), fixture('config.set.replaced.json').hash, 'clear'),
  replied('push-stored-other-hash', 'A push that stored something other than the applied document: the machine holds bytes this device did not send, so this is a failure.', STORED, H_PARENT, 'fail'),
  replied('push-stale-revision', 'config.set.stale-revision.json: the machine is ahead. Its current is filed under its hash and decided on at once (filed-current-fetches).', STALE, H_PARENT, 'fileMeta', STALE.current.hash),
  replied('push-divergent', 'config.set.conflict-divergent.json for a revision 12 of this device\'s own: the current, a sibling of it, is filed under its hash and decided on at once (filed-current-diverges).', DIVERGENT, SIBLING_12, 'fileMeta', DIVERGENT.current.hash),
  replied('push-operation-in-progress', 'config.set.locked.json: an update holds the lock; push again once a later status lists nothing running.', LOCKED, H_PARENT, 'locked'),
  replied('push-lock-held', 'The lock refusal under its other code.', { ...LOCKED, reasonCode: 'lock-held' }, H_PARENT, 'locked'),
  replied('push-no-reply', 'No reply was read: the link dropped or the command timed out. The push may have landed or not; back off and decide from the next status.', null, H_PARENT, 'fail'),
  replied('push-bad-argument', 'config.set.bad-argument.json carries current too, but only stale-revision and controller-conflict are filed: this is a failure.', fixture('config.set.bad-argument.json'), H_PARENT, 'fail'),
  replied('push-restricted', 'config.set.restricted-launch.json: this key may not publish these launch values, and trying again cannot change that.', fixture('config.set.restricted-launch.json'), H_PARENT, 'hold'),
  replied('push-config-invalid', 'config.set.invalid-document.json: the agent refused the document\'s structure.', fixture('config.set.invalid-document.json'), H_PARENT, 'hold'),
  replied('push-internal', 'An internal refusal is a failure to back off from.', INTERNAL, H_PARENT, 'fail'),
  replied('push-stale-revision-without-hash', 'The stale-revision refusal with a null current.hash. Meta is filed under the hash it describes, and this current has none, so it says nothing about what the machine holds: a failure, never meta filed under null.', STALE_WITHOUT_HASH, H_PARENT, 'fail'),
];

export default {
  family: 'setup-reconcile',
  section: '9.5',
  reference: 'tools/setup-sync.mjs',
  notes: [
    'decide: local is {hash, id, lineage} or null; status is the status reply with only contract and controller {hash, id}; metaByHash maps a hash to the config meta answer or refusal current filed for it. expect is the hash a fetch must produce, null for every other decision.',
    'admit: expected is what the decision was made from; currentLocalHash is the applied hash read again from storage after the read; fetched is the canonical hash of the bytes, the hash the reply carried, and the id and lineage of the bytes\' own controller block; accepted is section 9.7 under peer intake. Checked in the order headMoved, moved, unverifiable, invalid, inconsistent, adopt.',
    'admit: a config reply without a hash never reaches admit. Section 9.5 never adopts it; the client counts it as a failed request (the 60 s back-off) before admitting anything, so replyHash is always a hash and admit does not test for its absence.',
    'gate: times are integer milliseconds on one clock. failedAt and held are keyed by the applied hash; settled holds "<applied hash> <remote hash> <agentVersion>" for each remembered admission refusal. The back-off is 60000 ms: a failure exactly 60000 ms ago no longer holds.',
    'pushReply: reply is the parsed config set reply, or null when none was read. metaHash is set only for fileMeta.',
    'pushReply: only a reply with ok: false is a refusal, whatever its reasonCode. fileMeta needs current.hash to be a hash; otherwise the reply is a failure.',
  ],
  consumers: { decide: CLIENTS, admit: CLIENTS, gate: CLIENTS, pushReply: CLIENTS },
  groups: { decide: decisions, admit: admissions, gate: gates, pushReply: replies },
};
