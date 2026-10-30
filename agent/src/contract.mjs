// The wire contract, in one place.
//
// Everything a client decodes is defined here: the contract number that gates
// v3 features, the closed reasonCode enum, the shape of the reply envelope, and
// the grammar every token that arrives over ssh has to match. Nothing else in
// the agent invents a reason string or a state name; if it is not in this file
// it is not part of the contract.

export const CONTRACT_VERSION = 3;

/**
 * The closed reasonCode enum.
 *
 * A client renders by code and falls back to `message`. Codes may be added
 * later; a client that meets one it does not know treats it as "failed, show the
 * message". They are grouped only for reading — the enum is flat.
 */
export const REASON = {
  // the busy gate
  busy: 'busy',
  busyUnknown: 'busy-unknown',
  // policy
  policyOff: 'policy-off',
  policyPaused: 'policy-paused',
  outsideWindow: 'outside-window',
  // exclusion
  operationInProgress: 'operation-in-progress',
  lockHeld: 'lock-held',
  // updates
  noUpdate: 'no-update',
  latestUnknown: 'latest-unknown',
  notInstalled: 'not-installed',
  notRunning: 'not-running',
  appClosed: 'app-closed',
  applyAttemptsExhausted: 'apply-attempts-exhausted',
  applyFailed: 'apply-failed',
  postconditionFailed: 'postcondition-failed',
  rolledBack: 'rolled-back',
  // configuration and platform
  notConfigured: 'not-configured',
  unsupportedPlatform: 'unsupported-platform',
  unknownService: 'unknown-service',
  unknownTarget: 'unknown-target',
  unknownAction: 'unknown-action',
  badArgument: 'bad-argument',
  configInvalid: 'config-invalid',
  configMissing: 'config-missing',
  // operation lifecycle
  interrupted: 'interrupted',
  expired: 'expired',
  cancelled: 'cancelled',
  alreadyRunning: 'already-running',
  alreadyOnTarget: 'already-on-target',
  // controller documents
  staleRevision: 'stale-revision',
  controllerConflict: 'controller-conflict',
  // trust and access
  signatureInvalid: 'signature-invalid',
  restricted: 'restricted',
  // generic
  timedOut: 'timed-out',
  internal: 'internal',
};

export const REASON_CODES = new Set(Object.values(REASON));

/** Operation kinds. `run` is a configured action; `self-update` replaces the agent. */
export const OPERATION_KINDS = ['update', 'restart', 'boot', 'sleep', 'run', 'cycle', 'self-update'];

/** The three operation states. Everything terminal is `finished` with a result. */
export const OPERATION_STATES = ['queued', 'running', 'finished'];

/** How the operation was asked for. */
export const OPERATION_MODES = ['scheduled', 'manual', 'force', 'queued'];

/** The phases each kind moves through, in order. Recovery reads these by name. */
export const PHASES = {
  update: [
    'resolving',
    'recovering',
    'checking-busy',
    'warming',
    'locking',
    'stopping',
    'installing',
    'verifying',
    'starting',
    'health',
    'rolling-back',
    'done',
  ],
  restart: ['checking-busy', 'locking', 'restarting', 'health', 'done'],
  boot: ['checking-busy', 'locking', 'arming', 'verifying', 'rebooting', 'done'],
  sleep: ['checking-busy', 'locking', 'preparing', 'suspending', 'done'],
  run: ['checking-busy', 'locking', 'running', 'done'],
  cycle: ['recovering', 'queued', 'services', 'done'],
  'self-update': ['verifying', 'staging', 'self-test', 'swapping', 'done'],
};

/**
 * The phases during which an interrupted operation can have left a service down.
 * Recovery only acts on these: a run that died while resolving a version never
 * touched anything, and starting a service somebody stopped on purpose would be
 * damage of its own. A restart is one of them: for a command, a scheduled task
 * or an app it is a stop and then a start, and a run that dies between the two
 * leaves the service stopped like any update.
 */
export const DISRUPTED_PHASES = new Set(['stopping', 'installing', 'verifying', 'starting', 'rolling-back', 'restarting']);

/**
 * The phases in which a run may already have changed the machine, whether or
 * not it can leave a service down. A run that fails for a reason of its own
 * making in one of these reports its outcome as unknown rather than failed: it
 * may have restarted, armed, suspended, run or swapped something nobody then
 * observed, and "failed" would invite a retry of work that may have happened.
 */
