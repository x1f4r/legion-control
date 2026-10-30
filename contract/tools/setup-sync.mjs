// Setup sync and merging: the reference of v3.md sections 9.5 and 9.8.
//
// Every device decides on its own, from one status reply at a time, whether a
// machine is behind it, ahead of it, holding another setup or holding a branch
// of this one. Before this file the three clients each had their own reading of
// that table. Android remembered "already handled" per machine hash for the
// whole session and retried a refused push every fifteen seconds (C7); every
// client decided to fetch from a snapshot, suspended for the read and then
// adopted the bytes over whatever the person had saved meanwhile (M16, A15);
// and the three merges lost different things: a deletion beat an edit without a
// question, endpoints without an id vanished, unknown keys were dropped, merged
// entries moved to the end of their arrays and `22.0` became `22` (C6).
//
// So there is one function for each step, ported by every client and held to
// contract/vectors/setup-reconcile.json and contract/vectors/setup-merge.json:
//
//   decide     the eleven ordered rules of section 9.5, from one status;
//   admit      whether the bytes a fetch read may replace the applied document;
//   gate       the guardrails: one request at a time, a back-off, no push
//              during a running mutation or after a refusal no retry can fix;
//   afterPush  what a push reply means for the next decision;
//   merge      the entries of section 9.8, their kinds, and the merged document.
//
// A merge has to keep what it does not understand exactly as it found it, so
// it works on a JSON tree that keeps member order and number spelling: parsing
// `22.0` into a double and printing it again is precisely how the Mac rewrote
// another peer's document. Equality is by value (two spellings of one number
// are equal), and no floating point decides it.
//
// No dependencies, no clock, no environment, no random source: a time is always
// an argument, so the vectors can pin a back-off to the millisecond.

import { canonicalBytes, canonicalHash } from './canonical.mjs';
import { checkDocument } from './document-rules.mjs';

export const FAMILIES = ['setup-reconcile', 'setup-merge'];

/** How long nothing automatic goes to a machine for the same applied hash after a failed request. */
export const BACKOFF_MS = 60_000;

/** The longest lineage a document carries (section 9.3). */
export const LINEAGE_CAP = 32;

const isPlainObject = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);
const own = (object, key) => (isPlainObject(object) && Object.hasOwn(object, key) ? object[key] : undefined);

/**
 * Strings in code-point order. JavaScript's `<` compares UTF-16 code units,
 * which puts U+FFFD after U+1F600; Swift, Kotlin and C# each have their own
 * default as well. Differences are listed in this order on every platform.
 */
export function compareCodePoints(a, b) {
  const x = [...a];
  const y = [...b];
  for (let index = 0; index < Math.min(x.length, y.length); index += 1) {
    const difference = x[index].codePointAt(0) - y[index].codePointAt(0);
    if (difference !== 0) return difference;
  }
  return x.length - y.length;
}

// ---------------------------------------------------------------------------
// Deciding (section 9.5).
// ---------------------------------------------------------------------------

/**
 * The first of the eleven rules that matches: `{ decision, reason, expect }`.
 *
 * `local` is this device's applied document as `{ hash, id, lineage }`, or null
 * when it has none (or only a hand edit not yet given a revision). `status` is
 * the machine's status reply, of which only `contract` and `controller.hash`
 * and `controller.id` are read. `metaByHash` holds what this device filed about
 * hashes the machine reported: `config meta` answers and the `current` of push
 * refusals. A hash fixes a document, so an entry never goes stale.
 *
 * `expect` is set only for `fetch`: the hash the fetched bytes must have.
 * Rule 7 is checked before rule 8 on purpose. A machine that holds another
 * setup whose hash happens to be in this device's lineage is still another
 * setup, and pushing over it would replace somebody else's fleet.
 */
export function decide({ local, status, metaByHash }) {
  const r = (decision, reason = null, expect = null) => ({ decision, reason, expect });
  if (!status || !(status.contract >= 3) || !status.controller) return r('unsupported');
  if (!local) return r('unknown', 'no-local');
  if (!local.id) return r('unknown', 'local-unidentified');
  const s = status.controller;
  if (!s.hash) return r('push', 'machine-empty');
  if (s.hash === local.hash) return r('inSync');
  const meta = own(metaByHash, s.hash) ?? null;
  const id = meta ? meta.id : s.id;
  if (!id) return r('push', 'machine-unidentified');
  if (id !== local.id) return r('differentSetup');
  if (local.lineage.includes(s.hash)) return r('push', 'machine-behind');
  if (!meta) return r('readMeta');
  if (meta.lineage.includes(local.hash)) return r('fetch', null, s.hash);
  return r('diverged');
}

