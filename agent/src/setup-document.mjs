// The structure of the setup document, checked before `config set` stores it.
//
// Agents before 3.1.0 stored whatever parsed. Every client then had to refuse
// a bad document on its own, and they did not refuse the same ones: a host
// written as `victim.example,*` reached known_hosts on one platform as a
// pattern pinning one key for every host (T5), a member written twice meant
// one thing to one parser and another to the next, and a document one device
// refused was served to every other device anyway. So the agent checks the
// part of contract section 9.7 that it can check on its own, and refuses a
// document that breaks it with `config-invalid` before anything is written
// (section 9.4).
//
// It is the agent's subset, never more: every rule here is also a rule of
// section 9.7, so a document any client accepts is one the agent stores. What
// a client cannot use but may keep (a machine with no endpoints, a prefix that
// matches nothing) is the client's business, and only the four warnings below
// come back, as notes. Unknown keys are never a problem.
//
// The four string grammars are the guard layer of common.schema.json. They are
// written here as the exact strings the schema holds, and a test compares them,
// because the agent ships without the contract directory and a second spelling
// of a pattern is one more place for the two to drift apart.

import { CONFIGURED_ID_PATTERN, SETUP_ID_PATTERN, TOKEN_PATTERN } from './contract.mjs';

/** common.schema.json#/$defs/sshHostGuard: a name or an address, and nothing ssh reads as syntax. */
export const HOST_GUARD_PATTERN = String.raw`^(?:[A-Za-z0-9_](?:[A-Za-z0-9_-]{0,61}[A-Za-z0-9_])?(?:\.[A-Za-z0-9_](?:[A-Za-z0-9_-]{0,61}[A-Za-z0-9_])?)*\.?|[0-9A-Fa-f.]*:[0-9A-Fa-f:.]*(?:%[A-Za-z0-9._-]{1,32})?)$`;

/** common.schema.json#/$defs/sshUser: a local or domain login name. */
export const USER_PATTERN = String.raw`^(?=[^\n]{1,64}$)(?!.* [ -])[A-Za-z0-9_](?:[A-Za-z0-9._@\\ -]*[A-Za-z0-9._-])?$`;

/** common.schema.json#/$defs/agentArgItem: not empty, no control character. */
export const AGENT_ARG_PATTERN = String.raw`^[^\u0000-\u001f\u007f]+$`;

/** common.schema.json#/$defs/githubRepo: owner/name as GitHub spells them. */
export const REPO_PATTERN = String.raw`^[A-Za-z0-9](?:[A-Za-z0-9-]{0,37}[A-Za-z0-9])?/(?!\.\.?$)[A-Za-z0-9._-]{1,100}$`;

/** A schema can count characters but not bytes, so this one is checked here. */
export const MAX_AGENT_ARG_BYTES = 4096;

const MAC_PATTERN = /^[0-9A-Fa-f]{2}(?:[:-][0-9A-Fa-f]{2}){5}$/;
const PLATFORMS = ['linux', 'windows', 'mac'];
const SHELLS = ['posix', 'powershell', 'cmd'];
const KINDS = ['lan', 'remote'];

/**
 * A string held to one of the patterns above, the way every implementation
 * applies them: the whole input, a line feed or carriage return refused before
 * the pattern runs, and the length limits counted in characters.
 */
function grammar(pattern, { min = 0, max = Infinity } = {}) {
  const expression = new RegExp(pattern, 'u');
  return (value) => {
    if (typeof value !== 'string' || /[\n\r]/.test(value)) return false;
    const length = [...value].length;
    return length >= min && length <= max && expression.test(value);
  };
}

export const isHostGuard = grammar(HOST_GUARD_PATTERN, { min: 1, max: 253 });
export const isUserGuard = grammar(USER_PATTERN);
export const isRepoName = grammar(REPO_PATTERN);
const isArgText = grammar(AGENT_ARG_PATTERN, { min: 1, max: 4096 });