export const MUTATING_PHASES = new Set([
  ...DISRUPTED_PHASES,
  'arming',
  'rebooting',
  'preparing',
  'suspending',
  'running',
  'swapping',
]);

/** The action values each kind may report. Supersets of the 2.x values. */
export const ACTIONS = {
  update: [
    'updated',
    'noop',
    'deferred',
    'queued',
    'failed',
    'rolled-back',
    'accepted',
    'conflict',
    'interrupted',
    'cancelled',
    'expired',
    'already-running',
  ],
  restart: ['restarted', 'deferred', 'queued', 'failed', 'accepted', 'conflict', 'interrupted', 'cancelled', 'expired', 'already-running'],
  boot: ['rebooting', 'rebooted', 'armed', 'noop', 'deferred', 'queued', 'failed', 'conflict', 'interrupted', 'cancelled', 'expired', 'already-running'],
  sleep: ['sleeping', 'slept', 'deferred', 'queued', 'failed', 'conflict', 'interrupted', 'cancelled', 'expired', 'already-running'],
  run: ['ran', 'deferred', 'queued', 'failed', 'accepted', 'conflict', 'interrupted', 'cancelled', 'expired', 'already-running'],
  cycle: ['cycled', 'skipped', 'failed', 'conflict', 'already-running'],
  'self-update': ['installed', 'rolled-back', 'noop', 'checked', 'failed', 'conflict', 'accepted', 'already-running'],
};

// ---------------------------------------------------------------------------
// Outcome
// ---------------------------------------------------------------------------

/**
 * What a client decides on (contract section 5.6). `ok` keeps its older
 * meanings, which differ between a reply and a record; `outcome` is the one
 * field that says the same thing everywhere.
 */
export const OUTCOMES = ['succeeded', 'failed', 'deferred', 'pending', 'unknown'];

/**
 * The actions rule 7 answers with `ok: true` and exit 0, whatever the work
 * itself returned: they are answers about the request, not failures of it.
 */
export const OK_REPLY_ACTIONS = new Set(['accepted', 'already-running', 'queued', 'deferred', 'noop', 'conflict']);

const PENDING_ACTIONS = new Set(['accepted', 'already-running', 'queued', 'rebooting', 'sleeping']);
const SUCCESS_ACTIONS = new Set([
  'updated', 'restarted', 'ran', 'rebooted', 'slept', 'armed', 'installed',
  'cycled', 'noop', 'checked', 'skipped', 'stored', 'replaced',
]);
const DEFERRED_ACTIONS = new Set(['deferred', 'conflict', 'expired']);

/**
 * The outcome of a reply, a result, a summary or a cycle child, exactly as the
 * reducer of section 5.6 gives it. Every client ports the same function, and
 * contract/vectors/outcome.json is what proves this copy agrees with theirs.
 *
 * It never invents optimism: where the fields do not say, the answer is
 * `unknown`, and `pending` belongs only to work the agent still owns.
 */
export function outcomeOf(x) {
  if (typeof x.outcome === 'string') return OUTCOMES.includes(x.outcome) ? x.outcome : 'unknown';
  if (x.state === 'queued' || x.state === 'running') return 'pending';

  const { action, reasonCode } = x;
  if (action === undefined || action === null) {
    if (x.ok === true) return reasonCode == null ? 'succeeded' : 'unknown';
    if (x.ok === false) {
      if (reasonCode === REASON.operationInProgress || reasonCode === REASON.lockHeld) return 'deferred';
      if (reasonCode === REASON.timedOut || reasonCode === REASON.interrupted || reasonCode === REASON.internal) return 'unknown';
      return 'failed';
    }
    return 'unknown';
  }
  if (PENDING_ACTIONS.has(action)) return 'pending';
  if (SUCCESS_ACTIONS.has(action)) return 'succeeded';
  if (DEFERRED_ACTIONS.has(action)) return 'deferred';
  if (action === 'cancelled') {
    if (reasonCode == null) return 'succeeded';
    if (reasonCode === REASON.cancelled) return 'deferred';
    return reasonCode === REASON.internal ? 'unknown' : 'failed';
  }
  if (action === 'rolled-back') return reasonCode == null ? 'succeeded' : 'failed';
  if (action === 'interrupted') return 'unknown';
  if (action === 'failed') return reasonCode === REASON.timedOut || reasonCode === REASON.interrupted ? 'unknown' : 'failed';
  return 'unknown';
}