/**
 * Whether the bytes a fetch read may be adopted, checked in this order:
 * `headMoved`, `moved`, `unverifiable`, `invalid`, `inconsistent`, `adopt`.
 *
 * `expected` is what the decision was made from: the applied hash and setup id,
 * and the remote hash that justified the fetch. `currentLocalHash` is the
 * applied hash read again from storage now, after the read; `fetched` is the
 * canonical hash of the bytes, the `hash` the reply carried, and the id and
 * lineage of the bytes' own controller block; `accepted` is section 9.7 under
 * peer intake.
 *
 * The head check is first because it is the one that protects a person: an
 * edit saved while the read was suspended must never be overwritten by bytes
 * that were chosen against the document it replaced. A hash that is not the
 * expected one but is the reply's own means the machine moved on; one that is
 * neither means the bytes could not be reproduced, which is what an agent that
 * sends no `raw` gives a client that rebuilt them in another spelling (C1).
 *
 * A `config` reply without a `hash` never gets here. The agent sends one
 * whenever it holds a document, so such a reply describes none, and section
 * 9.5 never adopts it: the client counts it as a failed request before it
 * admits anything. `fetched.replyHash` is therefore always a hash, and admit()
 * does not test for its absence.
 */
export function admit({ expected, currentLocalHash, fetched, accepted }) {
  if (currentLocalHash !== expected.localHash) return 'headMoved';
  if (fetched.hash !== expected.remoteHash) return fetched.replyHash === fetched.hash ? 'moved' : 'unverifiable';
  if (!accepted) return 'invalid';
  if (fetched.id !== expected.localId || !fetched.lineage.includes(expected.localHash)) return 'inconsistent';
  return 'adopt';
}

/**
 * The admission outcomes a device remembers, per `settledKey`, so it does not
 * read the same refused document again on every status. Nothing else is
 * remembered as handled: the decision is recomputed from the hashes each time.
 */
export const REMEMBERED_ADMISSIONS = Object.freeze(['invalid', 'unverifiable', 'inconsistent']);

/** The key under which a remembered admission is filed: it changes when either hash or the agent does. */
export const settledKey = (localHash, remoteHash, agentVersion) => `${localHash} ${remoteHash} ${agentVersion}`;

/**
 * A fresh per-machine state, in memory only. `metaByHash` is what `decide`
 * reads under that name, the cache section 9.5 calls `meta`; the other fields
 * are the ones `gate` reads. Times are integer milliseconds on the device's
 * own clock and are only ever compared with each other.
 */
export function peerState() {
  return {
    metaByHash: {},
    inFlight: false,
    failedAt: {},
    held: {},
    localInvalid: false,
    localMutation: false,
    runningOperations: 0,
    lockedAt: null,
    statusAt: null,
    settled: [],
  };
}

/**
 * Whether the request a decision calls for goes out now: `{ action, reason }`,
 * with `action` one of `act`, `hold` and `none`.
 *
 * Only `push`, `readMeta` and `fetch` send anything; every other decision is
 * `none`. One request is in flight per machine, and a failure holds everything
 * automatic for the same applied hash for 60 seconds. The rest applies to
 * pushes only, because reading a machine never changes it: a device whose own
 * document has a section 9.7 error publishes nothing, a push the machine
 * refused with `restricted` or `config-invalid` is not sent again for that
 * hash, and nothing is pushed while this device has an action in flight on the
 * machine, while its last status lists a running operation, or after a lock
 * refusal until a status read after it says nothing is running.
 */
export function gate({ decision, localHash, remoteHash, agentVersion, now, state }) {
  if (!['push', 'readMeta', 'fetch'].includes(decision)) return { action: 'none', reason: null };
  if (state.inFlight) return { action: 'hold', reason: 'in-flight' };
  const failed = own(state.failedAt, localHash);
  if (failed != null && now - failed < BACKOFF_MS) return { action: 'hold', reason: 'backoff' };
  if (decision === 'push') {
    if (state.localInvalid) return { action: 'hold', reason: 'local-invalid' };
    if (own(state.held, localHash)) return { action: 'hold', reason: 'held' };
    if (state.localMutation || state.runningOperations > 0 || (state.lockedAt != null && state.statusAt <= state.lockedAt)) {
      return { action: 'hold', reason: 'operation-running' };
    }
  }
  if (decision === 'fetch' && state.settled.includes(settledKey(localHash, remoteHash, agentVersion))) {
    return { action: 'hold', reason: 'settled' };
  }
  return { action: 'act', reason: null };
}

/**
 * What a push reply means: `{ effect, metaHash }`.
 *
 * `reply` is the parsed reply, or null when none was read: no answer, a dropped
 * link, a timeout, output that is not a reply. That and every refusal not named
 * here is a failure to back off from. `stale-revision` and
 * `controller-conflict` are not failures: they say what the machine holds, and
 * the client files `current` and decides again at once. A lock refusal waits
 * for the operation to finish. `restricted` and `config-invalid` cannot be
 * cured by trying again, so the push is held for that hash until it changes or
 * a person asks. A reply that stored something other than the applied hash is
 * a failure too: the machine now holds a document this device did not send.
 * `now` never changes the effect; recordPush() writes it into the state.
 *
 * Two readings a port might find more natural are failures on purpose. Only a
 * reply with `ok: false` is a refusal: the agent sends it with every refusal
 * code, so a code on anything else is a reply this device cannot account for.
 * And meta is filed under the hash it describes, so a `current` without a hash
 * has nothing to be filed under and says nothing about what the machine holds.
 */
