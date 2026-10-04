// The wake family (v3.md sections 10.1 and 10.2).
//
// The reference defects are what each client did before there was one plan:
// a prefix matched as text, so `192.168.1` held 192.168.10.7 (A9); the first
// of two sites with the same subnet taken as home; the limited broadcast tried
// before the site's own; the person's currentSite trusted on a network it does
// not describe; a command helper run while another command's result was still
// unknown; an action the helper does not have sent anyway (C8). Each must be
// caught by validate.mjs and by the generator. The vector-file defects prove
// validate.mjs reads the plans and the address grammar with code of its own.

import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { CONTRACT, defect, readJson, referenceDefect, writeJson } from './support.mjs';
import { WAKE_DEFAULTS, WAKE_DEFAULTS_TEXT } from '../tools/documents.mjs';
import { checkDocument } from '../tools/document-rules.mjs';
import {
  onTargetNetwork, parseLanPrefix, parseLocalAddress, placement, planWake, prefixWarnings, readinessSchedule,
} from '../tools/wake-plan.mjs';

const wake = readJson(path.join(CONTRACT, 'vectors', 'wake.json'));
const plans = wake.groups.plans;

// --- the reference -------------------------------------------------------------

referenceDefect('a prefix matched as text', 'tools/wake-plan.mjs',
  'return prefix !== null && address !== null && prefixContains(prefix, address);',
  'return prefix !== null && address !== null && text.startsWith(input.prefix.replace(/\\.$/, \'\'));',
  /dotted-without-dot: tools\/wake-plan\.mjs gives/);

referenceDefect('the first of several matching sites taken as the place', 'tools/wake-plan.mjs',
  'if (matching.length === 1) return { matching, effective: matching[0], ambiguous: false };',
  'if (matching.length >= 1) return { matching, effective: matching[0], ambiguous: false };',
  /two-sites-match-neither-is-home: tools\/wake-plan\.mjs gives/);

referenceDefect('the limited broadcast before the site\'s own', 'tools/wake-plan.mjs',
  '  const shared = site === undefined ? [] : validBroadcasts(site.broadcast);',
  '  return [LIMITED_BROADCAST];\n  const shared = site === undefined ? [] : validBroadcasts(site.broadcast);',
  /at-home: tools\/wake-plan\.mjs gives/);

referenceDefect('currentSite taken without an address on its network', 'tools/wake-plan.mjs',
  'effective: corroborated ? named.id : null',
  'effective: named.id',
  /current-site-not-corroborated: tools\/wake-plan\.mjs gives/);

referenceDefect('a command helper run while a hold exists', 'tools/wake-plan.mjs',
  "  if (holds.length > 0 && !(actions !== null && actions[entry.action] === 'wol')) return 'held';\n",
  '',
  /held-command-helper: tools\/wake-plan\.mjs gives/);

referenceDefect('an action the helper does not list sent anyway', 'tools/wake-plan.mjs',
  "  if (actions !== null && !Object.hasOwn(actions, entry.action)) return 'no-action';\n",
  '',
  /no-action-duplicate-unusable: tools\/wake-plan\.mjs gives/);

referenceDefect('readiness polls that keep slowing down past 20 s', 'tools/wake-plan.mjs',
  'wait = Math.min((wait * 8) / 5, LONGEST_INTERVAL_MS);',
  'wait = (wait * 8) / 5;',
  /after-a-send: tools\/wake-plan\.mjs gives/);

// --- the committed vectors, against validate.mjs's own reading -----------------

const editWake = (group, name, change) => (dir) => {
  const file = path.join(dir, 'vectors', 'wake.json');
  const data = readJson(file);
  change(data.groups[group].find((entry) => entry.name === name));
  writeJson(file, data);
};

defect('a plan against another setup than the wake fixture',
  /wake\.json\/plans\/at-home: .*documentText is not fixtures\/controller-document\.wake-defaults\.json/,
  editWake('plans', 'at-home', (entry) => {
    const document = JSON.parse(entry.input.documentText);
    document.sites[0].broadcast = ['192.168.1.127'];
    entry.input.documentText = `${JSON.stringify(document, null, 2)}\n`;
  }));

