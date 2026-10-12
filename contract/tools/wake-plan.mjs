// Sites and wake: the reference of v3.md sections 10.1 and 10.2.
//
// This file starts with the address grammar every other part of the wake rules
// stands on: what a LAN prefix is, what it contains, what a broadcast address
// is, and the document warnings that follow from them. Placement, the wake plan
// and the readiness schedule are built on these functions and pinned by
// contract/vectors/wake.json; document-rules.mjs already reads the warnings, so
// that a setup a client cannot wake the way its author meant is reported
// identically on every platform.
//
// Before this, each client matched a prefix with "starts with" on the text, so
// `192.168.1` matched an address on 192.168.10.x and a device two subnets away
// believed it was at home (A9). A prefix is now parsed into a network and a
// length and matched on octet or bit boundaries, and anything that does not
// parse never matches anything.
//
// No dependencies, and nothing here resolves a name: a broadcast entry that is
// not a dotted IPv4 address is skipped, never looked up.

export const FAMILIES = ['wake'];

const OCTET = /^(?:0|[1-9][0-9]{0,2})$/;
// A CIDR length from 8 to 32, written without leading zeros.
const LENGTH = /^(?:[89]|[12][0-9]|3[0-2])$/;
// The prefix length of one of this device's own addresses, 0 to 32.
const LOCAL_LENGTH = /^(?:[0-9]|[12][0-9]|3[0-2])$/;

const isPlainObject = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);
const own = (object, key) => (isPlainObject(object) && Object.hasOwn(object, key) ? object[key] : undefined);

function octet(text) {
  if (!OCTET.test(text)) return null;
  const value = Number(text);
  return value <= 255 ? value : null;
}

/** The mask of the first `length` bits, as an unsigned 32-bit number. */
function maskOf(length) {
  return length === 0 ? 0 : (0xffffffff << (32 - length)) >>> 0;
}

/**
 * A dotted-quad IPv4 address as an unsigned 32-bit number, or null. Exactly
 * four decimal octets from 0 to 255 without leading zeros: `192.168.01.1` is
 * octal to some parsers and decimal to others, so it is neither.
 */
export function parseIPv4(text) {
  if (typeof text !== 'string') return null;
  const parts = text.split('.');
  if (parts.length !== 4) return null;
  let value = 0;
  for (const part of parts) {
    const byte = octet(part);
    if (byte === null) return null;
    value = value * 256 + byte;
  }
  return value;
}

/**
 * A `sites[].lanPrefixes` entry or a `wake.lanPrefix`, parsed (section 10.1):
 * `{ network, length, form }` or null.
 *
 * - dotted: one to three octets, optionally followed by a single dot. The
 *   length is eight bits per octet, so `192.168.1` and `192.168.1.` are the
 *   same /24 and neither contains 192.168.10.7.
 * - cidr: a dotted-quad network, `/`, and a length from 8 to 32, with no host
 *   bits set. `192.168.1.1/24` names an address, not a network, and is refused
 *   rather than silently widened.
 *
 * Anything else, a full address without a length, IPv6, a wildcard, surrounding
 * whitespace, is not a prefix.
 */
export function parseLanPrefix(text) {
  if (typeof text !== 'string' || text.length === 0) return null;
  const slash = text.indexOf('/');
  if (slash >= 0) {
    const network = parseIPv4(text.slice(0, slash));
    const lengthText = text.slice(slash + 1);
    if (network === null || !LENGTH.test(lengthText)) return null;
    const length = Number(lengthText);
    if (((network & ~maskOf(length)) >>> 0) !== 0) return null;
    return { network, length, form: 'cidr' };
  }
  const body = text.endsWith('.') ? text.slice(0, -1) : text;
  const parts = body.split('.');
  if (parts.length < 1 || parts.length > 3) return null;
  let network = 0;
  for (const part of parts) {
    const byte = octet(part);
    if (byte === null) return null;
    network = network * 256 + byte;
  }
  network = (network * 2 ** (8 * (4 - parts.length))) >>> 0;
  return { network, length: 8 * parts.length, form: 'dotted' };
}

