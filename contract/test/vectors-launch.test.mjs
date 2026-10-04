// The launch family: which launch values a device runs without asking, which
// wait for a person, and what a restricted key may publish (v3.md section 9.9).
//
// The reference defects are the ways a port could quietly reopen T1: taking a
// launcher back to a direct entry as if it were the migration, ignoring the
// grammar when it compares a migration, comparing the raw restricted field
// instead of the grammar it gives, and approving text a person cannot read as
// it really is. Each must be caught by validate.mjs and by the generator. The
// vector-file defects prove that validate.mjs recomputes what a restricted
// push gets from a record it builds itself, and that every offered document is
// one the agent would store at all.

import test from 'node:test';
import assert from 'node:assert/strict';
import path from 'node:path';
import { CONTRACT, defect, readJson, referenceDefect, writeJson } from './support.mjs';
import {
  approvalsFrom, effectiveLaunch, isLaunchText, isLauncherMigration, launchProjection, launchReview, run,
} from '../tools/launch.mjs';

const launch = readJson(path.join(CONTRACT, 'vectors', 'launch.json'));
const reviews = launch.groups.review;
const migrations = launch.groups.migration;

// --- the reference -------------------------------------------------------------

referenceDefect('launch.mjs accepts the reverse migration', 'tools/launch.mjs',
  'sameTuple(candidate, tuple) || isLauncherMigration(candidate, tuple)',
  'sameTuple(candidate, tuple) || isLauncherMigration(candidate, tuple) || isLauncherMigration(tuple, candidate)',
  /launcher-back-to-entry: tools\/launch\.mjs gives/);

referenceDefect('the grammar ignored in a migration', 'tools/launch.mjs',
  'if (approved.grammar !== offered.grammar || approved.agent.length !== offered.agent.length) return false;',
  'if (approved.agent.length !== offered.agent.length) return false;',
  /migration-different-grammar: tools\/launch\.mjs gives/);

referenceDefect('restricted compared raw instead of through the grammar', 'tools/launch.mjs',
  '  return { agent: [...agent], grammar: r.grammar ?? `refused:${r.refusal}` };',
  '  return { agent: [...agent], grammar: system.restricted === undefined ? r.grammar : `${r.grammar}:${system.restricted}` };',
  /restricted-absent-equals-false: tools\/launch\.mjs gives/);

referenceDefect('bidirectional formatting approved', 'tools/launch.mjs',
  '\\u202A-\\u202E',
  '',
  /argv-with-bidi-control: tools\/launch\.mjs gives/);

referenceDefect('a waiting value falling back to a value approved somewhere else', 'tools/launch.mjs',
  'const recorded = recordedTuple(store, machine.id, system.id);',
  'const recorded = approved.tuples[0];',
  /fallback-to-location-record: tools\/launch\.mjs gives/);

// --- the committed vectors, against validate.mjs's own reading -----------------

const editCase = (name, change) => (dir) => {
  const file = path.join(dir, 'vectors', 'launch.json');
  const data = readJson(file);
  change(data.groups.review.find((entry) => entry.name === name));
  writeJson(file, data);
};

defect('a restricted push said to go through when the held values do not hold it',
  /review\/agent-changed: .*a restricted push is refused, and the case says restrictedRefused false/,
  editCase('agent-changed', (entry) => { entry.expect.restrictedRefused = false; }));

defect('an offered document that does not descend from the held one',
  /review\/repo-changed: offeredText does not descend from heldText/,
  editCase('repo-changed', (entry) => {
    const offered = JSON.parse(entry.input.offeredText);
    offered.controller.lineage = [];
    entry.input.offeredText = `${JSON.stringify(offered, null, 2)}\n`;
  }));

defect('an offered document section 9.7 refuses',
  /review\/display-and-routing-only: .*offeredText is refused by section 9\.7: duplicate-machine/,
  editCase('display-and-routing-only', (entry) => {
    const offered = JSON.parse(entry.input.offeredText);
    offered.machines.push(structuredClone(offered.machines[0]));
    entry.input.offeredText = `${JSON.stringify(offered, null, 2)}\n`;
  }));

// --- coverage --------------------------------------------------------------------

test('every scope, kind and state is pinned, and both answers of every question', () => {
  const entries = reviews.flatMap((entry) => entry.expect.review);
  for (const scope of ['system', 'identityFile', 'repo', 'local']) assert.ok(entries.some((entry) => entry.scope === scope), scope);
  for (const kind of ['changed', 'new']) assert.ok(entries.some((entry) => entry.kind === kind), kind);
  for (const approvable of [true, false]) assert.ok(entries.some((entry) => entry.approvable === approvable), String(approvable));
  const states = new Set(reviews.flatMap((entry) => entry.expect.effective.map((effective) => `${effective.scope}/${effective.state}`)));
  for (const pair of ['system/offered', 'system/approved', 'system/not-driven', 'identityFile/none', 'identityFile/approved', 'repo/approved', 'repo/built-in', 'local/offered', 'local/approved', 'local/none']) {
    assert.ok(states.has(pair), pair);
  }
  for (const refused of [true, false]) assert.ok(reviews.some((entry) => entry.expect.restrictedRefused === refused));
  for (const result of [true, false]) assert.ok(migrations.some((entry) => entry.expect.migration === result));
});

