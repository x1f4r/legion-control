// Sites and wake, v3.md sections 10.1 and 10.2, as cases.
//
// The prefix and broadcast cases are the grammar: what a LAN prefix is, what
// it holds, and the text that looks like one and is not. `192.168.1` holding
// 192.168.10.7 is the bug every client had while prefixes were matched with
// "starts with" (A9). The plan cases all run against one setup,
// WAKE_DEFAULTS in tools/documents.mjs, also written to
// fixtures/controller-document.wake-defaults.json, from a device at a given
// set of addresses, with given helper states and wake holds. They pin what the
// three clients used to disagree about (C8): which broadcast address a direct
// wake uses when the target names none, whether a device that matches two
// sites picks one, whether a command helper runs while another one's result
// is still unknown, and when "wake it first" is offered.

import { WAKE_DEFAULTS_TEXT } from '../documents.mjs';

const CLIENTS = ['mac', 'desktop', 'android'];
const A9 = ['finding:a9'];
const C8 = ['finding:c8'];

// ---------------------------------------------------------------------------
// Prefixes and broadcasts.
// ---------------------------------------------------------------------------

const prefix = (name, purpose, text, addresses, valid, contains) => ({
  name, purpose, tags: A9, input: { prefix: text, addresses }, want: { valid, contains },
});

const prefixes = [
  prefix('dotted-with-dot', 'Three octets and the trailing dot editors write: the /24, and nothing that merely starts with the same characters.',
    '192.168.1.', ['192.168.1.7', '192.168.10.7', '192.168.100.7'], true, [true, false, false]),
  prefix('dotted-without-dot', 'Without the dot it is the same /24, matched on the octet boundary, so 192.168.10.7 is outside it.',
    '192.168.1', ['192.168.1.7', '192.168.10.7'], true, [true, false]),
  prefix('one-octet', 'One octet is a /8, matched on the octet boundary: 100.64.0.1 is not in 10.',
    '10', ['10.20.30.40', '100.64.0.1'], true, [true, false]),
  prefix('one-octet-with-dot', 'One octet and a dot is the same /8.',
    '10.', ['10.0.0.1', '110.0.0.1'], true, [true, false]),
  prefix('cidr-24', 'A CIDR /24 holds its own 256 addresses and not its neighbours\'.',
    '192.168.10.0/24', ['192.168.10.200', '192.168.1.200', '192.168.11.1'], true, [true, false, false]),
  prefix('cidr-12', 'A CIDR length that is not a multiple of eight is matched on the bit boundary.',
    '172.16.0.0/12', ['172.31.255.1', '172.32.0.1'], true, [true, false]),
  prefix('cidr-32', 'A /32 holds exactly one address.',
    '192.168.1.5/32', ['192.168.1.5', '192.168.1.50'], true, [true, false]),
  prefix('empty', 'An empty prefix would match every address with "starts with". It is no prefix.',
    '', ['192.168.1.7'], false, [false]),
  prefix('leading-space', 'Surrounding whitespace is not trimmed: the text is not a prefix and never matches.',
    ' 192.168.1.', ['192.168.1.7'], false, [false]),
  prefix('full-address', 'A full address without a length is not a prefix: it would be a /32 to one reader and a text prefix of 192.168.1.50 to another.',
    '192.168.1.5', ['192.168.1.5', '192.168.1.50'], false, [false, false]),
  prefix('leading-zero', 'An octet with a leading zero is octal to some parsers and decimal to others, so it is neither.',
    '192.168.01.', ['192.168.1.7'], false, [false]),
  prefix('octet-too-large', 'An octet above 255 is no octet, so the text is no prefix and matches nothing.',
    '256.1.', ['10.0.0.1'], false, [false]),
  prefix('double-dot', 'Two trailing dots are not the one optional dot.',
    '192.168.1..', ['192.168.1.7'], false, [false]),
  prefix('length-too-long', 'A CIDR length above 32.',
    '192.168.1.0/33', ['192.168.1.7'], false, [false]),
  prefix('host-bits-set', 'A network with host bits set names an address, not a network, and is refused rather than silently widened.',
    '192.168.1.1/24', ['192.168.1.1', '192.168.1.7'], false, [false, false]),
  prefix('length-too-short', 'A CIDR length below 8 would put a device on a sixteenth of the internet.',
    '10.0.0.0/7', ['10.0.0.1', '10.0.0.200'], false, [false, false]),
  prefix('wildcard', 'A wildcard is not a prefix.',
    '192.168.*', ['192.168.1.7'], false, [false]),
  prefix('ipv6', 'IPv6 prefixes are not part of placement.',
    'fd00::/8', ['192.168.1.7'], false, [false]),
  prefix('addresses-not-ipv4', 'Only a dotted-quad IPv4 address is inside anything: one with an octet above 255 or a leading zero is not.',
    '192.168.1.', ['192.168.1.256', '192.168.001.7'], true, [false, false]),
];