/** Whether `address` (an unsigned 32-bit IPv4 address) is inside a parsed prefix. */
export function prefixContains(prefix, address) {
  const mask = maskOf(prefix.length);
  return ((address & mask) >>> 0) === ((prefix.network & mask) >>> 0);
}

/** Whether two parsed prefixes share any address: the shorter one contains the other's network. */
export function prefixesOverlap(a, b) {
  const mask = maskOf(Math.min(a.length, b.length));
  return ((a.network & mask) >>> 0) === ((b.network & mask) >>> 0);
}

/** The entries of a broadcast list that are dotted IPv4 addresses, the only ones a client sends to. */
function validBroadcasts(list) {
  return Array.isArray(list) ? list.filter((entry) => parseIPv4(entry) !== null) : [];
}

/**
 * What a client that predates section 10.1 does with a dotted prefix written
 * without its trailing dot. It matches the prefix as text, so it also takes an
 * address whose next octet merely begins with the prefix's last one:
 * `192.168.1` takes 192.168.10.x to 192.168.19.x and 192.168.100.x to
 * 192.168.199.x. Octets have no leading zeros and stop at 255, so that needs a
 * last octet from 1 to 25. After a 0, or after 26 and above, no octet
 * continues it and text matching is right by luck, so the message names an
 * address it wrongly takes only when there is one.
 */
function withoutDotMessage(text) {
  const last = Number(text.slice(text.lastIndexOf('.') + 1));
  const start = `${JSON.stringify(text)} has no trailing dot; clients that predate section 10.1 match it as text`;
  if (last >= 1 && last <= 25) return `${start}, so they also take ${text}0.x for ${text}.x`;
  return `${start}, which takes nothing more here only because no octet begins with ${last} but ${last} itself`;
}

/**
 * The warnings of section 9.7 that come from addresses: `shared-prefix`,
 * `prefix-invalid`, `prefix-without-dot`, `broadcast-invalid` and
 * `wake-broadcast-defaulted`, as `{ code, path, message }` in document order.
 *
 * None of them refuses a document. Each says something a person should know
 * about how the setup will behave: that a device on one of two overlapping
 * sites cannot tell which, that a prefix will never match, that an older client
 * will match a prefix more widely than this contract does, that a broadcast
 * entry will be skipped, or that direct wake will fall back to a derived or the
 * limited broadcast address. Shapes the schema refuses are skipped quietly;
 * those are errors, and warnings are only reported for a document without any.
 */
