// Which setup documents a client accepts: the reference of v3.md section 9.7.
//
// Before this file there were five answers to that question. Android refused
// documents the schema allows (a machine with no systems yet, an endpoint with
// no id), the desktop counted id-less endpoints as duplicates, the Mac checked
// setup ids with one grammar and the other two with another, contract/validate.mjs
// had a fourth set of rules, and the agent stored whatever parsed. A setup one
// device wrote could be refused by the next, and a client that refuses its
// peer's document stops replicating without anybody noticing (C3).
//
// So acceptance is one function, ported by every client and held to
// contract/vectors/setup-acceptance.json: a document is refused for a broken
// schema constraint or one of the ten relationship rules below, and for
// nothing else. Everything a client cannot use is a warning with a code, so it
// can say why a machine is not driven without refusing the setup it is in.
//
// The schema constraints are written out here by hand rather than run through a
// JSON Schema validator. That is what a port does, since none of the clients
// carries one, and it lets contract/validate.mjs hold this file to
// schemas/controller-document.schema.json case by case: any drift between the two
// fails there. Only the string grammars are read from the schemas, at import,
// because those are the patterns the agent's constants must equal (C-2) and a
// second copy is one more place for them to drift.

import fs from 'node:fs';
import { canonicalBytes, rejection } from './canonical.mjs';
import { hostForm, isPort, isUser } from './transport.mjs';
import { parseLanPrefix, prefixWarnings } from './wake-plan.mjs';

export const FAMILIES = ['setup-acceptance'];

/**
 * Where a document comes from, which decides what happens to one that fails:
 * `peer` (read from a machine, a kept revision, a merge result) and `local`
 * (typed, pasted, opened) are refused; `applied` (the document this client
 * already runs on) is kept as long as section 12 accepts its bytes.
 */
export const INTAKES = Object.freeze(['peer', 'local', 'applied']);

export const ERROR_CODES = Object.freeze([
  'duplicate-member', 'schema', 'duplicate-machine', 'duplicate-site', 'duplicate-system', 'duplicate-endpoint',
  'unknown-site', 'helper-self', 'helper-unknown', 'helper-alias', 'helper-cycle',
]);

export const WARNING_CODES = Object.freeze([
  'no-machines', 'machine-no-systems', 'machine-no-endpoints', 'system-no-agent', 'endpoint-no-id',
  'endpoint-no-user', 'endpoint-unknown-system', 'endpoint-undialable', 'wake-no-mac', 'wake-unplaced',
  'wake-broadcast-defaulted', 'helper-alias-unused', 'helper-other-site', 'helper-not-always-on',
  'shared-prefix', 'prefix-invalid', 'prefix-without-dot', 'broadcast-invalid',
]);

/**
 * The setup id local intake gives a document that has none. A client mints a
 * fresh `setup-<uuid>` instead; any valid setup id gives the same verdict, so
 * the reference uses one fixed value and its output never depends on a random
 * source.
 */
export const ASSIGNED_SETUP_ID = 'setup-00000000-0000-4000-8000-000000000000';

// ---------------------------------------------------------------------------
// The string grammars, from the schemas.
// ---------------------------------------------------------------------------

const read = (name) => JSON.parse(fs.readFileSync(new URL(`../schemas/${name}`, import.meta.url), 'utf8'));
const COMMON = read('common.schema.json');
const DOCUMENT = read('controller-document.schema.json');

/**
 * A string rule from a schema definition: its pattern and length limits and
 * nothing else. A definition that grows another constraint stops this file
 * from loading, because a constraint read nowhere is one the vectors would
 * silently stop testing.
 */
function stringRule(definition, where) {
  const unread = Object.keys(definition).filter((key) => !['type', 'pattern', 'minLength', 'maxLength', 'description'].includes(key));
  if (definition.type !== 'string' || typeof definition.pattern !== 'string' || unread.length > 0) {
    throw new Error(`${where} is not a plain string pattern, so document-rules.mjs cannot apply it`);
  }
  const pattern = new RegExp(definition.pattern, 'u');
  const length = (value) => [...value].length;
  // The matching rule every implementation shares: the whole input, ASCII
  // classes, and a line feed or carriage return refused before any pattern runs.
  return (value) => typeof value === 'string' && !/[\n\r]/.test(value)
    && length(value) >= (definition.minLength ?? 0) && length(value) <= (definition.maxLength ?? Infinity)
    && pattern.test(value);
}