test('the cases section 1.3 lists are all here', () => {
  const names = new Set(reviews.map((entry) => entry.name));
  for (const name of [
    'display-and-routing-only', 'agent-changed', 'verb-appended-to-launcher', 'shell-posix-to-powershell', 'restricted-cleared',
    'restricted-absent-equals-false', 'platform-linux-to-windows-shell-null', 'platform-absent-equals-linux', 'windows-shell-null-to-cmd',
    'launcher-migration-posix', 'launcher-migration-windows-mixed-separators', 'migration-with-new-interpreter',
    'migration-with-grammar-change', 'launcher-back-to-entry', 'migration-with-dotdot-base', 'agent-removed', 'agent-emptied',
    'machine-id-renamed-reusing-tuple', 'new-machine-reusing-launcher', 'new-machine-new-launcher', 'identity-file-added',
    'identity-file-removed', 'repo-changed', 'repo-explicit-default', 'repo-removed', 'local-agent-changed', 'local-default-path',
    'local-disabled', 'unicode-path-not-normalised', 'argv-with-bidi-control', 'restricted-three-element-agent',
    'fallback-to-location-record', 'first-enrolment',
  ]) assert.ok(names.has(name), name);
});

// --- properties ------------------------------------------------------------------

const documents = reviews.flatMap((entry) => [entry.input.heldText, entry.input.offeredText])
  .filter((text) => text !== null)
  .map((text) => JSON.parse(text));

test('a document approves itself: seeding from it leaves nothing to review', () => {
  for (const document of documents) {
    assert.deepEqual(launchReview(approvalsFrom(document), launchProjection(document)), []);
  }
});

test('the launcher migration is never accepted in both directions', () => {
  const tuples = [
    ...migrations.flatMap((entry) => [entry.input.approved, entry.input.offered]),
    ...documents.flatMap((document) => launchProjection(document).machines.flatMap((machine) => machine.systems.map((system) => system.tuple))),
  ];
  for (const a of tuples) {
    for (const b of tuples) assert.ok(!(isLauncherMigration(a, b) && isLauncherMigration(b, a)), JSON.stringify([a, b]));
  }
});

test('nothing runs that was not approved: every effective value is offered and accepted, or recorded at its place', () => {
  for (const entry of reviews) {
    const store = entry.input.approvals ?? (entry.input.heldText === null ? { schema: 1, seededFrom: 'none' } : approvalsFrom(JSON.parse(entry.input.heldText)));
    const projection = launchProjection(JSON.parse(entry.input.offeredText));
    const waiting = new Set(launchReview(store, projection).map((item) => item.path));
    for (const effective of effectiveLaunch(store, projection)) {
      if (effective.state === 'offered') assert.ok(!waiting.has(effective.path), `${entry.name}: ${effective.path} runs while it waits for review`);
      if (effective.state === 'approved') {
        const [, machine, rest] = /^machines\[(.+?)\]\.(.*)$/.exec(effective.path) ?? [];
        if (effective.scope === 'system') {
          const system = /^systems\[(.+)\]$/.exec(rest)[1];
          assert.deepEqual(store.machines[machine].systems[system], { agent: effective.agent, grammar: effective.grammar }, entry.name);
        }
      }
    }
  }
});

test('ids are nested keys in the record, never joined, and stay ordinary keys', () => {
  const document = {
    version: 1,
    machines: [
      { id: 'a/b.c', systems: [{ id: 'x:y', agent: ['node', '/srv/launcher.mjs'] }] },
      { id: '__proto__', systems: [{ id: 'constructor', agent: ['node', '/srv/other.mjs'] }] },
    ],
  };
  const record = approvalsFrom(document);
  assert.deepEqual(record.machines['a/b.c'].systems['x:y'], { agent: ['node', '/srv/launcher.mjs'], grammar: 'posix' });
  assert.ok(Object.hasOwn(record.machines, '__proto__'));
  assert.deepEqual(Object.getPrototypeOf(record.machines), Object.prototype);
  assert.deepEqual(JSON.parse(JSON.stringify(record)).machines.__proto__.systems.constructor.grammar, 'posix');
  assert.deepEqual(launchReview(record, launchProjection(document)), []);
});

test('launch text refuses what a person cannot read as it is, and nothing else', () => {
  for (const ok of ['node', '/home/me/.legion-control/bin/launcher.mjs', 'C:\\Program Files\\nodejs\\node.exe', 'naïve', '~/.ssh/id_tower', '日本', 'a b', 'x'.repeat(4096)]) {
    assert.equal(isLaunchText(ok), true, JSON.stringify(ok));
  }
  for (const bad of ['', 'a\nb', 'a\rb', 'a\u0007b', 'a\u0085b', 'a\u200bb', 'a\u200fb', 'a\u202eb', 'a\u2066b', 'a\u2060b', '\ufeffa', 'x'.repeat(4097), 'é'.repeat(2049)]) {
    assert.equal(isLaunchText(bad), false, JSON.stringify(bad));
  }
});

test('the reference answers every group it is asked and refuses one it does not know', () => {
  assert.throws(() => run('nothing', {}), /no group "nothing"/);
  for (const entry of migrations) assert.deepEqual(run('migration', entry.input), entry.expect);
});