export function prefixWarnings(document) {
  const warnings = [];
  const add = (code, path, message) => warnings.push({ code, path, message });
  const sites = Array.isArray(document?.sites) ? document.sites : [];
  const machines = Array.isArray(document?.machines) ? document.machines : [];

  const checkPrefix = (text, path) => {
    const parsed = parseLanPrefix(text);
    if (parsed === null) {
      add('prefix-invalid', path, `${JSON.stringify(text)} is not a dotted or CIDR IPv4 prefix, so it never matches any address`);
    } else if (parsed.form === 'dotted' && !text.endsWith('.')) {
      add('prefix-without-dot', path, withoutDotMessage(text));
    }
    return parsed;
  };
  const checkBroadcasts = (list, path) => {
    if (!Array.isArray(list)) return;
    list.forEach((entry, index) => {
      if (parseIPv4(entry) === null) {
        add('broadcast-invalid', `${path}[${index}]`, `${JSON.stringify(entry)} is not a dotted IPv4 address, so no client sends a packet to it and none resolves it as a name`);
      }
    });
  };

  // Each site's prefixes and broadcasts, remembering the valid prefixes for the
  // overlap check below.
  const parsedBySite = sites.map((site, index) => {
    if (!isPlainObject(site)) return [];
    const parsed = [];
    if (Array.isArray(site.lanPrefixes)) {
      site.lanPrefixes.forEach((text, k) => {
        const prefix = checkPrefix(text, `sites[${index}].lanPrefixes[${k}]`);
        if (prefix !== null) parsed.push({ text, prefix });
      });
    }
    checkBroadcasts(site.broadcast, `sites[${index}].broadcast`);
    return parsed;
  });

  // Overlapping prefixes on two sites are legal and expected: home routers ship
  // the same default range. The warning tells the reader that matching one no
  // longer says which site a device is on, and that a client then treats its
  // location as unconfirmed unless the person chose one.
  for (let j = 1; j < sites.length; j += 1) {
    for (let i = 0; i < j; i += 1) {
      const pair = parsedBySite[i].flatMap((a) => parsedBySite[j].map((b) => [a, b])).find(([a, b]) => prefixesOverlap(a.prefix, b.prefix));
      if (pair === undefined) continue;
      const [a, b] = pair;
      const what = a.text === b.text ? `both use the prefix ${a.text}` : `have the overlapping prefixes ${a.text} and ${b.text}`;
      add('shared-prefix', `sites[${j}].lanPrefixes`, `sites ${sites[i].id} and ${sites[j].id} ${what}, so matching it does not say which site a device is on; the client must treat its location as unconfirmed unless the user has chosen one`);
    }
  }

  const siteById = new Map();
  for (const site of sites) if (isPlainObject(site) && typeof site.id === 'string' && !siteById.has(site.id)) siteById.set(site.id, site);

  machines.forEach((machine, index) => {
    if (!isPlainObject(machine) || !isPlainObject(machine.wake)) return;
    const { wake } = machine;
    const path = `machines[${index}].wake`;
    const site = typeof machine.site === 'string' ? siteById.get(machine.site) ?? null : null;
    const lanPrefix = wake.lanPrefix === undefined ? null : checkPrefix(wake.lanPrefix, `${path}.lanPrefix`);
    checkBroadcasts(wake.broadcast, `${path}.broadcast`);

    // A machine that can be woken directly: it has a MAC address and a place,
    // either a site in sites[] or, only when it names none, a valid lanPrefix.
    // With no dotted broadcast at either level, direct wake falls back to the
    // directed broadcast of the placing address or to 255.255.255.255.
    const placed = site !== null || lanPrefix !== null;
    if (typeof wake.mac !== 'string' || !placed) return;
    if (validBroadcasts(wake.broadcast).length === 0 && (site === null || validBroadcasts(site.broadcast).length === 0)) {
      add('wake-broadcast-defaulted', path, `${machine.id} can be woken directly and names no dotted broadcast address${site === null ? '' : ` and neither does site ${site.id}`}, so a client sends to the broadcast it derives from its own address, or to 255.255.255.255`);
    }
  });

  return warnings;
}

// ---------------------------------------------------------------------------
// Placement (section 10.1).
// ---------------------------------------------------------------------------

const LOOPBACK = parseLanPrefix('127.0.0.0/8');

/** An unsigned 32-bit IPv4 address as dotted-quad text. */
function formatIPv4(address) {
  return [24, 16, 8, 0].map((shift) => (address >>> shift) & 0xff).join('.');
}

/**
 * One of this device's own addresses, `a.b.c.d` or `a.b.c.d/length`, as
 * `{ address, length }` with `length` null when the interface did not say; or
 * null for anything else and for loopback, which is on no site's network
 * however its prefixes read.
 */
export function parseLocalAddress(text) {
  if (typeof text !== 'string') return null;
  const slash = text.indexOf('/');
  const address = parseIPv4(slash < 0 ? text : text.slice(0, slash));
  if (address === null || prefixContains(LOOPBACK, address)) return null;
  if (slash < 0) return { address, length: null };
  const lengthText = text.slice(slash + 1);
  return LOCAL_LENGTH.test(lengthText) ? { address, length: Number(lengthText) } : null;
}

const sitesOf = (document) => (Array.isArray(document?.sites) ? document.sites.filter(isPlainObject) : []);
const machineOf = (document, id) => (Array.isArray(document?.machines) ? document.machines : [])
  .find((machine) => isPlainObject(machine) && machine.id === id);

/** A site's prefixes that parse; the others never match anything. */
function validPrefixes(site) {
  return (Array.isArray(site.lanPrefixes) ? site.lanPrefixes : []).map(parseLanPrefix).filter((prefix) => prefix !== null);
}

