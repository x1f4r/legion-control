// Every conformance vector group the agent consumes, run through the agent's
// own code (contract section 14.2).
//
// The clients and the agent each port the same rules: the canonical bytes,
// the identifier grammars, the outcome reducer, the grammar a command line is
// quoted in, the guard on hosts and users, the document checks and the launch
// values. contract/vectors/index.json says which groups the agent runs and how
// many cases each has for it. Each group here has exactly one adapter, which
// either produces the expected value from the input or verifies a property of
// the case, and the counts and the adapters are both held to the index.

import assert from 'node:assert/strict';
import test from 'node:test';
import { CLOCK_TIME_PATTERN, CONFIGURED_ID_PATTERN, DURATION_PATTERN, OPERATION_ID_PATTERN, TOKEN_PATTERN, outcomeOf } from '../src/contract.mjs';
import { HASH_PATTERN, SETUP_ID_PATTERN, canonicalBytes, canonicalHash, storeController } from '../src/controller.mjs';
import { parseCommandLine } from '../src/dispatch.mjs';
import { isLauncherMigration, resolveGrammar } from '../src/launch.mjs';
import { findRepeatedMember, isHostGuard, isUserGuard, validateSetupDocument } from '../src/setup-document.mjs';
import { withHome } from './helpers.mjs';
import { groupsFor } from './vectors.mjs';

const matches = (pattern) => (input) => ({ valid: typeof input.value === 'string' && pattern.test(input.value) });

/**
 * One adapter per `family/group`: `produce(input)` gives the case's `expect`,
 * or `verify(entry)` asserts what the agent promises about the case.
 */
const ADAPTERS = {
  'hash/vectors': {
    async verify(entry) {
      const input = Buffer.from(entry.inputBase64, 'base64');
      if (entry.sha256 === null) {
        assert.throws(() => canonicalHash(input), /UTF-8/);
      } else {
        assert.equal(canonicalHash(input), entry.sha256);
        assert.equal(canonicalBytes(input).toString('base64'), entry.canonicalBase64);
      }
      // A document section 12 accepts is one the agent stores, and nothing else is.
      await withHome(async () => {
        const stored = storeController(input);
        assert.equal(stored.ok, entry.accepted, stored.error ?? 'stored');
      });
    },
  },
  'identifiers/token': { produce: matches(TOKEN_PATTERN) },
  'identifiers/configuredId': { produce: matches(CONFIGURED_ID_PATTERN) },
  'identifiers/setupId': { produce: matches(SETUP_ID_PATTERN) },
  'identifiers/opId': { produce: matches(OPERATION_ID_PATTERN) },
  'identifiers/duration': { produce: matches(DURATION_PATTERN) },
  'identifiers/clockTime': { produce: matches(CLOCK_TIME_PATTERN) },
  'identifiers/sha256': { produce: matches(HASH_PATTERN) },
  'outcome/reduce': { produce: (input) => ({ outcome: outcomeOf(input) }) },
  'transport/grammars': { produce: (input) => resolveGrammar(input) },
  'transport/commandLines': {
    // Whatever a client writes for a restricted system, the dispatcher reads back as the same argv.
    verify(entry) {
      const parsed = parseCommandLine(entry.expect.line);
      assert.equal(parsed.ok, true, parsed.error);
      assert.deepEqual(parsed.tokens, entry.input.argv);
    },
  },
  'transport/hosts': {
    // The guard is looser than a client's dial grammar: everything a client
    // dials passes it, and nothing that injects syntax does.
    verify(entry) {
      if (entry.expect.valid) assert.equal(isHostGuard(entry.input.value), true, 'a dialable host passes the guard');
      if (entry.tags.includes('injection')) assert.equal(isHostGuard(entry.input.value), false, 'an injection never passes the guard');
    },
  },
  'transport/users': { produce: (input) => ({ valid: isUserGuard(input.value) }) },
  'transport/ports': { produce: (input) => ({ valid: Number.isInteger(input.value) && input.value >= 1 && input.value <= 65535 }) },
  'setup-acceptance/documents': {
    // The agent checks a subset of section 9.7: it never refuses a document a
    // client accepts, and always refuses the ones it enforces.
    verify(entry) {
      const text = canonicalBytes(entry.input.text).toString('utf8');
      const repeated = findRepeatedMember(text);
      const errors = repeated ? [repeated] : validateSetupDocument(JSON.parse(text)).errors;
      if (entry.expect.valid) assert.deepEqual(errors, [], 'a valid document is never refused');
      if (entry.tags.includes('agent-enforced')) assert.ok(errors.length > 0, 'an agent-enforced document is always refused');
    },
  },
  'launch/review': {
    // Held as an ordinary session would leave it, the offered descendant is
    // refused to a restricted session exactly when it brings a launch value
    // the held copy does not have.
    async verify(entry) {
      await withHome(async () => {
        if (entry.input.heldText !== null) {
          const held = storeController(entry.input.heldText);
          assert.equal(held.ok, true, held.error);
        }
        const pushed = storeController(entry.input.offeredText, { restricted: true });
        assert.equal(pushed.reasonCode === 'restricted', entry.expect.restrictedRefused, pushed.error ?? pushed.action);
        if (!entry.expect.restrictedRefused) assert.equal(pushed.ok, true, pushed.error);
        else assert.ok(pushed.launchChanges.length > 0);
      });
    },
  },
  'launch/migration': { produce: (input) => ({ migration: isLauncherMigration(input.approved, input.offered) }) },
};

const groups = groupsFor('agent');

test('every group the index gives the agent has exactly one adapter, and every adapter a group', () => {
  assert.deepEqual(Object.keys(ADAPTERS).sort(), groups.map((group) => group.key).sort());
});

for (const group of groups) {
  test(`vectors ${group.key}`, async () => {
    const adapter = ADAPTERS[group.key];
    assert.ok(adapter, `${group.key} has no adapter`);
    assert.equal(group.cases.length, group.count, 'the index count is the number of cases run');
    for (const entry of group.cases) {
      try {
        if (adapter.produce) assert.deepEqual(adapter.produce(entry.input), entry.expect);
        else await adapter.verify(entry);
      } catch (error) {
        error.message = `${group.key}/${entry.name}: ${error.message}`;
        throw error;
      }
    }
  });
}