const broadcast = (name, purpose, text, valid) => ({ name, purpose, tags: C8, input: { text }, want: { valid } });

const broadcasts = [
  broadcast('broadcast-directed', 'A directed broadcast address.', '192.168.1.255', true),
  broadcast('broadcast-limited', 'The limited broadcast address.', '255.255.255.255', true),
  broadcast('broadcast-name', 'A name is never resolved: no client sends a magic packet to whatever a resolver says.', 'studio.local', false),
  broadcast('broadcast-octet-too-large', 'An octet above 255 makes it no address at all.', '192.168.1.256', false),
  broadcast('broadcast-hexadecimal', 'inet_aton reads this as 10.0.0.1; this contract reads it as nothing.', '0x0A000001', false),
  broadcast('broadcast-empty', 'An empty entry is skipped, never sent to.', '', false),
  broadcast('broadcast-ipv6', 'IPv6 has no broadcast.', 'fd00::1', false),
  broadcast('broadcast-three-octets', 'Three octets are a prefix, not an address.', '192.168.1', false),
];

// ---------------------------------------------------------------------------
// Plans.
// ---------------------------------------------------------------------------

// Off every site in the setup.
const ELSEWHERE = ['198.51.100.7/24'];

const ready = (actions) => ({ state: 'ready', actions });
const ASLEEP = { state: 'asleep' };
const UNUSABLE = { state: 'unusable' };
const UNKNOWN = { state: 'unknown' };
const NOT_DRIVEN = { state: 'not-driven' };
const TOWER_HELPERS = { pi: ready({ 'wake-tower': 'wol' }), nas: ready({ 'wake-tower': 'command' }) };

const direct = (broadcast, ports) => ({ kind: 'direct', machine: null, action: null, readFirst: null, broadcast, ports, repeats: 3 });
const helper = (machine, action, readFirst = false) => ({ kind: 'helper', machine, action, readFirst, broadcast: null, ports: null, repeats: null });
const skipDirect = (reason) => ({ kind: 'direct', machine: null, action: null, reason });
const skipHelper = (machine, action, reason) => ({ kind: 'helper', machine, action, reason });
const NO_HELPERS = { kind: 'helpers', machine: null, action: null, reason: 'none' };

/**
 * A plan case. `addresses` and `currentSite` are where this device is;
 * `helpers` the last status read of each helper (missing means unknown);
 * `holds` the wake holds this device recorded for the target.
 */
const plan = (name, purpose, target, { addresses, currentSite = null, helpers = {}, holds = [] }, want) => ({
  name, purpose, tags: C8,
  input: { documentText: WAKE_DEFAULTS_TEXT, target, here: { addresses, currentSite }, helpers, holds },
  want: { offerWakeFirst: [], ...want },
});