/** The first candidate address inside any of `prefixes`, or null: the placing address. */
function firstInside(prefixes, candidates) {
  return candidates.find((candidate) => prefixes.some((prefix) => prefixContains(prefix, candidate.address))) ?? null;
}

/** This device's candidate addresses, in the order the interfaces gave them. */
const candidatesOf = (here) => (Array.isArray(here?.addresses) ? here.addresses : []).map(parseLocalAddress).filter((candidate) => candidate !== null);

/**
 * Where this device is: `{ matching, effective, ambiguous }`.
 *
 * `matching` is every site, in document order, with a valid prefix that holds
 * one of the candidate addresses. A `currentSite` that names a site decides,
 * but only while that site has no valid prefix or one of its prefixes matches:
 * the person's answer resolves two sites with the same subnet, it does not
 * place a device on a network it is not on. A `currentSite` that names no site
 * is ignored. Otherwise exactly one matching site is effective, and several
 * are ambiguous: a device never picks the first of two homes behind the same
 * stock router.
 */
export function placement(document, here) {
  const sites = sitesOf(document);
  const candidates = candidatesOf(here);
  const matches = (site) => firstInside(validPrefixes(site), candidates) !== null;
  const matching = sites.filter(matches).map((site) => site.id);
  const named = typeof here?.currentSite === 'string' ? sites.find((site) => site.id === here.currentSite) : undefined;
  if (named !== undefined) {
    const corroborated = validPrefixes(named).length === 0 || matches(named);
    return { matching, effective: corroborated ? named.id : null, ambiguous: false };
  }
  if (matching.length === 1) return { matching, effective: matching[0], ambiguous: false };
  return { matching, effective: null, ambiguous: matching.length > 1 };
}

/**
 * Whether machine `targetId` is on this device's network: `{ on, reason,
 * placing }`, the one test route order and direct wake share.
 *
 * A machine that names a site in `sites[]` is on the network exactly when that
 * site is effective; otherwise the reason is `ambiguous` when its site is one
 * of several matching sites and `elsewhere` when not. Only a machine that names
 * no such site falls back to its `wake.lanPrefix`: `unplaced` when that is
 * missing or not a prefix, `elsewhere` when no candidate is inside it.
 * `placing` is the first candidate inside the prefix that placed the machine,
 * `{ address, length }`, or null; a site taken on the person's word with no
 * prefixes places the machine without one.
 */
export function onTargetNetwork(document, targetId, here) {
  const machine = machineOf(document, targetId);
  if (machine === undefined) throw new Error(`the document has no machine "${targetId}"`);
  const candidates = candidatesOf(here);
  const site = typeof machine.site === 'string' ? sitesOf(document).find((entry) => entry.id === machine.site) : undefined;
  if (site !== undefined) {
    const place = placement(document, here);
    if (place.effective === site.id) return { on: true, reason: null, placing: firstInside(validPrefixes(site), candidates) };
    const reason = place.ambiguous && place.matching.includes(site.id) ? 'ambiguous' : 'elsewhere';
    return { on: false, reason, placing: null };
  }
  const prefix = isPlainObject(machine.wake) ? parseLanPrefix(machine.wake.lanPrefix) : null;
  if (prefix === null) return { on: false, reason: 'unplaced', placing: null };
  const placing = firstInside([prefix], candidates);
  return placing === null ? { on: false, reason: 'elsewhere', placing: null } : { on: true, reason: null, placing };
}

// ---------------------------------------------------------------------------
// The plan (section 10.2).
// ---------------------------------------------------------------------------

/** The limited broadcast address, the last resort of direct wake. */
export const LIMITED_BROADCAST = '255.255.255.255';
/** The ports direct wake sends to when the wake block names none it can use. */
export const DEFAULT_PORTS = Object.freeze([9, 7]);
/** Magic packets per broadcast address and port. */
export const REPEATS = 3;