export function afterPush(reply, localHash, now) {
  const r = (effect, metaHash = null) => ({ effect, metaHash });
  if (!isPlainObject(reply)) return r('fail');
  if (reply.ok === true) return r(reply.hash === localHash ? 'clear' : 'fail');
  if (reply.ok !== false) return r('fail');
  const code = reply.reasonCode;
  if ((code === 'stale-revision' || code === 'controller-conflict') && typeof own(reply.current, 'hash') === 'string') {
    return r('fileMeta', reply.current.hash);
  }
  if (code === 'operation-in-progress' || code === 'lock-held') return r('locked');
  if (code === 'restricted' || code === 'config-invalid') return r('hold');
  return r('fail');
}

/**
 * The state after a push reply, as a new state: `clear` forgets the failure,
 * `fail` starts the back-off, `fileMeta` files `current` under its hash,
 * `locked` waits for a later status, `hold` remembers the refusal code for the
 * applied hash. "Try again" is `delete held[localHash]`, and nothing else
 * clears a hold except a new applied hash.
 */
export function recordPush(state, { effect, metaHash }, { reply, localHash, now }) {
  const next = { ...state, failedAt: { ...state.failedAt }, held: { ...state.held }, metaByHash: { ...state.metaByHash }, inFlight: false };
  if (effect === 'clear') delete next.failedAt[localHash];
  else if (effect === 'fail') next.failedAt[localHash] = now;
  else if (effect === 'fileMeta') next.metaByHash[metaHash] = reply.current;
  else if (effect === 'locked') next.lockedAt = now;
  else if (effect === 'hold') next.held[localHash] = reply.reasonCode;
  else throw new Error(`"${effect}" is not an effect of a push reply`);
  return next;
}

// ---------------------------------------------------------------------------
// A JSON tree that keeps what a merge must not change.
//
// Objects keep their members in order, numbers keep their lexeme, strings are
// decoded. A node is one of
//
//   { kind: 'object', members: [[name, node], ...] }
//   { kind: 'array', items: [node, ...] }
//   { kind: 'string', value }
//   { kind: 'number', lexeme }
//   { kind: 'literal', value: true | false | null }
//
// and "absent" is `undefined`.
// ---------------------------------------------------------------------------

const NUMBER = /-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?/y;

/**
 * Parse JSON text into the tree above. Throws on anything that is not exactly
 * one JSON value, and on an object that repeats a member name: parsers
 * disagree about which copy counts, so such a document is never merged
 * (section 9.7 refuses it first).
 */
export function parseJson(text) {
  let at = 0;
  const fail = (what) => {
    throw new Error(`${what} at offset ${at}`);
  };
  const space = () => {
    while (at < text.length && (text[at] === ' ' || text[at] === '\t' || text[at] === '\n' || text[at] === '\r')) at += 1;
  };
  const string = () => {
    const start = at;
    at += 1;
    while (at < text.length && text[at] !== '"') at += text[at] === '\\' ? 2 : 1;
    if (at >= text.length) fail('an unterminated string');
    at += 1;
    // JSON.parse decodes exactly the escapes JSON has, and refuses a raw
    // control character inside the quotes, which is what the grammar says.
    return JSON.parse(text.slice(start, at));
  };
  const value = () => {
    space();
    const character = text[at];
    if (character === '{') {
      at += 1;
      const members = [];
      const names = new Set();
      space();
      if (text[at] === '}') {
        at += 1;
        return { kind: 'object', members };
      }
      for (;;) {
        space();
        if (text[at] !== '"') fail('a member name was expected');
        const name = string();
        if (names.has(name)) fail(`the member name ${JSON.stringify(name)} is repeated`);
        names.add(name);
        space();
        if (text[at] !== ':') fail('a colon was expected');
        at += 1;
        members.push([name, value()]);
        space();
        if (text[at] === ',') {
          at += 1;
          continue;
        }
        if (text[at] === '}') {
          at += 1;
          return { kind: 'object', members };
        }
        fail('a comma or a closing brace was expected');
      }
    }
    if (character === '[') {
      at += 1;
      const items = [];
      space();
      if (text[at] === ']') {
        at += 1;
        return { kind: 'array', items };
      }
      for (;;) {
        items.push(value());
        space();
        if (text[at] === ',') {
          at += 1;
          continue;
        }
        if (text[at] === ']') {
          at += 1;
          return { kind: 'array', items };
        }
        fail('a comma or a closing bracket was expected');
      }
    }
    if (character === '"') return { kind: 'string', value: string() };
    for (const [word, literal] of [['true', true], ['false', false], ['null', null]]) {
      if (text.startsWith(word, at)) {
        at += word.length;
        return { kind: 'literal', value: literal };
      }
    }
    NUMBER.lastIndex = at;
    const number = NUMBER.exec(text);
    if (number === null) fail('a JSON value was expected');
    at += number[0].length;
    return { kind: 'number', lexeme: number[0] };
  };
  const root = value();
  space();
  if (at !== text.length) fail('text after the JSON value');
  return root;
}