const isToken = (value) => typeof value === 'string' && !/[\n\r]/.test(value) && TOKEN_PATTERN.test(value);
const isSetupId = (value) => typeof value === 'string' && !/[\n\r]/.test(value) && SETUP_ID_PATTERN.test(value);
const isConfiguredId = (value) => typeof value === 'string' && !/[\n\r]/.test(value) && CONFIGURED_ID_PATTERN.test(value);
const isPlainObject = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);
const isText = (value) => typeof value === 'string' && value.length > 0;
const isPort = (value) => Number.isInteger(value) && value >= 1 && value <= 65535;
const hasOwn = (object, key) => Object.hasOwn(object, key);
const arrayOf = (value) => (Array.isArray(value) ? value : []);
const join = (base, key) => (base === '' ? key : `${base}.${key}`);

// ---------------------------------------------------------------------------
// A member written twice
// ---------------------------------------------------------------------------

/**
 * The first object member whose name its object already used, as `{ path }`,
 * or null. `text` is valid JSON. Parsers disagree about which copy of a
 * repeated name counts, the first, the last or neither, so a document that
 * repeats one says different things to different devices and no other check
 * can be trusted with it. Names are compared decoded: `"a"` is `a`.
 *
 * This reads the text rather than a parsed value, because by the time
 * JSON.parse has finished one of the copies is already gone.
 */
export function findRepeatedMember(text) {
  const stack = [];
  const childPath = (frame) => (frame.kind === 'object' ? join(frame.path, frame.key) : `${frame.path}[${frame.index}]`);
  let index = 0;
  while (index < text.length) {
    const character = text[index];
    if (character === '"') {
      let end = index + 1;
      while (end < text.length && text[end] !== '"') end += text[end] === '\\' ? 2 : 1;
      end += 1;
      const top = stack.at(-1);
      if (top?.kind === 'object' && top.expectingName) {
        const name = JSON.parse(text.slice(index, end));
        if (top.names.has(name)) return { path: join(top.path, name) };
        top.names.add(name);
        top.key = name;
        top.expectingName = false;
      }
      index = end;
      continue;
    }
    if (character === '{' || character === '[') {
      const top = stack.at(-1);
      const path = top === undefined ? '' : childPath(top);
      stack.push(character === '{'
        ? { kind: 'object', path, names: new Set(), key: null, expectingName: true }
        : { kind: 'array', path, index: 0 });
    } else if (character === '}' || character === ']') {
      stack.pop();
    } else if (character === ',') {
      const top = stack.at(-1);
      if (top?.kind === 'object') top.expectingName = true;
      else if (top?.kind === 'array') top.index += 1;
    }
    index += 1;
  }
  return null;
}

// ---------------------------------------------------------------------------
// The shape (code `schema`)
// ---------------------------------------------------------------------------