/**
 * Where direct wake sends: the dotted entries of `wake.broadcast`; else those
 * of the machine's site; else the directed broadcast of the placing address
 * when its prefix length is known and at most 30; else 255.255.255.255. A
 * device that guesses the limited broadcast first wakes nothing behind a
 * router that drops it, which is why a site's own broadcast comes before it.
 */
function broadcastsFor(wake, site, placing) {
  const listed = validBroadcasts(wake.broadcast);
  if (listed.length > 0) return listed;
  const shared = site === undefined ? [] : validBroadcasts(site.broadcast);
  if (shared.length > 0) return shared;
  if (placing !== null && placing.length !== null && placing.length <= 30) {
    return [formatIPv4((placing.address | ~maskOf(placing.length)) >>> 0)];
  }
  return [LIMITED_BROADCAST];
}

/** The ports of a wake block from 1 to 65535, or 9 and 7 when it names none of those. */
function portsFor(wake) {
  const ports = (Array.isArray(wake.ports) ? wake.ports : []).filter((port) => Number.isInteger(port) && port >= 1 && port <= 65535);
  return ports.length > 0 ? ports : [...DEFAULT_PORTS];
}

const step = (kind, fields) => ({ kind, machine: null, action: null, readFirst: null, broadcast: null, ports: null, repeats: null, ...fields });
const skip = (kind, machine, action, reason) => ({ kind, machine, action, reason });

/**
 * Why a helper entry is skipped, checked in this order, or null when it is a
 * step: `missing`, `self`, `duplicate`, then from the helper's last status
 * read `asleep`, `unusable`, `no-action`, and `held`.
 *
 * `state` is what that read said: `ready` (with `actions`, action id to kind,
 * when the status listed them), `asleep` (it failed with noRoute or
 * hostUnreachable), `unusable` (a failure no wake can cure), `not-driven` (no
 * system of the helper is approved on this device) or `unknown` (not read this
 * session, or ended some other way). An unknown helper is read once just
 * before its step, so only what needs no status can skip it, and a hold, which
 * no unread helper can be shown to be exempt from.
 */
function helperSkip(document, targetId, entry, earlier, helpers, holds) {
  if (machineOf(document, entry.machine) === undefined) return 'missing';
  if (entry.machine === targetId) return 'self';
  if (earlier.some((other) => other.machine === entry.machine && other.action === entry.action)) return 'duplicate';
  const known = own(helpers, entry.machine);
  const state = known?.state ?? 'unknown';
  if (state === 'asleep') return 'asleep';
  if (state === 'unusable' || state === 'not-driven') return 'unusable';
  const actions = state === 'ready' && isPlainObject(known.actions) ? known.actions : null;
  if (actions !== null && !Object.hasOwn(actions, entry.action)) return 'no-action';
  // While any hold for the target exists, only a declared wol action runs,
  // the held helper's included: a command whose first run may still be going
  // must not get a second one somewhere else.
  if (holds.length > 0 && !(actions !== null && actions[entry.action] === 'wol')) return 'held';
  return null;
}

/**
 * The plan for waking `targetId` from this device: `{ matching, effective,
 * steps, skipped, offerWakeFirst }`.
 *
 * `here` is `{ addresses, currentSite }`; `helpers` maps a machine id to its
 * last status read (`{ state, actions? }`, a missing machine is `unknown`);
 * `holds` are the wake holds this device recorded for the target
 * (`{ helper, action }`).
 *
 * `steps` run in order. A direct step is first when the target has a MAC
 * address and is on this device's network: `{ kind: "direct", broadcast,
 * ports, repeats }`. Then each helper of `wake.helpers`, or of `[wake.helper]`
 * when that is empty, that is not skipped: `{ kind: "helper", machine, action,
 * readFirst }`, `readFirst` when its state is unknown. Every key of a step is
 * present and null where it does not apply. `skipped` says, in the same order,
 * why the direct step or a helper entry is not a step, and ends with
 * `{ kind: "helpers", reason: "none" }` when there is no direct step and no
 * helper at all. `offerWakeFirst` lists the helpers skipped as asleep that
 * have a wake block of their own: "wake it first" is a separate press, never
 * part of this plan.
 */
