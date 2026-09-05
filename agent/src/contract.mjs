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
 * damage of its own.
 */
export const DISRUPTED_PHASES = new Set(['stopping', 'installing', 'verifying', 'starting', 'rolling-back']);

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
  ],
  restart: ['restarted', 'deferred', 'queued', 'failed', 'accepted', 'conflict', 'interrupted', 'cancelled', 'expired'],
  boot: ['rebooting', 'rebooted', 'armed', 'noop', 'deferred', 'queued', 'failed', 'conflict', 'cancelled', 'expired'],
  sleep: ['sleeping', 'slept', 'deferred', 'queued', 'failed', 'conflict', 'cancelled', 'expired'],
  run: ['ran', 'deferred', 'queued', 'failed', 'accepted', 'conflict', 'interrupted', 'cancelled', 'expired'],
  cycle: ['cycled', 'skipped', 'failed', 'conflict'],
  'self-update': ['installed', 'rolled-back', 'noop', 'failed', 'conflict'],
};

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