/**
 * The outcome the agent states for something it produced.
 *
 * It is the reducer's answer for the fields beside it, with one exception: a
 * producer that knows a failed run may have changed the machine marks it
 * `outcome: 'unknown'`, so no client resends it. Lowering `failed` to
 * `unknown` is the only move section 5.6 allows, so any other stated value is
 * recomputed rather than trusted, and the agent can never report something
 * more optimistic than the function.
 */
export function producedOutcome(x) {
  const { outcome, ...fields } = x;
  const reduced = outcomeOf(fields);
  return outcome === 'unknown' && reduced === 'failed' ? 'unknown' : reduced;
}

// ---------------------------------------------------------------------------
// The token grammar
// ---------------------------------------------------------------------------

/**
 * Every positional argument and every option value that arrives over ssh.
 *
 * Clients send command lines unquoted, so the only safe answer is a character
 * set with nothing a shell would look at twice: no space, no quote, no `$`, no
 * `;`, no backtick, no newline. A value outside it is refused with
 * `bad-argument` rather than passed on and hoped for.
 */
export const TOKEN_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:@/\\~=+-]*$/;

/** Operation ids: a lowercase UUIDv4, or any lowercase id of a sensible length. */
export const OPERATION_ID_PATTERN = /^[a-z0-9-]{8,64}$/;

/** Durations a client may write for an expiry or a pause: 30m, 4h, 2d. */
export const DURATION_PATTERN = /^\d+(m|h|d)$/;

/**
 * Service, action and boot target ids, and the action a wake helper runs.
 * They travel over ssh as bare tokens, so they are a narrower token.
 */
export const CONFIGURED_ID_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._-]*$/;

/** The shared setup's identity: one to 64 characters, and never a path. */
export const SETUP_ID_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:-]{0,63}$/;

/** A local time of day in a maintenance window: 00:00 to 23:59. */
export const CLOCK_TIME_PATTERN = /^(?:[01][0-9]|2[0-3]):[0-5][0-9]$/;

const DURATION_UNITS = { m: 60 * 1000, h: 60 * 60 * 1000, d: 24 * 60 * 60 * 1000 };

/** Milliseconds for a duration token, or null when it is not one. */
export function parseDuration(value) {
  const match = DURATION_PATTERN.exec(String(value ?? ''));
  if (!match) return null;
  const amount = Number.parseInt(match[0].slice(0, -1), 10);
  if (!Number.isFinite(amount) || amount <= 0) return null;
  return amount * DURATION_UNITS[match[1]];
}

/** A duration token or an ISO timestamp, resolved to an absolute ISO time. */
export function resolveDeadline(value, { now = Date.now() } = {}) {
  const ms = parseDuration(value);
  if (ms !== null) return { ok: true, at: new Date(now + ms).toISOString(), error: null };
  const parsed = Date.parse(String(value ?? ''));
  if (!Number.isNaN(parsed)) return { ok: true, at: new Date(parsed).toISOString(), error: null };
  return { ok: false, at: null, error: `expected a duration like 30m, 4h or 2d, or an ISO timestamp; got ${JSON.stringify(value)}` };
}

export function isValidToken(value) {
  return typeof value === 'string' && value.length > 0 && value.length <= 4096 && TOKEN_PATTERN.test(value);
}

export function isValidOperationId(value) {
  return typeof value === 'string' && OPERATION_ID_PATTERN.test(value);
}

// ---------------------------------------------------------------------------
// The reply envelope
// ---------------------------------------------------------------------------

/**
 * Wrap a payload in the envelope every 3.x reply carries.
 *
 * `contract` is what a client gates every v3 feature on; its absence is how a
 * client recognises a 2.x agent. `system` is on every reply, not just status,
 * so a client that got an error still knows which machine said it.
 */
export function envelope(payload, { agentVersion, system, notes = [] }) {
  const reply = {
    ok: payload.ok !== false,
    contract: CONTRACT_VERSION,
    agentVersion,
    ...payload,
  };
  if (system) reply.system = system;
  if (notes.length > 0) reply.notes = [...(payload.notes ?? []), ...notes];
  // A reason code is only meaningful when something did not go the happy way.
  if (reply.reasonCode === null || reply.reasonCode === undefined) delete reply.reasonCode;
  return reply;
}