export function planWake(document, targetId, here, helpers = {}, holds = []) {
  const target = machineOf(document, targetId);
  if (target === undefined) throw new Error(`the document has no machine "${targetId}"`);
  const wake = isPlainObject(target.wake) ? target.wake : null;
  const place = placement(document, here);
  const steps = [];
  const skipped = [];
  const offerWakeFirst = [];

  if (wake === null || typeof wake.mac !== 'string') {
    skipped.push(skip('direct', null, null, 'no-mac'));
  } else {
    const network = onTargetNetwork(document, targetId, here);
    if (!network.on) {
      skipped.push(skip('direct', null, null, network.reason));
    } else {
      const site = typeof target.site === 'string' ? sitesOf(document).find((entry) => entry.id === target.site) : undefined;
      steps.push(step('direct', { broadcast: broadcastsFor(wake, site, network.placing), ports: portsFor(wake), repeats: REPEATS }));
    }
  }

  let list = [];
  if (wake !== null && Array.isArray(wake.helpers) && wake.helpers.length > 0) list = wake.helpers;
  else if (wake !== null && wake.helper !== undefined) list = [wake.helper];
  list = list.filter((entry) => isPlainObject(entry) && typeof entry.machine === 'string' && typeof entry.action === 'string');

  const earlier = [];
  for (const entry of list) {
    const reason = helperSkip(document, targetId, entry, earlier, helpers, holds);
    earlier.push(entry);
    if (reason === null) {
      const state = own(helpers, entry.machine)?.state ?? 'unknown';
      steps.push(step('helper', { machine: entry.machine, action: entry.action, readFirst: state === 'unknown' }));
      continue;
    }
    skipped.push(skip('helper', entry.machine, entry.action, reason));
    if (reason === 'asleep' && isPlainObject(machineOf(document, entry.machine).wake) && !offerWakeFirst.includes(entry.machine)) {
      offerWakeFirst.push(entry.machine);
    }
  }
  if (list.length === 0 && !steps.some((entry) => entry.kind === 'direct')) skipped.push(skip('helpers', null, null, 'none'));

  return { matching: place.matching, effective: place.effective, steps, skipped, offerWakeFirst };
}

// ---------------------------------------------------------------------------
// Readiness (section 10.2, "The walk").
// ---------------------------------------------------------------------------

/** How long after a send a client keeps polling for the target. */
export const READINESS_WINDOW_MS = 120_000;
const FIRST_POLL_MS = 5_000;
const LONGEST_INTERVAL_MS = 20_000;

/**
 * When the readiness polls start, in milliseconds after the send: the first
 * after 5 s, then at intervals growing by 8/5 up to 20 s, for as long as a poll
 * starts within 120 s. A step that sent nothing waits for nothing. The
 * intervals are 5000, 8000, 12800 and then 20000, all exact in any arithmetic.
 */
export function readinessSchedule(sentAnything) {
  const starts = [];
  if (!sentAnything) return starts;
  let wait = FIRST_POLL_MS;
  for (let at = FIRST_POLL_MS; at <= READINESS_WINDOW_MS; at += wait) {
    starts.push(at);
    wait = Math.min((wait * 8) / 5, LONGEST_INTERVAL_MS);
  }
  return starts;
}

// ---------------------------------------------------------------------------
// The reference interface of the `wake` vector family.
// ---------------------------------------------------------------------------

export function run(group, input) {
  switch (group) {
    case 'prefixes': {
      const prefix = parseLanPrefix(input.prefix);
      return {
        valid: prefix !== null,
        contains: input.addresses.map((text) => {
          const address = parseIPv4(text);
          return prefix !== null && address !== null && prefixContains(prefix, address);
        }),
      };
    }
    case 'broadcasts':
      return { valid: parseIPv4(input.text) !== null };
    case 'plans':
      return planWake(JSON.parse(input.documentText), input.target, input.here, input.helpers, input.holds);
    case 'readiness':
      return { pollStartsMs: readinessSchedule(input.sentAnything) };
    default:
      throw new Error(`the wake family has no group "${group}"`);
  }
}