const isToken = stringRule(DOCUMENT.$defs.token, 'controller-document.schema.json#/$defs/token');
const isSetupId = stringRule(COMMON.$defs.setupId, 'common.schema.json#/$defs/setupId');
const isConfiguredId = stringRule(COMMON.$defs.configuredId, 'common.schema.json#/$defs/configuredId');
const isSha256 = stringRule(COMMON.$defs.sha256, 'common.schema.json#/$defs/sha256');
const isHostGuard = stringRule(COMMON.$defs.sshHostGuard, 'common.schema.json#/$defs/sshHostGuard');
const isUserGuard = stringRule(COMMON.$defs.sshUser, 'common.schema.json#/$defs/sshUser');
const isArgItemText = stringRule(COMMON.$defs.agentArgItem, 'common.schema.json#/$defs/agentArgItem');
const isRepo = stringRule(COMMON.$defs.githubRepo, 'common.schema.json#/$defs/githubRepo');
const isMac = stringRule(DOCUMENT.$defs.wake.properties.mac, 'controller-document.schema.json#/$defs/wake/properties/mac');

/** The argv-item guard, including the limit a schema cannot count: 4096 UTF-8 bytes. */
const ARG_ITEM_BYTES = 4096;

const SOURCES = ['mac', 'desktop', 'phone', 'cli', 'legacy'];
const PLATFORMS = ['linux', 'windows', 'mac'];
const SHELLS = ['posix', 'powershell', 'cmd'];
const KINDS = ['lan', 'remote'];

// ---------------------------------------------------------------------------
// Small helpers.
// ---------------------------------------------------------------------------

const isPlainObject = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);
const hasOwn = (object, key) => Object.hasOwn(object, key);
const isText = (value) => typeof value === 'string' && value.length > 0;
const isPortNumber = (value) => Number.isInteger(value) && value >= 1 && value <= 65535;
const arrayOf = (value) => (Array.isArray(value) ? value : []);
const join = (base, key) => (base === '' ? key : `${base}.${key}`);
const show = (value) => JSON.stringify(value) ?? String(value);

/** Each problem once, in the order found. Two rules can describe the same fault from two ends. */
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
 * Every wake helper of a machine: the entries of `wake.helpers` and the alias
 * `wake.helper`. Both are checked, because clients disagree about which one
 * counts when `helpers` is empty, and a helper some client would run must name
 * a machine that exists and must not lead back to the machine it wakes.
 */
function helpersOf(machine) {
  const wake = isPlainObject(machine?.wake) ? machine.wake : null;
  if (wake === null) return [];
  const list = [...arrayOf(wake.helpers)];
  if (wake.helper !== undefined) list.push(wake.helper);
  return list.filter((helper) => isPlainObject(helper) && typeof helper.machine === 'string');
}

// ---------------------------------------------------------------------------
// Duplicate member names, found in the text.
// ---------------------------------------------------------------------------