function printNode(node, indent) {
  const inner = `${indent}  `;
  switch (node.kind) {
    case 'object':
      if (node.members.length === 0) return '{}';
      return `{\n${node.members.map(([name, child]) => `${inner}${JSON.stringify(name)}: ${printNode(child, inner)}`).join(',\n')}\n${indent}}`;
    case 'array':
      if (node.items.length === 0) return '[]';
      return `[\n${node.items.map((child) => `${inner}${printNode(child, inner)}`).join(',\n')}\n${indent}]`;
    case 'string':
      return JSON.stringify(node.value);
    case 'number':
      return node.lexeme;
    case 'literal':
      return String(node.value);
    default:
      throw new Error(`"${node.kind}" is not a kind of JSON node`);
  }
}

/**
 * The document text of a tree: two-space indentation, LF, one final newline,
 * the layout `JSON.stringify(value, null, 2)` gives, with every number written
 * exactly as it was read.
 */
export function printJson(node) {
  return `${printNode(node, '')}\n`;
}

/**
 * A number lexeme as its exact decimal value, `<sign><digits>e<exponent>` with
 * no leading or trailing zeros in the digits: `22`, `22.0`, `2.2e1` and
 * `220e-1` are all `22e0`, and `0` and `-0` are both `0`. Two lexemes are the
 * same number exactly when their keys are equal, at any size and precision, so
 * `0.1` and `0.10000000000000001` are different numbers here even though a
 * double cannot tell them apart.
 */
export function decimalKey(lexeme) {
  const match = /^(-?)(0|[1-9][0-9]*)(?:\.([0-9]+))?(?:[eE]([+-]?[0-9]+))?$/.exec(lexeme);
  if (match === null) throw new Error(`${JSON.stringify(lexeme)} is not a JSON number`);
  const [, sign, whole, fraction = '', exponent = '0'] = match;
  const digits = `${whole}${fraction}`.replace(/^0+/, '');
  if (digits === '') return '0';
  const significant = digits.replace(/0+$/, '');
  const scale = BigInt(exponent) - BigInt(fraction.length) + BigInt(digits.length - significant.length);
  return `${sign}${significant}e${scale}`;
}

/** The integer a number node holds, as a BigInt, or null when it holds none. */
function integerOf(node) {
  if (node?.kind !== 'number') return null;
  const key = decimalKey(node.lexeme);
  if (key === '0') return 0n;
  const [digits, scale] = key.split('e');
  return BigInt(scale) < 0n ? null : BigInt(digits) * 10n ** BigInt(scale);
}

/**
 * Whether two nodes are the same JSON value: objects with the same names in
 * any order and equal values, arrays element by element, strings by code
 * point, numbers by exact decimal value, literals by identity. Absent equals
 * only absent.
 */
export function jsonEqual(a, b) {
  if (a === undefined || b === undefined) return a === b;
  if (a.kind !== b.kind) return false;
  switch (a.kind) {
    case 'object': {
      if (a.members.length !== b.members.length) return false;
      return a.members.every(([name, child]) => jsonEqual(child, memberOf(b, name)));
    }
    case 'array':
      return a.items.length === b.items.length && a.items.every((child, index) => jsonEqual(child, b.items[index]));
    case 'string':
      return a.value === b.value;
    case 'number':
      return decimalKey(a.lexeme) === decimalKey(b.lexeme);
    default:
      return a.value === b.value;
  }
}

const clone = (node) => structuredClone(node);
const stringNode = (value) => ({ kind: 'string', value });

/** The value of an object's member, or undefined when the node is not an object or has no such member. */
function memberOf(node, name) {
  if (node?.kind !== 'object') return undefined;
  const found = node.members.find(([key]) => key === name);
  return found === undefined ? undefined : found[1];
}

/** Set a member where it stands, or append it when the object has no member of that name. */
function setMember(node, name, value) {
  const index = node.members.findIndex(([key]) => key === name);
  if (index >= 0) node.members[index] = [name, value];
  else node.members.push([name, value]);
}

function removeMember(node, name) {
  const index = node.members.findIndex(([key]) => key === name);
  if (index >= 0) node.members.splice(index, 1);
}

/** The objects of an array node, or none. */
const objectsOf = (node) => (node?.kind === 'array' ? node.items.filter((item) => item.kind === 'object') : []);

/** The string `id` of an object node, or null. */
function idOf(node) {
  const id = memberOf(node, 'id');
  return id?.kind === 'string' ? id.value : null;
}

// ---------------------------------------------------------------------------
// Entries (section 9.8).
// ---------------------------------------------------------------------------