function shapeProblems(document) {
  const problems = [];
  const add = (path, message) => problems.push({ path, code: 'schema', message });

  /** An optional key that, when present, has to pass `rule`. */
  const field = (object, key, path, rule, message) => {
    if (hasOwn(object, key) && !rule(object[key])) add(join(path, key), message);
  };
  const required = (object, keys, path) => {
    for (const key of keys) if (!hasOwn(object, key)) add(join(path, key), 'is required');
  };
  /** An optional array: its type and limit, then `each` on every item. */
  const list = (object, key, path, max, each) => {
    if (!hasOwn(object, key)) return;
    const value = object[key];
    const at = join(path, key);
    if (!Array.isArray(value)) {
      add(at, 'is not an array');
      return;
    }
    if (value.length > max) add(at, `has ${value.length} entries; at most ${max} are allowed`);
    value.forEach((item, index) => each(item, `${at}[${index}]`));
  };
  const object = (value, path) => {
    if (isPlainObject(value)) return true;
    add(path, 'is not an object');
    return false;
  };
  const host = (holder, path) => field(holder, 'host', path, isHostGuard, 'is not a host name or an address, so no client may hand it to ssh');
  const user = (holder, path) => field(holder, 'user', path, isUserGuard, 'is not a login name: letters, digits, . _ - @ \\ and single spaces, so no client may hand it to ssh');
  const port = (holder, path) => field(holder, 'port', path, isPort, 'is not a port: an integer from 1 to 65535');

  if (!isPlainObject(document)) {
    add('', 'the document is not a JSON object');
    return problems;
  }

  if (!hasOwn(document, 'version')) add('version', 'is required, and it is 1');
  else if (document.version !== 1) add('version', 'is not 1, the only version of the setup document there is');
  if (!hasOwn(document, 'machines')) add('machines', 'is required');
  if (isPlainObject(document.controller)) {
    field(document.controller, 'id', 'controller', isSetupId, 'is not a setup id: one to 64 letters, digits and . _ : -, starting with a letter or digit');
  }

  list(document, 'sites', '', 20, (site, path) => {
    if (!object(site, path)) return;
    required(site, ['id'], path);
    field(site, 'id', path, isToken, 'is not a token');
  });

  const helper = (value, path) => {
    if (!object(value, path)) return;
    required(value, ['machine', 'action'], path);
    field(value, 'machine', path, isToken, 'is not a token');
    field(value, 'action', path, isConfiguredId, 'is not an action id: letters, digits, dot, dash and underscore');
  };

  list(document, 'machines', '', 50, (machine, path) => {
    if (!object(machine, path)) return;
    required(machine, ['id'], path);
    field(machine, 'id', path, isToken, 'is not a token');

    list(machine, 'endpoints', path, 20, (endpoint, at) => {
      if (!object(endpoint, at)) return;
      required(endpoint, ['host'], at);
      field(endpoint, 'id', at, isToken, 'is not a token');
      field(endpoint, 'kind', at, (kind) => KINDS.includes(kind), 'is not lan or remote');
      host(endpoint, at);
      port(endpoint, at);
      user(endpoint, at);
      field(endpoint, 'system', at, isToken, 'is not a token');
    });

    if (hasOwn(machine, 'ssh') && object(machine.ssh, join(path, 'ssh'))) {
      const at = join(path, 'ssh');
      host(machine.ssh, at);
      user(machine.ssh, at);
      port(machine.ssh, at);
      field(machine.ssh, 'identityFile', at, isText, 'is not a non-empty string');
    }

    if (hasOwn(machine, 'wake') && object(machine.wake, join(path, 'wake'))) {
      const wake = machine.wake;
      const at = join(path, 'wake');
      field(wake, 'mac', at, (mac) => typeof mac === 'string' && MAC_PATTERN.test(mac), 'is not a MAC address like 02:00:00:00:00:01');
      list(wake, 'broadcast', at, 10, (address, where) => {
        if (!isText(address)) add(where, 'is not a non-empty string');
      });
      list(wake, 'ports', at, 5, (value, where) => {
        if (!isPort(value)) add(where, 'is not a port: an integer from 1 to 65535');
      });
      if (hasOwn(wake, 'probe') && object(wake.probe, join(at, 'probe'))) {
        required(wake.probe, ['host'], join(at, 'probe'));
        host(wake.probe, join(at, 'probe'));
        port(wake.probe, join(at, 'probe'));
      }
      if (hasOwn(wake, 'helper')) helper(wake.helper, join(at, 'helper'));
      list(wake, 'helpers', at, 5, helper);
    }

    list(machine, 'systems', path, 10, (system, at) => {
      if (!object(system, at)) return;
      required(system, ['id'], at);
      field(system, 'id', at, isToken, 'is not a token');
      field(system, 'platform', at, (value) => PLATFORMS.includes(value), `is not one of ${PLATFORMS.join(', ')}`);
      field(system, 'shell', at, (value) => SHELLS.includes(value), `is not one of ${SHELLS.join(', ')}`);
      field(system, 'restricted', at, (value) => typeof value === 'boolean', 'is not true or false');
      // The argv is the one value here a client runs. Its items are named by
      // position, never quoted back: a message has no business repeating a
      // command line.
      list(system, 'agent', at, 20, (item, where) => {
        if (!isArgText(item)) add(where, 'is not an argv item: a non-empty string with no control character');
        else if (Buffer.byteLength(item, 'utf8') > MAX_AGENT_ARG_BYTES) add(where, `is longer than the ${MAX_AGENT_ARG_BYTES} UTF-8 bytes every dispatcher and client allows`);
      });
    });
  });

  if (hasOwn(document, 'appUpdates') && object(document.appUpdates, 'appUpdates')) {
    required(document.appUpdates, ['githubRepo'], 'appUpdates');
    field(document.appUpdates, 'githubRepo', 'appUpdates', isRepoName, 'is not owner/name as GitHub spells them');
  }
  return problems;
}