defect('a name accepted as a broadcast address',
  /wake\.json\/broadcasts\/broadcast-name: whether the entry is a dotted IPv4 address disagrees with the case/,
  editWake('broadcasts', 'broadcast-name', (entry) => { entry.expect.valid = true; }));

defect('a prefix said to hold an address on the next subnet',
  /wake\.json\/prefixes\/dotted-without-dot: read here, "192\.168\.1" is a prefix holding \[true,false\]/,
  editWake('prefixes', 'dotted-without-dot', (entry) => { entry.expect.contains = [true, true]; }));

defect('a direct step to a name',
  /wake\.json\/plans\/current-site-resolves: .*a direct step sends three packets to dotted IPv4 addresses/,
  editWake('plans', 'current-site-resolves', (entry) => { entry.expect.steps[0].broadcast.push('studio.local'); }));

defect('an offer to wake a helper that has no wake block',
  /wake\.json\/plans\/helper-asleep: .*pi is offered to be woken first/,
  editWake('plans', 'helper-asleep', (entry) => { entry.expect.offerWakeFirst = ['pi']; }));

// --- the fixture and the setup ------------------------------------------------

test('the wake fixture is WAKE_DEFAULTS byte for byte, and every plan carries it', () => {
  const file = fs.readFileSync(path.join(CONTRACT, 'fixtures', 'controller-document.wake-defaults.json'), 'utf8');
  assert.equal(file, WAKE_DEFAULTS_TEXT);
  const index = readJson(path.join(CONTRACT, 'fixtures', 'index.json'));
  const entry = index.fixtures.find((candidate) => candidate.file === 'controller-document.wake-defaults.json');
  assert.equal(entry.kind, 'document');
  assert.equal(entry.expect, 'valid');
  assert.deepEqual(entry.tags, ['controller', 'sites', 'wake']);
  for (const plan of plans) assert.equal(plan.input.documentText, WAKE_DEFAULTS_TEXT, plan.name);
  assert.equal(checkDocument(WAKE_DEFAULTS_TEXT, { intake: 'peer' }).valid, true);
});

test('every machine of the wake setup has one endpoint and one Linux system, so only its wake rules differ', () => {
  for (const machine of WAKE_DEFAULTS.machines) {
    assert.deepEqual(machine.endpoints, [{ id: 'remote', host: `${machine.id}.example.net`, user: 'me' }], machine.id);
    assert.deepEqual(machine.systems, [{ id: 'linux', platform: 'linux', agent: ['node', '/home/me/.legion-control/bin/launcher.mjs'] }], machine.id);
  }
});

// --- coverage -----------------------------------------------------------------

test('every reason a direct step or a helper can be skipped for is pinned, except those the shared setup cannot reach', () => {
  const reasons = new Set(plans.flatMap((plan) => plan.expect.skipped.map((skip) => `${skip.kind}:${skip.reason}`)));
  // missing and self are errors of section 9.7 (helper-unknown, helper-self),
  // so no plan against a valid document reaches them. unplaced is only a
  // warning (wake-unplaced), so a valid document can reach it, but the shared
  // setup places every wake block and no vector does. The unit tests below
  // are what pins these three, direct:unplaced included.
  const pinnedBelow = ['direct:unplaced', 'helper:missing', 'helper:self'];
  const expected = ['direct:no-mac', 'direct:ambiguous', 'direct:unplaced', 'direct:elsewhere',
    'helper:missing', 'helper:self', 'helper:duplicate', 'helper:asleep', 'helper:unusable', 'helper:no-action', 'helper:held', 'helpers:none'];
  assert.deepEqual(expected.filter((reason) => !reasons.has(reason) && !pinnedBelow.includes(reason)), []);
  for (const state of ['ready', 'asleep', 'unusable', 'unknown', 'not-driven']) {
    assert.ok(plans.some((plan) => Object.values(plan.input.helpers).some((helper) => helper.state === state)), state);
  }
  assert.ok(plans.some((plan) => plan.expect.offerWakeFirst.length > 0));
  assert.ok(plans.some((plan) => plan.input.holds.length > 0));
});

