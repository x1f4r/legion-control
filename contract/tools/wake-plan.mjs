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

const OCTET = /^(?:0|[1-9][0-9]{0,2})$/;
// A CIDR length from 8 to 32, written without leading zeros.
const LENGTH = /^(?:[89]|[12][0-9]|3[0-2])$/;

const isPlainObject = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);

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
      add('prefix-without-dot', path, `${JSON.stringify(text)} has no trailing dot; clients that predate section 10.1 match it as text, so they also take ${text}0.x for ${text}.x`);
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