// ---------------------------------------------------------------------------
// The relationships (the other error codes of section 9.7)
// ---------------------------------------------------------------------------

/**
 * Every wake helper of a machine: the entries of `wake.helpers` and the alias
 * `wake.helper`, even beside an empty `helpers`. Clients disagree about which
 * one counts in that shape, and a helper any of them would run has to name a
 * machine that exists and must not lead back to the machine it wakes.
 */
function helpersOf(machine) {
  const wake = isPlainObject(machine?.wake) ? machine.wake : null;
  if (wake === null) return [];
  const list = [...arrayOf(wake.helpers)];
  if (wake.helper !== undefined) list.push(wake.helper);
  return list.filter((helper) => isPlainObject(helper) && typeof helper.machine === 'string');
}

function relationProblems(document) {
  const problems = [];
  const add = (code, path, message) => problems.push({ path, code, message });
  if (!isPlainObject(document)) return problems;
  const machines = arrayOf(document.machines);
  const sites = arrayOf(document.sites);

  const machineIds = new Set();
  machines.forEach((machine, index) => {
    if (!isPlainObject(machine) || typeof machine.id !== 'string') return;
    if (machineIds.has(machine.id)) add('duplicate-machine', `machines[${index}].id`, `is "${machine.id}", which another machine already uses`);
    machineIds.add(machine.id);

    // An endpoint without an id is never a duplicate: it cannot be remembered
    // as a route, which is a client's warning and no reason to refuse.
    const endpointIds = new Set();
    arrayOf(machine.endpoints).forEach((endpoint, k) => {
      if (!isPlainObject(endpoint) || typeof endpoint.id !== 'string') return;
      if (endpointIds.has(endpoint.id)) add('duplicate-endpoint', `machines[${index}].endpoints[${k}].id`, `is "${endpoint.id}", which another endpoint of ${machine.id} already uses, so a remembered route is ambiguous`);
      endpointIds.add(endpoint.id);
    });

    const systemIds = new Set();
    arrayOf(machine.systems).forEach((system, k) => {
      if (!isPlainObject(system) || typeof system.id !== 'string') return;
      if (systemIds.has(system.id)) add('duplicate-system', `machines[${index}].systems[${k}].id`, `is "${system.id}", which another system of ${machine.id} already uses`);
      systemIds.add(system.id);
    });
  });

  const siteIds = new Set();
  sites.forEach((site, index) => {
    if (!isPlainObject(site) || typeof site.id !== 'string') return;
    if (siteIds.has(site.id)) add('duplicate-site', `sites[${index}].id`, `is "${site.id}", which another site already uses`);
    siteIds.add(site.id);
  });

  machines.forEach((machine, index) => {
    if (!isPlainObject(machine)) return;
    // A setup without sites places nobody, so a site is only checked against
    // a list that exists.
    if (typeof machine.site === 'string' && sites.length > 0 && !siteIds.has(machine.site)) {
      add('unknown-site', `machines[${index}].site`, `is "${machine.site}", which is not in sites[], so this machine drops out of every on-site decision`);
    }
    for (const helper of helpersOf(machine)) {
      if (helper.machine === machine.id) {
        add('helper-self', `machines[${index}].wake`, `names ${machine.id} as its own wake helper, which asks a machine that is off to wake itself`);
      } else if (!machineIds.has(helper.machine)) {
        add('helper-unknown', `machines[${index}].wake`, `names the wake helper "${helper.machine}", which is not a machine in this document`);
      }
    }
    // The singular key is what a 1.2 client reads. When it disagrees with the
    // first entry, an old and a new client try different machines.
    const wake = isPlainObject(machine.wake) ? machine.wake : null;
    if (wake && isPlainObject(wake.helper) && Array.isArray(wake.helpers) && isPlainObject(wake.helpers[0])) {
      const first = wake.helpers[0];
      if (first.machine !== wake.helper.machine || first.action !== wake.helper.action) {
        add('helper-alias', `machines[${index}].wake.helper`, 'does not repeat wake.helpers[0]; the singular key is the compatibility alias of the first entry');
      }
    }
  });

  // A ring of helpers looks fine one machine at a time: every machine in it
  // waits for another to be awake first.
  const byId = new Map();
  for (const machine of machines) if (isPlainObject(machine) && typeof machine.id === 'string' && !byId.has(machine.id)) byId.set(machine.id, machine);
  machines.forEach((machine, index) => {
    if (!isPlainObject(machine) || typeof machine.id !== 'string') return;
    const seen = new Set([machine.id]);
    const walk = (id, trail) => {
      for (const helper of helpersOf(byId.get(id))) {
        if (seen.has(helper.machine)) {
          if (helper.machine === machine.id && trail.length > 0) {
            add('helper-cycle', `machines[${index}].wake`, `is part of a wake helper cycle, ${[...trail, id, helper.machine].join(' needs ')}, so every machine in it waits for another to be awake first`);
          }
          continue;
        }
        if (!byId.has(helper.machine)) continue;
        seen.add(helper.machine);
        walk(helper.machine, [...trail, id]);
      }
    };
    walk(machine.id, []);
  });
  return problems;
}