test('a machine with neither a site nor a valid lanPrefix is unplaced', () => {
  const document = structuredClone(WAKE_DEFAULTS);
  const shed = document.machines.find((machine) => machine.id === 'shed');
  delete shed.site;
  assert.deepEqual(onTargetNetwork(document, 'shed', { addresses: ['192.168.1.23/24'], currentSite: null }), { on: false, reason: 'unplaced', placing: null });
  shed.wake.lanPrefix = '10.0.0.0/7';
  assert.equal(onTargetNetwork(document, 'shed', { addresses: ['10.0.0.1/8'], currentSite: null }).reason, 'unplaced');
  assert.deepEqual(planWake(document, 'shed', { addresses: [], currentSite: null }).skipped.map((skip) => skip.reason), ['unplaced', 'none']);
});

test('a helper that is not in the setup, or is the target itself, is skipped for that, before anything else', () => {
  const document = structuredClone(WAKE_DEFAULTS);
  document.machines[0].wake.helpers = [{ machine: 'ghost', action: 'wake-tower' }, { machine: 'tower', action: 'wake-tower' }];
  const plan = planWake(document, 'tower', { addresses: [], currentSite: null }, { tower: { state: 'asleep' } }, [{ helper: 'nas', action: 'wake-tower' }]);
  assert.deepEqual(plan.skipped.filter((skip) => skip.kind === 'helper').map((skip) => skip.reason), ['missing', 'self']);
});

// --- properties ---------------------------------------------------------------

test('a prefix the vectors call invalid is reported as prefix-invalid, and one without its dot as prefix-without-dot', () => {
  for (const entry of wake.groups.prefixes) {
    const document = { version: 1, sites: [{ id: 'home', lanPrefixes: [entry.input.prefix] }], machines: [] };
    const codes = prefixWarnings(document).map((warning) => warning.code);
    const parsed = parseLanPrefix(entry.input.prefix);
    assert.equal(codes.includes('prefix-invalid'), !entry.expect.valid, entry.name);
    assert.equal(codes.includes('prefix-without-dot'), parsed?.form === 'dotted' && !entry.input.prefix.endsWith('.'), entry.name);
  }
});

test('a site whose prefixes are all invalid is taken on the person\'s word, like a site with none', () => {
  const document = { version: 1, sites: [{ id: 'cabin', lanPrefixes: ['192.168.*'] }], machines: [] };
  assert.equal(placement(document, { addresses: ['198.51.100.7/24'], currentSite: 'cabin' }).effective, 'cabin');
  assert.equal(placement(document, { addresses: ['198.51.100.7/24'], currentSite: null }).effective, null);
});

test('this device\'s addresses: loopback and anything not dotted IPv4 place nothing', () => {
  assert.deepEqual(parseLocalAddress('192.168.1.23/24'), { address: 0xc0a80117, length: 24 });
  assert.deepEqual(parseLocalAddress('192.168.1.23'), { address: 0xc0a80117, length: null });
  for (const text of ['127.0.0.1/8', '127.255.0.9', '192.168.1.23/33', '192.168.1.23/024', 'fe80::1/64', '192.168.1.23 /24', '']) {
    assert.equal(parseLocalAddress(text), null, text);
  }
});

test('the directed broadcast is used only with a known prefix length of at most 30', () => {
  const at = (address) => planWake(WAKE_DEFAULTS, 'laptop', { addresses: [address], currentSite: null }).steps[0].broadcast;
  assert.deepEqual(at('192.168.10.77/30'), ['192.168.10.79']);
  assert.deepEqual(at('192.168.10.77/31'), ['255.255.255.255']);
  assert.deepEqual(at('192.168.10.77/16'), ['192.168.255.255']);
});

test('planning reads its inputs and changes none of them', () => {
  for (const plan of plans) {
    const input = structuredClone(plan.input);
    const document = JSON.parse(input.documentText);
    const before = structuredClone({ document, input });
    planWake(document, input.target, input.here, input.helpers, input.holds);
    assert.deepEqual({ document, input }, before, plan.name);
  }
});

test('readiness polls start within 120 s of a send, at intervals of 5, 8, 12.8 and then 20 s', () => {
  const starts = readinessSchedule(true);
  assert.deepEqual(starts.map((start, index) => start - (index === 0 ? 0 : starts[index - 1])), [5000, 8000, 12800, 20000, 20000, 20000, 20000]);
  assert.ok(starts.at(-1) + 20000 > 120000);
  assert.deepEqual(readinessSchedule(false), []);
});