const plans = [
  plan('at-home', 'At home: the direct step to the site\'s broadcast on the default ports, then the helpers in order.', 'tower',
    { addresses: ['192.168.1.23/24'], helpers: TOWER_HELPERS },
    { matching: ['home'], effective: 'home', steps: [direct(['192.168.1.255'], [9, 7]), helper('pi', 'wake-tower'), helper('nas', 'wake-tower')], skipped: [] }),
  plan('at-the-office', 'At another site: no direct step, only the helpers.', 'tower',
    { addresses: ['192.168.10.5/24'], helpers: TOWER_HELPERS },
    { matching: ['office'], effective: 'office', steps: [helper('pi', 'wake-tower'), helper('nas', 'wake-tower')], skipped: [skipDirect('elsewhere')] }),
  plan('two-sites-match-neither-is-home', 'Two sites match and neither is the target\'s: nothing is effective, and the target is elsewhere either way.', 'tower',
    { addresses: ['192.168.50.9/24'], helpers: TOWER_HELPERS },
    { matching: ['flat', 'attic'], effective: null, steps: [helper('pi', 'wake-tower'), helper('nas', 'wake-tower')], skipped: [skipDirect('elsewhere')] }),
  plan('helper-asleep', 'pi\'s last read found no route to it: skipped, and not offered, because pi has no wake block of its own.', 'tower',
    { addresses: ELSEWHERE, helpers: { pi: ASLEEP, nas: ready({ 'wake-tower': 'command' }) } },
    { matching: [], effective: null, steps: [helper('nas', 'wake-tower')], skipped: [skipDirect('elsewhere'), skipHelper('pi', 'wake-tower', 'asleep')] }),
  plan('asleep-helper-offered', 'nas is asleep and has a wake block of its own, so "wake it first" is offered: a separate press, never a cascade.', 'tower',
    { addresses: ELSEWHERE, helpers: { pi: ready({ 'wake-tower': 'wol' }), nas: ASLEEP } },
    { matching: [], effective: null, steps: [helper('pi', 'wake-tower')], skipped: [skipDirect('elsewhere'), skipHelper('nas', 'wake-tower', 'asleep')], offerWakeFirst: ['nas'] }),
  plan('held-command-helper', 'A hold for tower exists: the command helper is held, the declared wol helper still runs.', 'tower',
    { addresses: ELSEWHERE, helpers: TOWER_HELPERS, holds: [{ helper: 'nas', action: 'wake-tower' }] },
    { matching: [], effective: null, steps: [helper('pi', 'wake-tower')], skipped: [skipDirect('elsewhere'), skipHelper('nas', 'wake-tower', 'held')] }),
  plan('held-unread-helpers', 'With a hold, a helper not read this session cannot be shown to be a wol action, so it is held too.', 'tower',
    { addresses: ELSEWHERE, helpers: { pi: UNKNOWN, nas: UNKNOWN }, holds: [{ helper: 'nas', action: 'wake-tower' }] },
    { matching: [], effective: null, steps: [], skipped: [skipDirect('elsewhere'), skipHelper('pi', 'wake-tower', 'held'), skipHelper('nas', 'wake-tower', 'held')] }),
  plan('held-at-home', 'A hold never stops the direct step.', 'tower',
    { addresses: ['192.168.1.23/24'], helpers: { nas: ready({ 'wake-tower': 'command' }) }, holds: [{ helper: 'nas', action: 'wake-tower' }] },
    { matching: ['home'], effective: 'home', steps: [direct(['192.168.1.255'], [9, 7])], skipped: [skipHelper('pi', 'wake-tower', 'held'), skipHelper('nas', 'wake-tower', 'held')] }),
  plan('directed-broadcast', 'laptop and its site name no broadcast: the directed broadcast of the placing address, on the one port laptop names.', 'laptop',
    { addresses: ['192.168.10.77/24'] },
    { matching: ['office'], effective: 'office', steps: [direct(['192.168.10.255'], [9])], skipped: [] }),
  plan('limited-broadcast', 'Without the address\'s prefix length there is no directed broadcast: 255.255.255.255.', 'laptop',
    { addresses: ['192.168.10.77'] },
    { matching: ['office'], effective: 'office', steps: [direct(['255.255.255.255'], [9])], skipped: [] }),
  plan('lan-prefix-only', 'old-box names no site; its deprecated lanPrefix places it, matched on the octet boundary.', 'old-box',
    { addresses: ['10.0.0.12/24'] },
    { matching: [], effective: null, steps: [direct(['10.0.0.255'], [9, 7])], skipped: [] }),
  plan('lan-prefix-elsewhere', 'An address outside old-box\'s lanPrefix: elsewhere, and with no direct step and no helpers the plan says there is nothing to try.', 'old-box',
    { addresses: ['10.0.10.12/24'] },
    { matching: [], effective: null, steps: [], skipped: [skipDirect('elsewhere'), NO_HELPERS] }),
  plan('current-site-resolves', 'Two sites share the subnet; the person said flat, and flat\'s prefix matches, so flat is effective. studio\'s name broadcast is skipped.', 'studio',
    { addresses: ['192.168.50.9/24'], currentSite: 'flat' },
    { matching: ['flat', 'attic'], effective: 'flat', steps: [direct(['192.168.50.255'], [9, 7])], skipped: [] }),
  plan('ambiguous', 'The same place without the person\'s answer: two sites match, none is picked, and the direct step is skipped as ambiguous.', 'studio',
    { addresses: ['192.168.50.9/24'] },
    { matching: ['flat', 'attic'], effective: null, steps: [], skipped: [skipDirect('ambiguous'), NO_HELPERS] }),
  plan('current-site-not-corroborated', 'The person said flat, but no address is on flat\'s prefix: the answer does not place a device on a network it is not on.', 'studio',
    { addresses: ELSEWHERE, currentSite: 'flat' },
    { matching: [], effective: null, steps: [], skipped: [skipDirect('elsewhere'), NO_HELPERS] }),
  plan('site-without-prefixes', 'cabin has no prefixes, so it is taken on the person\'s word; nothing placed the device on a subnet, so the limited broadcast.', 'shed',
    { addresses: ELSEWHERE, currentSite: 'cabin' },
    { matching: [], effective: 'cabin', steps: [direct(['255.255.255.255'], [9, 7])], skipped: [] }),
  plan('no-action-duplicate-unusable', 'pi does not list wake-tv, the second pi entry repeats the first, and nas cannot be used: every entry is skipped, each with its reason.', 'tv',
    { addresses: ELSEWHERE, helpers: { pi: ready({ 'wake-tower': 'wol' }), nas: UNUSABLE } },
    { matching: [], effective: null, steps: [], skipped: [skipDirect('elsewhere'), skipHelper('pi', 'wake-tv', 'no-action'), skipHelper('pi', 'wake-tv', 'duplicate'), skipHelper('nas', 'wake-tv', 'unusable')] }),
  plan('loopback-and-unknown-site-ignored', 'A loopback address places nothing and a currentSite that names no site is ignored. Helpers not read this session are read just before their step.', 'tower',
    { addresses: ['127.0.0.1/8', '192.168.1.23/24'], currentSite: 'nowhere' },
    { matching: ['home'], effective: 'home', steps: [direct(['192.168.1.255'], [9, 7]), helper('pi', 'wake-tower', true), helper('nas', 'wake-tower', true)], skipped: [] }),
  plan('current-site-elsewhere', 'At home by address, but the person said attic, whose prefix does not match: nothing is effective, and tower is elsewhere.', 'tower',
    { addresses: ['192.168.1.23/24'], currentSite: 'attic', helpers: TOWER_HELPERS },
    { matching: ['home'], effective: null, steps: [helper('pi', 'wake-tower'), helper('nas', 'wake-tower')], skipped: [skipDirect('elsewhere')] }),
  plan('helper-not-driven', 'No system of pi is approved on this device, so pi is unusable.', 'tower',
    { addresses: ELSEWHERE, helpers: { pi: NOT_DRIVEN, nas: ready({ 'wake-tower': 'command' }) } },
    { matching: [], effective: null, steps: [helper('nas', 'wake-tower')], skipped: [skipDirect('elsewhere'), skipHelper('pi', 'wake-tower', 'unusable')] }),
  plan('unread-helper-is-a-step', 'Without a hold, a helper not read this session is a step, read once just before it.', 'tower',
    { addresses: ELSEWHERE, helpers: { pi: UNKNOWN, nas: ready({ 'wake-tower': 'command' }) } },
    { matching: [], effective: null, steps: [helper('pi', 'wake-tower', true), helper('nas', 'wake-tower')], skipped: [skipDirect('elsewhere')] }),
  plan('no-mac', 'pi has no wake block: no magic packet can be addressed to it, whatever the network, and it has no helpers.', 'pi',
    { addresses: ['192.168.1.23/24'] },
    { matching: ['home'], effective: 'home', steps: [], skipped: [skipDirect('no-mac'), NO_HELPERS] }),
];