/**
 * The first object member whose name the same object already used, as
 * `{ path }`, or null. Implementations' JSON parsers disagree about which copy
 * of a repeated name counts (the first, the last, or an error), so a document
 * that repeats one means different things on different devices, and nothing
 * after this check can be trusted. `text` must be valid JSON; names are
 * compared after decoding, so `"a"` and `"a"` are the same name.
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
// The schema, written out (code `schema`).
// ---------------------------------------------------------------------------

function schemaProblems(document) {
  const problems = [];
  const add = (path, message) => problems.push({ code: 'schema', path, message });

  /** An optional string key held to `rule`. */
  const string = (object, key, path, rule, what) => {
    if (hasOwn(object, key) && !rule(object[key])) add(join(path, key), `${show(object[key])} is not ${what}`);
  };
  /** An optional array key: its type and limits, then `each` on every item. */
  const list = (object, key, path, { min = 0, max }, each) => {
    if (!hasOwn(object, key)) return;
    const value = object[key];
    const at = join(path, key);
    if (!Array.isArray(value)) {
      add(at, `${key} is not an array`);
      return;
    }
    if (value.length < min) add(at, `${key} has ${value.length} entries and needs at least ${min}`);
    if (value.length > max) add(at, `${key} has ${value.length} entries; at most ${max} are allowed`);
    value.forEach((item, index) => each(item, `${at}[${index}]`));
  };
  const required = (object, keys, path) => {
    for (const key of keys) if (!hasOwn(object, key)) add(join(path, key), `${key} is required`);
  };
  const object = (value, path, what) => {
    if (isPlainObject(value)) return true;
    add(path, `${what} is not an object`);
    return false;
  };
  const port = (object, key, path) => {
    if (hasOwn(object, key) && !isPortNumber(object[key])) add(join(path, key), `${show(object[key])} is not a port: an integer from 1 to 65535`);
  };
  const nonEmptyString = (value, path, what) => {
    if (!isText(value)) add(path, `${what} is not a non-empty string`);
  };

  if (!isPlainObject(document)) {
    add('', 'the document is not a JSON object');
    return problems;
  }

  if (!hasOwn(document, 'version')) add('version', 'version is required, and it is 1');
  else if (document.version !== 1) add('version', `version is ${show(document.version)}; it stays 1`);
  if (!hasOwn(document, 'machines')) add('machines', 'machines is required');

  if (hasOwn(document, 'controller') && object(document.controller, 'controller', 'controller')) {
    const block = document.controller;
    required(block, ['id', 'revision'], 'controller');
    string(block, 'id', 'controller', isSetupId, 'a setup id (^[A-Za-z0-9][A-Za-z0-9._:-]{0,63}$)');
    string(block, 'name', 'controller', isText, 'a non-empty string');
    if (hasOwn(block, 'revision') && !(Number.isInteger(block.revision) && block.revision >= 0)) {
      add('controller.revision', `${show(block.revision)} is not a revision: a whole number from 0`);
    }
    if (hasOwn(block, 'updatedAt') && typeof block.updatedAt !== 'string') add('controller.updatedAt', 'updatedAt is not a string');
    if (hasOwn(block, 'source') && !SOURCES.includes(block.source)) {
      add('controller.source', `${show(block.source)} is not one of ${SOURCES.join(', ')}; the device kind goes in device`);
    }
    string(block, 'device', 'controller', isText, 'a non-empty string');
    list(block, 'lineage', 'controller', { max: 32 }, (hash, path) => {
      if (!isSha256(hash)) add(path, `${show(hash)} is not a lowercase sha256`);
    });
    if (Array.isArray(block.lineage)) {
      const seen = new Set();
      block.lineage.forEach((hash, index) => {
        const key = JSON.stringify(hash);
        if (seen.has(key)) add(`controller.lineage[${index}]`, `${show(hash)} is listed twice; a document cannot descend from one ancestor twice`);
        seen.add(key);
      });
    }
  }

  list(document, 'sites', '', { max: 20 }, (site, path) => {
    if (!object(site, path, 'a site')) return;
    required(site, ['id'], path);
    string(site, 'id', path, isToken, 'a token');
    string(site, 'name', path, isText, 'a non-empty string');
    list(site, 'lanPrefixes', path, { min: 1, max: 10 }, (prefix, at) => nonEmptyString(prefix, at, 'a prefix'));
    list(site, 'broadcast', path, { max: 10 }, (address, at) => nonEmptyString(address, at, 'a broadcast address'));
  });

  const helper = (value, path) => {
    if (!object(value, path, 'a helper')) return;
    required(value, ['machine', 'action'], path);
    string(value, 'machine', path, isToken, 'a token');
    string(value, 'action', path, isConfiguredId, 'a configured id (^[A-Za-z0-9][A-Za-z0-9._-]*$)');
  };

  list(document, 'machines', '', { max: 50 }, (machine, path) => {
    if (!object(machine, path, 'a machine')) return;
    required(machine, ['id'], path);
    string(machine, 'id', path, isToken, 'a token');
    string(machine, 'name', path, isText, 'a non-empty string');
    string(machine, 'site', path, isToken, 'a token');
    if (hasOwn(machine, 'alwaysOn') && typeof machine.alwaysOn !== 'boolean') add(join(path, 'alwaysOn'), 'alwaysOn is not a boolean');

    list(machine, 'endpoints', path, { max: 20 }, (endpoint, at) => {
      if (!object(endpoint, at, 'an endpoint')) return;
      required(endpoint, ['host'], at);
      string(endpoint, 'id', at, isToken, 'a token');
      if (hasOwn(endpoint, 'kind') && !KINDS.includes(endpoint.kind)) add(join(at, 'kind'), `${show(endpoint.kind)} is not lan or remote`);
      string(endpoint, 'host', at, isHostGuard, 'a host name or address');
      port(endpoint, 'port', at);
      string(endpoint, 'user', at, isUserGuard, 'a login name');
      string(endpoint, 'system', at, isToken, 'a token');
      string(endpoint, 'label', at, isText, 'a non-empty string');
    });

    if (hasOwn(machine, 'wake') && object(machine.wake, join(path, 'wake'), 'wake')) {
      const wake = machine.wake;
      const at = join(path, 'wake');
      string(wake, 'mac', at, isMac, 'a MAC address');
      list(wake, 'broadcast', at, { max: 10 }, (address, where) => nonEmptyString(address, where, 'a broadcast address'));
      list(wake, 'ports', at, { max: 5 }, (value, where) => {
        if (!isPortNumber(value)) add(where, `${show(value)} is not a port: an integer from 1 to 65535`);
      });
      if (hasOwn(wake, 'probe') && object(wake.probe, join(at, 'probe'), 'wake.probe')) {
        required(wake.probe, ['host'], join(at, 'probe'));
        string(wake.probe, 'host', join(at, 'probe'), isHostGuard, 'a host name or address');
        port(wake.probe, 'port', join(at, 'probe'));
      }
      string(wake, 'lanPrefix', at, isText, 'a non-empty string');
      if (hasOwn(wake, 'helper')) helper(wake.helper, join(at, 'helper'));
      list(wake, 'helpers', at, { max: 5 }, helper);
    }

    list(machine, 'systems', path, { max: 10 }, (system, at) => {
      if (!object(system, at, 'a system')) return;
      required(system, ['id'], at);
      string(system, 'id', at, isToken, 'a token');
      string(system, 'name', at, isText, 'a non-empty string');
      if (hasOwn(system, 'platform') && !PLATFORMS.includes(system.platform)) add(join(at, 'platform'), `${show(system.platform)} is not one of ${PLATFORMS.join(', ')}`);
      if (hasOwn(system, 'shell') && !SHELLS.includes(system.shell)) add(join(at, 'shell'), `${show(system.shell)} is not one of ${SHELLS.join(', ')}`);
      if (hasOwn(system, 'restricted') && typeof system.restricted !== 'boolean') add(join(at, 'restricted'), 'restricted is not a boolean');
      list(system, 'agent', at, { max: 20 }, (item, where) => {
        if (!isArgItemText(item)) {
          add(where, `${show(item)} is not an argv item: not empty, and no control character`);
        } else if (Buffer.byteLength(item, 'utf8') > ARG_ITEM_BYTES) {
          add(where, `an agent argv item is ${Buffer.byteLength(item, 'utf8')} UTF-8 bytes, over the ${ARG_ITEM_BYTES} every dispatcher and client allows`);
        }
      });
    });

    if (hasOwn(machine, 'ssh') && object(machine.ssh, join(path, 'ssh'), 'ssh')) {
      const at = join(path, 'ssh');
      string(machine.ssh, 'host', at, isHostGuard, 'a host name or address');
      string(machine.ssh, 'user', at, isUserGuard, 'a login name');
      port(machine.ssh, 'port', at);
      string(machine.ssh, 'identityFile', at, isText, 'a non-empty string');
    }
  });

  if (hasOwn(document, 'appUpdates') && object(document.appUpdates, 'appUpdates', 'appUpdates')) {
    required(document.appUpdates, ['githubRepo'], 'appUpdates');
    string(document.appUpdates, 'githubRepo', 'appUpdates', isRepo, 'owner/name as GitHub spells them');
  }
  for (const key of ['local', 'ssh']) {
    if (hasOwn(document, key) && document[key] !== null && !isPlainObject(document[key])) add(key, `${key} is neither an object nor null`);
  }
  return problems;
}