/** The controller keys that are identity, which a merge writes itself and never offers as entries. */
const IDENTITY_KEYS = Object.freeze(['id', 'revision', 'updatedAt', 'source', 'device', 'lineage']);
/** The top-level keys whose contents are entries of their own. */
const STRUCTURE_KEYS = Object.freeze(['controller', 'machines', 'sites']);
/** The members of a machine that are entries of their own when both sides have the machine. */
const MACHINE_PARTS = Object.freeze(['endpoints', 'systems', 'wake']);
/** How the keys of the other entries begin, and the quote that marks a top-level key written in quotes. */
const QUOTED_BEGINNINGS = Object.freeze(['"', 'controller.', 'sites[', 'machines[']);

/**
 * The entry key of a top-level member: its name, unless the name begins the
 * way another entry's key does (`controller.`, `sites[`, `machines[`). Then it
 * is written inside double quotes with nothing escaped, and so is a name that
 * already begins with a quote, which keeps the quoting reversible.
 *
 * An unknown key can be any string. Without the quotes, a peer document with a
 * member called `sites[home]` beside a site `home` would give one key to two
 * entries, and neither a choice nor the list of differences could tell them
 * apart; peer intake accepts that document, so a merge has to as well.
 * Machine, site, system and endpoint ids are tokens, which hold no bracket, so
 * the other keys never collide with each other.
 */
export function topLevelKey(name) {
  return QUOTED_BEGINNINGS.some((beginning) => name.startsWith(beginning)) ? `"${name}"` : name;
}

/**
 * One document's entries, as a Map from key to `{ place, value }`. `paired`
 * holds the ids of the machines both mine and theirs have: those are split
 * into their fields, wake block, systems and endpoints, and every other
 * machine is one entry.
 *
 * Keys are unique in every document a merge takes: section 9.7 refuses two
 * machines, two sites, or two systems or endpoints of one machine with one id,
 * and topLevelKey() keeps unknown names apart from the rest. So a second entry
 * under one key is a bug, and it throws rather than merge one of the two.
 */
function entryMap(document, paired) {
  const map = new Map();
  const add = (key, place, value) => {
    if (map.has(key)) throw new Error(`two entries of one document are both called ${key}`);
    map.set(key, { place, value });
  };
  if (document?.kind !== 'object') return map;
  for (const [name, value] of document.members) {
    if (!STRUCTURE_KEYS.includes(name)) add(topLevelKey(name), { scope: 'top', name }, value);
  }
  const controller = memberOf(document, 'controller');
  if (controller?.kind === 'object') {
    for (const [name, value] of controller.members) {
      if (!IDENTITY_KEYS.includes(name)) add(`controller.${name}`, { scope: 'controller', name }, value);
    }
  }
  for (const site of objectsOf(memberOf(document, 'sites'))) {
    const id = idOf(site);
    if (id !== null) add(`sites[${id}]`, { scope: 'site', id }, site);
  }
  for (const machine of objectsOf(memberOf(document, 'machines'))) {
    const id = idOf(machine);
    if (id === null) continue;
    const at = `machines[${id}]`;
    if (!paired.has(id)) {
      add(at, { scope: 'machine', id }, machine);
      continue;
    }
    add(`${at}.fields`, { scope: 'fields', machine: id }, { kind: 'object', members: machine.members.filter(([name]) => !MACHINE_PARTS.includes(name)) });
    const wake = memberOf(machine, 'wake');
    if (wake !== undefined) add(`${at}.wake`, { scope: 'wake', machine: id }, wake);
    for (const system of objectsOf(memberOf(machine, 'systems'))) {
      const systemId = idOf(system);
      if (systemId !== null) add(`${at}.systems[${systemId}]`, { scope: 'system', machine: id, id: systemId }, system);
    }
    // Endpoints without an id cannot be told apart, so together, in order,
    // they are one entry. None at all is absent.
    const idless = [];
    for (const endpoint of objectsOf(memberOf(machine, 'endpoints'))) {
      const endpointId = idOf(endpoint);
      if (endpointId === null) idless.push(endpoint);
      else add(`${at}.endpoints[${endpointId}]`, { scope: 'endpoint', machine: id, id: endpointId }, endpoint);
    }
    if (idless.length > 0) add(`${at}.endpoints[]`, { scope: 'idless', machine: id }, { kind: 'array', items: idless });
  }
  return map;
}

const machineIds = (document) => new Set(objectsOf(memberOf(document, 'machines')).map(idOf).filter((id) => id !== null));

/** The ids of the machines both documents have, which merge part by part. */
function pairedMachines(mine, theirs) {
  const theirsIds = machineIds(theirs);
  return new Set([...machineIds(mine)].filter((id) => theirsIds.has(id)));
}

/**
 * Every entry of mine and theirs, in code-point order of its key:
 * `{ entry, place, mine, theirs, base }`, each value a node or undefined for
 * absent. `base` is undefined throughout when there is no base document.
 */