const readiness = [
  { name: 'nothing-sent', purpose: 'A step from which no packet left waits for nothing and moves straight on.', tags: C8, input: { sentAnything: false }, want: { pollStartsMs: [] } },
  { name: 'after-a-send', purpose: 'The first poll after 5 s, then every 8 s, 12.8 s and 20 s, for as long as a poll starts within 120 s of the send.', tags: C8, input: { sentAnything: true }, want: { pollStartsMs: [5000, 13000, 25800, 45800, 65800, 85800, 105800] } },
];

export default {
  family: 'wake',
  // Sections 10.1 and 10.2; the index names the section that holds both, as
  // the identifiers family names section 1 for its rule 5.
  section: '10',
  reference: 'tools/wake-plan.mjs',
  notes: [
    'prefixes: valid says whether the text is a LAN prefix of section 10.1; contains[i] whether addresses[i], a dotted-quad IPv4 address, is inside it. An invalid prefix contains nothing.',
    'broadcasts: valid says whether a broadcast entry is a dotted-quad IPv4 address a client sends to.',
    'plans: documentText is fixtures/controller-document.wake-defaults.json. here.addresses are this device\'s addresses as a.b.c.d/length or a.b.c.d; one that is not dotted IPv4, or is in 127/8, is ignored. helpers maps a helper to its last status read this session: ready (with actions, action id to kind, when the status listed them), asleep (noRoute or hostUnreachable), unusable (authenticationFailed, hostKeyChanged, hostKeyUnknown, interpreterMissing, agentMissing, badRequest), not-driven (no system of it approved here), unknown (not read, or any other ending); a helper missing from the map is unknown. holds are the wake holds recorded for the target.',
    'plans: steps run in order. A direct step is {kind: direct, broadcast, ports, repeats}; a helper step is {kind: helper, machine, action, readFirst}, readFirst when the helper is unknown and is read once just before its step. Every key is present, null where it does not apply. skipped gives the reason for the direct step (no-mac, ambiguous, unplaced, elsewhere) and for each helper entry (missing, self, duplicate, asleep, unusable, no-action, held, checked in that order), and ends with {kind: helpers, reason: none} when there is no direct step and no helper at all.',
    'plans: placement by wake.lanPrefix uses the matcher the prefixes group pins. old-box\'s prefix 10.0.0 ends in 0, where matching it as text happens to agree with section 10.1, so no plan tells the two apart: place machines with the same function the prefixes adapter runs.',
    'readiness: pollStartsMs are the milliseconds after a send at which the readiness polls start.',
  ],
  consumers: { prefixes: CLIENTS, broadcasts: CLIENTS, plans: CLIENTS, readiness: CLIENTS },
  groups: { prefixes, broadcasts, plans, readiness },
};