// ---------------------------------------------------------------------------
// The relationship rules (errors).
// ---------------------------------------------------------------------------

function ruleProblems(document) {
  const problems = [];
  const add = (code, path, message) => problems.push({ code, path, message });
  if (!isPlainObject(document)) return problems;
  const machines = arrayOf(document.machines);
  const sites = arrayOf(document.sites);

  const machineIds = new Set();
  machines.forEach((machine, index) => {
    if (!isPlainObject(machine) || typeof machine.id !== 'string') return;
    if (machineIds.has(machine.id)) add('duplicate-machine', `machines[${index}].id`, `two machines share the id "${machine.id}"`);
    machineIds.add(machine.id);

    // Endpoints without an id are never duplicates: they cannot be remembered
    // as a route, and that is a warning, not a reason to refuse the setup.
    const endpointIds = new Set();
    arrayOf(machine.endpoints).forEach((endpoint, k) => {
      if (!isPlainObject(endpoint) || typeof endpoint.id !== 'string') return;
      if (endpointIds.has(endpoint.id)) {
        add('duplicate-endpoint', `machines[${index}].endpoints[${k}].id`, `${machine.id}: two endpoints share the id "${endpoint.id}", so a remembered route is ambiguous`);
      }
      endpointIds.add(endpoint.id);
    });

    const systemIds = new Set();
    arrayOf(machine.systems).forEach((system, k) => {
      if (!isPlainObject(system) || typeof system.id !== 'string') return;
      if (systemIds.has(system.id)) add('duplicate-system', `machines[${index}].systems[${k}].id`, `${machine.id}: two systems share the id "${system.id}"`);
      systemIds.add(system.id);
    });
  });

  const siteIds = new Set();
  sites.forEach((site, index) => {
    if (!isPlainObject(site) || typeof site.id !== 'string') return;
    if (siteIds.has(site.id)) add('duplicate-site', `sites[${index}].id`, `two sites share the id "${site.id}"`);
    siteIds.add(site.id);
  });

  machines.forEach((machine, index) => {
    if (!isPlainObject(machine)) return;
    // A setup without sites places nobody, so a site name is only checked
    // against a list that exists.
    if (typeof machine.site === 'string' && sites.length > 0 && !siteIds.has(machine.site)) {
      add('unknown-site', `machines[${index}].site`, `${machine.id}: site "${machine.site}" is not in sites[], so this machine drops out of every on-site decision`);
    }
    for (const helper of helpersOf(machine)) {
      if (helper.machine === machine.id) {
        add('helper-self', `machines[${index}].wake`, `${machine.id}: is its own wake helper, which asks a machine that is off to wake itself`);
      } else if (!machineIds.has(helper.machine)) {
        add('helper-unknown', `machines[${index}].wake`, `${machine.id}: wake helper "${helper.machine}" is not a machine in this document`);
      }
    }
    // The singular key is what a 1.2 client reads. If the two disagree, an old
    // and a new client try different machines from the same document.
    const wake = isPlainObject(machine.wake) ? machine.wake : null;
    if (wake && isPlainObject(wake.helper) && Array.isArray(wake.helpers) && isPlainObject(wake.helpers[0])) {
      const first = wake.helpers[0];
      if (first.machine !== wake.helper.machine || first.action !== wake.helper.action) {
        add('helper-alias', `machines[${index}].wake.helper`, `${machine.id}: wake.helper is ${wake.helper.machine}/${wake.helper.action} and wake.helpers[0] is ${first.machine}/${first.action}; the singular key is the compatibility alias and must repeat the first entry`);
      }
    }
  });

  // A ring of helpers is the failure that looks fine one machine at a time:
  // every machine in it waits for another to be awake first.
  const byId = new Map();
  for (const machine of machines) if (isPlainObject(machine) && typeof machine.id === 'string' && !byId.has(machine.id)) byId.set(machine.id, machine);
  machines.forEach((machine, index) => {
    if (!isPlainObject(machine) || typeof machine.id !== 'string') return;
    const seen = new Set([machine.id]);
    const walk = (id, trail) => {
      for (const helper of helpersOf(byId.get(id))) {
        if (seen.has(helper.machine)) {
          if (helper.machine === machine.id && trail.length > 0) {
            add('helper-cycle', `machines[${index}].wake`, `wake helper cycle: ${[...trail, id, helper.machine].join(' needs ')}; every machine in it waits for another to be awake first`);
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
// What a client cannot use (warnings). Only read for a document with no error,
// so every shape below is one the schema allows.
// ---------------------------------------------------------------------------

function warningProblems(document) {
  const warnings = [];
  const add = (code, path, message) => warnings.push({ code, path, message });
  const machines = arrayOf(document.machines);
  const siteIds = new Set(arrayOf(document.sites).map((site) => site.id));
  const byId = new Map();
  for (const machine of machines) if (!byId.has(machine.id)) byId.set(machine.id, machine);

  if (machines.length === 0) add('no-machines', 'machines', 'the setup has no machines yet, so there is nothing to drive');

  /** The dial grammar of section 15.6, on a value the guard already passed. */
  const dialable = (holder, path, owner) => {
    if (typeof holder.host === 'string' && hostForm(holder.host) === null) {
      add('endpoint-undialable', join(path, 'host'), `${owner}: ${show(holder.host)} passes the document's host guard but is not a name or address a client dials, scans or pins; this entry is skipped`);
    }
    if (typeof holder.user === 'string' && !isUser(holder.user)) {
      add('endpoint-undialable', join(path, 'user'), `${owner}: ${show(holder.user)} is not a login name a client dials with; this entry is skipped`);
    }
    if (hasOwn(holder, 'port') && !isPort(holder.port)) {
      add('endpoint-undialable', join(path, 'port'), `${owner}: ${show(holder.port)} is not a port a client dials; this entry is skipped`);
    }
  };

  machines.forEach((machine, index) => {
    const path = `machines[${index}]`;
    const systems = arrayOf(machine.systems);
    const endpoints = arrayOf(machine.endpoints);
    const systemIds = new Set(systems.map((system) => system.id));
    const legacyHost = isPlainObject(machine.ssh) && typeof machine.ssh.host === 'string';

    if (systems.length === 0) add('machine-no-systems', path, `${machine.id} has no systems, so nothing on it can be driven yet`);
    if (endpoints.length === 0 && !legacyHost) add('machine-no-endpoints', path, `${machine.id} has no endpoints and no ssh host, so no client can reach it`);

    systems.forEach((system, k) => {
      if (!Array.isArray(system.agent) || system.agent.length === 0) {
        add('system-no-agent', `${path}.systems[${k}]`, `${machine.id}/${system.id} has no agent command, so it is not driven`);
      }
    });

    endpoints.forEach((endpoint, k) => {
      const at = `${path}.endpoints[${k}]`;
      if (!hasOwn(endpoint, 'id')) add('endpoint-no-id', at, `${machine.id}: an endpoint has no id, so it cannot be remembered as a route and merges with the other id-less endpoints`);
      if (!hasOwn(endpoint, 'user')) add('endpoint-no-user', at, `${machine.id}: an endpoint has no user, so ssh logs in as whatever the client's own configuration says`);
      if (typeof endpoint.system === 'string' && !systemIds.has(endpoint.system)) {
        add('endpoint-unknown-system', join(at, 'system'), `${machine.id}: the endpoint hint names system "${endpoint.system}", which this machine does not have; the hint is ignored`);
      }
      dialable(endpoint, at, machine.id);
    });
    if (isPlainObject(machine.ssh)) dialable(machine.ssh, `${path}.ssh`, machine.id);

    if (!isPlainObject(machine.wake)) return;
    const { wake } = machine;
    const helpers = helpersOf(machine);
    if (!hasOwn(wake, 'mac')) add('wake-no-mac', `${path}.wake`, `${machine.id}: the wake block has no mac, so no packet can wake it directly; helpers still work`);
    const namesSite = typeof machine.site === 'string' && siteIds.has(machine.site);
    if (helpers.length === 0 && !namesSite && parseLanPrefix(wake.lanPrefix) === null) {
      add('wake-unplaced', `${path}.wake`, `${machine.id}: has a wake block but names no site, has no valid lanPrefix and no helper, so no client can tell when it is on its network and nothing can wake it`);
    }
    if (isPlainObject(wake.helper) && Array.isArray(wake.helpers) && wake.helpers.length === 0) {
      add('helper-alias-unused', `${path}.wake.helper`, `${machine.id}: wake.helper is set beside an empty wake.helpers, which some clients read as no helper at all and others as this one; write it into helpers as well`);
    }
    for (const helper of helpers) {
      const target = byId.get(helper.machine);
      if (target === undefined) continue;
      if (typeof machine.site === 'string' && typeof target.site === 'string' && target.site !== machine.site) {
        add('helper-other-site', `${path}.wake`, `${machine.id}: helper ${helper.machine} is at site ${target.site}, not ${machine.site}; this only works if the action reaches the target's network another way, such as a router or a VPN`);
      }
      if (target.alwaysOn !== true) {
        add('helper-not-always-on', `${path}.wake`, `${machine.id}: helper ${helper.machine} is not marked alwaysOn, so waking ${machine.id} may need ${helper.machine} to be woken first`);
      }
    }
  });

  return [...warnings, ...prefixWarnings(document)];
}

// ---------------------------------------------------------------------------
// Acceptance.
// ---------------------------------------------------------------------------

/**
 * The errors and warnings of a parsed document under peer intake:
 * `{ errors: [{code, path, message}], warnings: [...] }`. A schema problem and a
 * relationship problem are both errors; warnings are only reported for a
 * document with none, because a client shows the errors of a broken document
 * and the warnings of a working one. The duplicate-member rule needs the text,
 * so it is checkDocument()'s.
 */
export function checkControllerDocument(document) {
  const errors = unique([...schemaProblems(document), ...ruleProblems(document)]);
  const warnings = errors.length > 0 ? [] : unique(warningProblems(document));
  return { errors, warnings };
}

/**
 * Whether local intake gives this document an identity before it is checked:
 * when it has no `controller` block, or a block without an id. Section 9.5 says
 * what it gets: a fresh setup id, revision 1 and an empty lineage, keeping the
 * block's other keys.
 */
export function needsIdentity(document) {
  if (!hasOwn(document, 'controller')) return true;
  return isPlainObject(document.controller) && (document.controller.id === undefined || document.controller.id === null);
}

function withIdentity(document) {
  const block = isPlainObject(document.controller) ? document.controller : {};
  return { ...document, controller: { ...block, id: ASSIGNED_SETUP_ID, revision: 1, lineage: [] } };
}

/**
 * Section 9.7 for a document as text (a string or bytes) under one intake:
 * `{ valid, errors, warnings, assignsIdentity, keep }`, the lists holding
 * `{code, path, message}`.
 *
 * The order is fixed. Section 12 comes first: a text it refuses is no document
 * at all, so it breaks the schema before any rule can be read, under every
 * intake, and even applied intake does not keep it. Then a repeated member
 * name, on the canonical text, which when it applies is the only thing
 * reported. Then local intake's identity, then the schema and the rules.
 * `keep` is whether the client goes on using the document: a refused peer or
 * local document is not used, an applied one is used with its errors shown.
 */
export function checkDocument(text, { intake = 'peer' } = {}) {
  if (!INTAKES.includes(intake)) throw new Error(`"${intake}" is not an intake; it is one of ${INTAKES.join(', ')}`);
  const input = Buffer.isBuffer(text) ? text : Buffer.from(text, 'utf8');
  const refused = rejection(input);
  if (refused !== null) {
    return {
      valid: false,
      errors: [{ code: 'schema', path: '', message: `section 12 refuses the text (${refused}), so there is no document to check` }],
      warnings: [],
      assignsIdentity: false,
      keep: false,
    };
  }
  const canonical = canonicalBytes(input).toString('utf8');
  const repeated = findRepeatedMember(canonical);
  if (repeated !== null) {
    return {
      valid: false,
      errors: [{ code: 'duplicate-member', path: repeated.path, message: `${repeated.path} is written twice in one object, and parsers disagree about which copy counts` }],
      warnings: [],
      assignsIdentity: false,
      keep: intake === 'applied',
    };
  }
  let document = JSON.parse(canonical);
  const assignsIdentity = intake === 'local' && needsIdentity(document);
  if (assignsIdentity) document = withIdentity(document);
  const { errors, warnings } = checkControllerDocument(document);
  const valid = errors.length === 0;
  return { valid, errors, warnings, assignsIdentity, keep: intake === 'applied' ? true : valid };
}

// ---------------------------------------------------------------------------
// The reference interface of the `setup-acceptance` vector family.
// ---------------------------------------------------------------------------

const codes = (problems) => [...new Set(problems.map((problem) => problem.code))].sort();

export function run(group, input) {
  if (group !== 'documents') throw new Error(`the setup-acceptance family has no group "${group}"`);
  const result = checkDocument(input.text, { intake: input.intake });
  return {
    valid: result.valid,
    errors: codes(result.errors),
    warnings: codes(result.warnings),
    assignsIdentity: result.assignsIdentity,
    keep: result.keep,
  };
}
