// The outcome of a reply or an operation, and the one function that derives it
// when the agent did not say (v3.md section 5.6).
//
// Agent 3.1.0 states `outcome` on every mutating reply, every record and every
// summary. Older agents do not, and before this file the three clients each
// guessed it from `ok`, `action` and `reasonCode` and guessed differently: one
// called a refusal with a code unknown, the next called it failed, and an
// unknown action with ok true was a success on one platform, unknown on the
// second and a failure on the third. This is the reducer every client ports
// line for line, the agent computes its own `outcome` with it, and
// contract/vectors/outcome.json is what proves the five copies agree.
//
// Two rules shape everything below. A reduction never invents optimism: when
// the evidence does not say, the answer is `unknown`, and `unknown` is never
// retried automatically. And `pending` belongs only to work the agent still
// owns, because it is the one outcome a client keeps polling.

export const FAMILIES = ['outcome'];

export const OUTCOMES = Object.freeze(['succeeded', 'failed', 'deferred', 'pending', 'unknown']);
export const TERMINAL_OUTCOMES = Object.freeze(['succeeded', 'failed', 'deferred', 'unknown']);

/** The actions rule 7 answers with exit 0 whatever `ok` says (v3.md section 1). */
export const OK_REPLY_ACTIONS = Object.freeze(['accepted', 'already-running', 'queued', 'deferred', 'noop', 'conflict']);

const PENDING_ACTIONS = new Set(['accepted', 'already-running', 'queued', 'rebooting', 'sleeping']);
const SUCCESS_ACTIONS = new Set([
  'updated', 'restarted', 'ran', 'rebooted', 'slept', 'armed', 'installed',
  'cycled', 'noop', 'checked', 'skipped', 'stored', 'replaced',
]);
const DEFERRED_ACTIONS = new Set(['deferred', 'conflict', 'expired']);

/** Codes after which nobody knows whether the work took effect. */
const UNKNOWN_FAILURES = new Set(['timed-out', 'interrupted']);

/**
 * The outcome of `x`, which is a reply, a summary, a cycle child, or the record
 * form `recordInput(op)` produces. Exactly the pseudo-code of v3.md section 5.6.
 */
export function reduceOutcome(x) {
  // 1. A stated outcome wins, but only one this version knows. A value from a
  //    later contract is not a reason to believe anything.
  if (typeof x.outcome === 'string') return OUTCOMES.includes(x.outcome) ? x.outcome : 'unknown';

  // 2. Work the agent still owns is pending whatever else the object says; a
  //    running record may already carry the action it is aiming at.
  if (x.state === 'queued' || x.state === 'running') return 'pending';

  // 3. Otherwise the action decides, with the reason code where one action
  //    covers two different endings.
  const { action, reasonCode } = x;
  if (action === undefined || action === null) {
    if (x.ok === true) return reasonCode == null ? 'succeeded' : 'unknown';
    if (x.ok === false) {
      if (reasonCode === 'operation-in-progress' || reasonCode === 'lock-held') return 'deferred';
      if (reasonCode === 'timed-out' || reasonCode === 'interrupted' || reasonCode === 'internal') return 'unknown';
      return 'failed';
    }
    return 'unknown';
  }
  if (PENDING_ACTIONS.has(action)) return 'pending';
  if (SUCCESS_ACTIONS.has(action)) return 'succeeded';
  if (DEFERRED_ACTIONS.has(action)) return 'deferred';
  // A cancelled request was itself deferred; a cancel that worked succeeded.
  if (action === 'cancelled') return reasonCode === 'cancelled' ? 'deferred' : 'succeeded';
  // The old version is back and running: success only when nothing went wrong.
  if (action === 'rolled-back') return reasonCode == null ? 'succeeded' : 'failed';
  if (action === 'interrupted') return 'unknown';
  if (action === 'failed') return UNKNOWN_FAILURES.has(reasonCode) ? 'unknown' : 'failed';
  return 'unknown';
}

/**
 * What a record is reduced from: its result, read in the light of its state and
 * of the outcome stated on the record or, failing that, on the result.
 */
export function recordInput(op) {
  const result = op.result !== null && typeof op.result === 'object' ? op.result : {};
  return { ...result, state: op.state, outcome: op.outcome ?? result.outcome };
}

/** The reference interface of the `outcome` vector family. */
export function run(group, input) {
  if (group === 'reduce') return { outcome: reduceOutcome(input) };
  if (group === 'records') {
    const from = input.kind === 'record' ? recordInput(input.value) : input.value;
    return { outcome: reduceOutcome(from) };
  }
  throw new Error(`the outcome family has no group "${group}"`);
}