export function entries(mine, theirs, base = null) {
  const paired = pairedMachines(mine, theirs);
  const ofMine = entryMap(mine, paired);
  const ofTheirs = entryMap(theirs, paired);
  const ofBase = base === null ? new Map() : entryMap(base, paired);
  const keys = [...new Set([...ofMine.keys(), ...ofTheirs.keys()])].sort(compareCodePoints);
  return keys.map((entry) => ({
    entry,
    place: (ofMine.get(entry) ?? ofTheirs.get(entry)).place,
    mine: ofMine.get(entry)?.value,
    theirs: ofTheirs.get(entry)?.value,
    base: ofBase.get(entry)?.value,
  }));
}

/**
 * The kind of a difference: with a base, the side that left the entry as the
 * base had it did not change it, so the other side's value is taken (`mine`,
 * `theirs`); when both changed it, a deletion against an edit included, it is
 * `both`. Without a base nothing says who changed what (`noBase`).
 */
function kindOf(found, hasBase) {
  if (!hasBase) return 'noBase';
  if (jsonEqual(found.theirs, found.base)) return 'mine';
  if (jsonEqual(found.mine, found.base)) return 'theirs';
  return 'both';
}

function differing(mine, theirs, base) {
  return entries(mine, theirs, base)
    .filter((found) => !jsonEqual(found.mine, found.theirs))
    .map((found) => ({ ...found, kind: kindOf(found, base !== null) }));
}

/**
 * The entries where mine and theirs differ, as `{ entry, kind }` in code-point
 * order of the key. A reorder alone, a different spelling of the same number
 * and a member written in another order are not differences.
 */
export function differences(mine, theirs, base = null) {
  return differing(mine, theirs, base).map(({ entry, kind }) => ({ entry, kind }));
}

// ---------------------------------------------------------------------------
// Bases and lineage.
// ---------------------------------------------------------------------------

/**
 * The base of a merge: the first hash of mine's lineage, newest first, that is
 * also in theirs' lineage and whose kept bytes still hash to it; null when
 * there is none. `kept` maps a hash to the text this device kept for it. The
 * heads themselves are never a base: a head is not in its own lineage, and a
 * merge whose base was one side would only ever take the other side.
 */
export function commonBase({ mineHash, theirsHash, mineLineage, theirsLineage, kept }) {
  for (const hash of mineLineage) {
    if (hash === mineHash || hash === theirsHash || !theirsLineage.includes(hash)) continue;
    const text = own(kept, hash);
    // A kept revision is read like anything else a merge reads, under peer
    // intake: one this build refuses, kept by an older one, is skipped exactly
    // like one that is not kept (section 9.8).
    if (typeof text === 'string' && canonicalHash(text) === hash && checkDocument(text, { intake: 'peer' }).errors.length === 0) return hash;
  }
  return null;
}

/**
 * The lineage of a merge result: mine's hash, theirs' hash, then the two
 * lineages interleaved, mine first, each hash once, cut at 32. Both heads are
 * in it, so both branches fast-forward onto the result.
 */
export function mergedLineage(mineHash, theirsHash, mineLineage, theirsLineage) {
  const lineage = [];
  const add = (hash) => {
    if (typeof hash === 'string' && !lineage.includes(hash)) lineage.push(hash);
  };
  add(mineHash);
  add(theirsHash);
  for (let index = 0; index < Math.max(mineLineage.length, theirsLineage.length); index += 1) {
    if (index < mineLineage.length) add(mineLineage[index]);
    if (index < theirsLineage.length) add(theirsLineage[index]);
  }
  return lineage.slice(0, LINEAGE_CAP);
}

/** The lineage of a document's controller block, as hashes. */
function lineageOf(document) {
  const lineage = memberOf(memberOf(document, 'controller'), 'lineage');
  return lineage?.kind === 'array' ? lineage.items.filter((item) => item.kind === 'string').map((item) => item.value) : [];
}

// ---------------------------------------------------------------------------
// Merging (section 9.8).
// ---------------------------------------------------------------------------

/**
 * In an array node of objects keyed by id: remove the element (value
 * undefined) or replace it where it stands. Returns whether the id was there.
 */
function replaceById(array, id, value) {
  const index = array === undefined ? -1 : array.items.findIndex((item) => idOf(item) === id);
  if (index < 0) return false;
  if (value === undefined) array.items.splice(index, 1);
  else array.items[index] = clone(value);
  return true;
}

function machineIn(result, id) {
  return objectsOf(memberOf(result, 'machines')).find((machine) => idOf(machine) === id);
}