// ---------------------------------------------------------------------------
// What is accepted with a note (warnings)
// ---------------------------------------------------------------------------

const OCTET = /^(?:0|[1-9][0-9]{0,2})$/;
const CIDR_LENGTH = /^(?:[89]|[12][0-9]|3[0-2])$/;
const maskOf = (length) => (length === 0 ? 0 : (0xffffffff << (32 - length)) >>> 0);

/** A dotted IPv4 address as an unsigned 32-bit number, or null. */
function parseIPv4(text) {
  const parts = typeof text === 'string' ? text.split('.') : [];
  if (parts.length !== 4 || !parts.every((part) => OCTET.test(part) && Number(part) <= 255)) return null;
  return parts.reduce((value, part) => value * 256 + Number(part), 0);
}

/**
 * A LAN prefix as section 10.1 reads it, `{ network, length }` or null: one to
 * three octets with an optional trailing dot, or a CIDR network with no host
 * bits set.
 */
function parseLanPrefix(text) {
  if (typeof text !== 'string' || text.length === 0) return null;
  const slash = text.indexOf('/');
  if (slash >= 0) {
    const network = parseIPv4(text.slice(0, slash));
    const lengthText = text.slice(slash + 1);
    if (network === null || !CIDR_LENGTH.test(lengthText)) return null;
    const length = Number(lengthText);
    return ((network & ~maskOf(length)) >>> 0) === 0 ? { network, length } : null;
  }
  const parts = (text.endsWith('.') ? text.slice(0, -1) : text).split('.');
  if (parts.length > 3 || !parts.every((part) => OCTET.test(part) && Number(part) <= 255)) return null;
  const network = parts.reduce((value, part) => value * 256 + Number(part), 0) * 2 ** (8 * (4 - parts.length));
  return { network: network >>> 0, length: 8 * parts.length };
}