/** Take one entry from theirs: remove what theirs lacks, replace in place what both have. Insertions come later. */
function takeTheirs(result, found) {
  const { place, theirs } = found;
  const setOrRemove = (holder, name) => {
    if (theirs === undefined) removeMember(holder, name);
    else setMember(holder, name, clone(theirs));
  };
  switch (place.scope) {
    case 'top':
      setOrRemove(result, place.name);
      return;
    case 'controller':
      setOrRemove(memberOf(result, 'controller'), place.name);
      return;
    case 'site':
      replaceById(memberOf(result, 'sites'), place.id, theirs);
      return;
    case 'machine':
      replaceById(memberOf(result, 'machines'), place.id, theirs);
      return;
    case 'fields': {
      // The machine's own members, other than its parts, become theirs: each
      // where mine has it, gone where theirs lacks it, appended where it is new.
      const machine = machineIn(result, place.machine);
      machine.members = machine.members
        .filter(([name]) => MACHINE_PARTS.includes(name) || memberOf(theirs, name) !== undefined)
        .map(([name, value]) => (MACHINE_PARTS.includes(name) ? [name, value] : [name, clone(memberOf(theirs, name))]));
      for (const [name, value] of theirs.members) {
        if (memberOf(machine, name) === undefined) machine.members.push([name, clone(value)]);
      }
      return;
    }
    case 'wake':
      setOrRemove(machineIn(result, place.machine), 'wake');
      return;
    case 'system':
      replaceById(memberOf(machineIn(result, place.machine), 'systems'), place.id, theirs);
      return;
    case 'endpoint':
      replaceById(memberOf(machineIn(result, place.machine), 'endpoints'), place.id, theirs);
      return;
    case 'idless': {
      const endpoints = memberOf(machineIn(result, place.machine), 'endpoints');
      if (endpoints !== undefined) endpoints.items = endpoints.items.filter((item) => idOf(item) !== null);
      return;
    }
    default:
      throw new Error(`"${place.scope}" is not a place an entry can be`);
  }
}

/**
 * Walk one of theirs' arrays in order with a cursor (section 9.8, "Building the
 * result"). An element whose key is in the result moves the cursor to it; an
 * element taken from theirs that is not in the result is inserted after the
 * cursor, first when there is none, and becomes the cursor. Anything else is
 * passed over. So an element theirs added lands after its predecessor in
 * theirs, and everything else keeps mine's order.
 *
 * An element without an id has no key of its own. When the id-less entry is
 * taken from theirs, mine's id-less elements are already gone and each of
 * theirs' is inserted; otherwise theirs' n-th id-less element stands for the
 * result's n-th, and only moves the cursor.
 */
function walkTheirs(holder, name, theirsArray, entryOf, taken) {
  let array = memberOf(holder, name);
  let cursor = -1;
  let idless = 0;
  for (const element of objectsOf(theirsArray)) {
    const id = idOf(element);
    const entry = entryOf(id);
    const items = array === undefined ? [] : array.items;
    let index;
    if (id !== null) {
      index = items.findIndex((item) => idOf(item) === id);
    } else if (taken.has(entry)) {
      index = -1;
    } else {
      const positions = items.flatMap((item, position) => (item.kind === 'object' && idOf(item) === null ? [position] : []));
      index = idless < positions.length ? positions[idless] : -1;
      idless += 1;
    }
    if (index >= 0) {
      cursor = index;
      continue;
    }
    if (!taken.has(entry)) continue;
    // An array mine never had is created only for something that goes in it.
    if (array === undefined) {
      array = { kind: 'array', items: [] };
      setMember(holder, name, array);
    }
    cursor += 1;
    array.items.splice(cursor, 0, clone(element));
  }
}

/**
 * The section 9.7 errors mine may carry into a merge. Each is about how parts
 * that are well formed on their own refer to each other: a site or a helper
 * that names nothing, a helper that is its own, a loop, an alias that
 * disagrees. Mine is the applied document, which applied intake keeps with its
 * errors, and sections 9.5 and 9.7 let a device in that state go on merging.
 * The result is checked like any other, so taking theirs' side of the machine
 * or wake block at fault repairs it, and keeping mine's names the rule.
 *
 * Every other error leaves no single answer to what the document's entries
 * are: a repeated member, a part that is not the array or object the entries
 * are read from, two parts with one id. A mine with one of those is fixed in
 * the editor before it is merged.
 */
export const MERGEABLE_ERRORS = Object.freeze(['unknown-site', 'helper-self', 'helper-unknown', 'helper-alias', 'helper-cycle']);

/**
 * Parse a side of a merge. Theirs and the base are held to section 9.7 under
 * peer intake, as everything read from a machine or kept is. Mine is held to
 * applied intake, and may only break the rules MERGEABLE_ERRORS names.
 * Anything else throws: a client checks the same before it offers Merge or
 * Keep mine, and a merge that never starts is nothing a vector could pin.
 */
function intake(text, side) {
  const verdict = checkDocument(text, { intake: side === 'mine' ? 'applied' : 'peer' });
  const blocking = verdict.errors.filter((problem) => side !== 'mine' || !MERGEABLE_ERRORS.includes(problem.code));
  if (blocking.length > 0) {
    const codes = blocking.map((problem) => problem.code).join(', ');
    throw new Error(side === 'mine'
      ? `mine breaks section 9.7 (${codes}) in a way that leaves its entries undefined; it is fixed in the editor before it is merged`
      : `${side} breaks section 9.7 (${codes}); a theirs or a base is merged only when peer intake accepts it`);
  }
  return parseJson(canonicalBytes(text).toString('utf8'));
}

/**
 * Merge two documents: `{ differences, resultText, error }`.
 *
 * `choices` maps an entry key to `mine` or `theirs`. An entry it does not name
 * takes the side its kind says: `mine` for a `mine` difference, `theirs` for
 * every other, so `both` and `noBase` are preselected theirs. Keep mine is
 * every entry answered `mine`.
 *
 * The result starts from mine. Every entry taken from theirs is removed where
 * theirs lacks it and replaced in place where both have it; then theirs'
 * arrays are walked to insert what theirs adds. Member order is mine's with new
 * members appended, and no value that is not replaced is printed differently.
 * The identity is mine's id, one more than the higher revision, the merged
 * lineage, and the writer's `updatedAt`, `source` and `device`.
 *
 * The result is held to section 9.7 under peer intake. One it refuses is not
 * written: `resultText` is null and `error` is the first of its error codes in
 * code-point order, so the person can be told which rule the choices broke, or
 * which rule mine broke already and the choices kept.
 */
export function merge({ baseText, mineText, theirsText, mineHash, theirsHash, writer, choices = {} }) {
  const mine = intake(mineText, 'mine');
  const theirs = intake(theirsText, 'theirs');
  const base = baseText === null ? null : intake(baseText, 'base');
  if (memberOf(memberOf(mine, 'controller'), 'id')?.kind !== 'string') {
    throw new Error('mine has no setup id; section 9.5 gives a document an identity before it takes part in reconciliation');
  }

  const found = differing(mine, theirs, base);
  const taken = new Set(found
    .filter((difference) => (own(choices, difference.entry) ?? (difference.kind === 'mine' ? 'mine' : 'theirs')) === 'theirs')
    .map((difference) => difference.entry));

  const result = clone(mine);
  for (const difference of found) if (taken.has(difference.entry)) takeTheirs(result, difference);

  walkTheirs(result, 'sites', memberOf(theirs, 'sites'), (id) => `sites[${id}]`, taken);
  walkTheirs(result, 'machines', memberOf(theirs, 'machines'), (id) => `machines[${id}]`, taken);
  const paired = pairedMachines(mine, theirs);
  for (const machine of objectsOf(memberOf(theirs, 'machines'))) {
    const id = idOf(machine);
    if (!paired.has(id)) continue;
    const at = `machines[${id}]`;
    const holder = machineIn(result, id);
    walkTheirs(holder, 'systems', memberOf(machine, 'systems'), (system) => `${at}.systems[${system}]`, taken);
    walkTheirs(holder, 'endpoints', memberOf(machine, 'endpoints'), (endpoint) => (endpoint === null ? `${at}.endpoints[]` : `${at}.endpoints[${endpoint}]`), taken);
  }

  // The identity. Mine's id stays where it is; the rest is written in place,
  // or appended in this order when mine's block lacks it.
  const block = memberOf(result, 'controller');
  const revisionOf = (document) => integerOf(memberOf(memberOf(document, 'controller'), 'revision')) ?? 0n;
  const higher = revisionOf(mine) > revisionOf(theirs) ? revisionOf(mine) : revisionOf(theirs);
  setMember(block, 'revision', { kind: 'number', lexeme: (higher + 1n).toString() });
  setMember(block, 'updatedAt', stringNode(writer.now));
  setMember(block, 'source', stringNode(writer.source));
  setMember(block, 'device', stringNode(writer.device));
  setMember(block, 'lineage', { kind: 'array', items: mergedLineage(mineHash, theirsHash, lineageOf(mine), lineageOf(theirs)).map(stringNode) });

  const text = printJson(result);
  const verdict = checkDocument(text, { intake: 'peer' });
  const codes = [...new Set(verdict.errors.map((problem) => problem.code))].sort(compareCodePoints);
  return {
    differences: found.map(({ entry, kind }) => ({ entry, kind })),
    resultText: verdict.valid ? text : null,
    error: verdict.valid ? null : codes[0],
  };
}

// ---------------------------------------------------------------------------
// The reference interface of the `setup-reconcile` and `setup-merge` vector
// families. Their group names do not overlap, so one run() serves both.
// ---------------------------------------------------------------------------

export function run(group, input) {
  switch (group) {
    case 'decide':
      return decide(input);
    case 'admit':
      return { admission: admit(input) };
    case 'gate':
      return gate(input);
    case 'pushReply':
      return afterPush(input.reply, input.localHash, input.now);
    case 'merges':
      return merge(input);
    case 'bases':
      return { base: commonBase(input) };
    case 'numbers':
      return { equal: decimalKey(input.a) === decimalKey(input.b) };
    default:
      throw new Error(`the setup-reconcile and setup-merge families have no group "${group}"`);
  }
}