const overlap = (a, b) => {
  const mask = maskOf(Math.min(a.length, b.length));
  return ((a.network & mask) >>> 0) === ((b.network & mask) >>> 0);
};

/**
 * The four warnings the agent reports (section 9.7 has more, which only a
 * client acts on): a system nobody can drive, a wake block nothing can place,
 * a helper at another site, and two sites whose prefixes overlap. Only read
 * for a document with no error, so every shape below is one the checks above
 * allow.
 */
function warningProblems(document) {
  const warnings = [];
  const add = (code, path, message) => warnings.push({ path, code, message });
  const machines = arrayOf(document.machines);
  const sites = arrayOf(document.sites);
  const siteIds = new Set(sites.map((site) => site.id));
  const byId = new Map();
  for (const machine of machines) if (!byId.has(machine.id)) byId.set(machine.id, machine);

  machines.forEach((machine, index) => {
    const path = `machines[${index}]`;
    arrayOf(machine.systems).forEach((system, k) => {
      if (!Array.isArray(system.agent) || system.agent.length === 0) {
        add('system-no-agent', `${path}.systems[${k}]`, `${machine.id}/${system.id} has no agent command, so no client drives it`);
      }
    });
    if (!isPlainObject(machine.wake)) return;
    const helpers = helpersOf(machine);
    const namesSite = typeof machine.site === 'string' && siteIds.has(machine.site);
    if (helpers.length === 0 && !namesSite && parseLanPrefix(machine.wake.lanPrefix) === null) {
      add('wake-unplaced', `${path}.wake`, `${machine.id} has a wake block but names no site, no valid lanPrefix and no helper, so no client can tell when it is on its network and nothing can wake it`);
    }
    for (const helper of helpers) {
      const target = byId.get(helper.machine);
      if (target && typeof machine.site === 'string' && typeof target.site === 'string' && target.site !== machine.site) {
        add('helper-other-site', `${path}.wake`, `${machine.id}'s helper ${helper.machine} is at site ${target.site}, not ${machine.site}; this only works if its action reaches the other network some other way`);
      }
    }
  });

  // Overlapping prefixes on two sites are legal, home routers ship the same
  // range, but matching one then says nothing about which site a device is on.
  const parsed = sites.map((site) => arrayOf(site.lanPrefixes).map((text) => ({ text, prefix: parseLanPrefix(text) })).filter((entry) => entry.prefix !== null));
  for (let j = 1; j < sites.length; j += 1) {
    for (let i = 0; i < j; i += 1) {
      const pair = parsed[i].flatMap((a) => parsed[j].map((b) => [a, b])).find(([a, b]) => overlap(a.prefix, b.prefix));
      if (pair === undefined) continue;
      add('shared-prefix', `sites[${j}].lanPrefixes`, `sites ${sites[i].id} and ${sites[j].id} share the addresses of ${pair[0].text} and ${pair[1].text}, so matching one does not say which site a device is on`);
    }
  }
  return warnings;
}

// ---------------------------------------------------------------------------
// The check
// ---------------------------------------------------------------------------

/** Each problem once, in the order found: two rules can describe one fault from two ends. */
function unique(problems) {
  const seen = new Set();
  return problems.filter((problem) => {
    const key = `${problem.code}\u0000${problem.path}\u0000${problem.message}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

/**
 * The agent's part of section 9.7 for a parsed document:
 * `{ errors: [{ path, code, message }], warnings: [...] }`. The repeated-member
 * rule needs the text, so it is findRepeatedMember's. Warnings are only
 * reported for a document with no error, as a client reports them.
 */
export function validateSetupDocument(document) {
  const errors = unique([...shapeProblems(document), ...relationProblems(document)]);
  const warnings = errors.length > 0 ? [] : unique(warningProblems(document));
  return { errors, warnings };
}
