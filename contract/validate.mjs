#!/usr/bin/env node
// Checks the shared contract: node contract/validate.mjs
//
// Three passes, and each one exists because the other two cannot see the fault
// it catches.
//
//   1. The schemas are linted against each other. A $ref that resolves to
//      nothing and a keyword with a typo in it both make a schema that accepts
//      everything, which is worse than no schema at all.
//   2. Every fixture is validated against the schema its index entry names, and
//      the index and the directory are checked against each other so a fixture
//      cannot be added without being announced or removed while a client still
//      loads it.
//   3. The things a schema structurally cannot say are asserted in code: that a
//      status' 2.x compatibility keys really do mirror its first service, that
//      an operation's timestamps run forwards, that the doctor's counts match
//      its checks, and that the canonical hash vectors reproduce byte for byte
//      under an implementation written separately from the one that made them.
//
// No dependencies, deliberately: this runs in CI on three platforms and inside
// four different toolchains, and a lockfile in contract/ would be one more thing
// to keep in step with nothing.
//
// Exit code 0 when everything holds, 1 otherwise. Failures are printed in full,
// not summarised: a contract check that says "3 errors" and stops is a check
// nobody runs twice.

import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const SCHEMA_DIR = path.join(HERE, 'schemas');
const FIXTURE_DIR = path.join(HERE, 'fixtures');
const INDEX_FILE = path.join(FIXTURE_DIR, 'index.json');
const VECTOR_FILE = path.join(HERE, 'hash-vectors.json');

const failures = [];
const warnings = [];
let checks = 0;

function fail(where, message) {
  failures.push(`${where}: ${message}`);
}

function pass() {
  checks += 1;
}

function readJson(file) {
  return JSON.parse(fs.readFileSync(file, 'utf8'));
}

// ---------------------------------------------------------------------------
// A JSON Schema validator, restricted to the keywords these schemas use.
//
// Restricted on purpose. A general implementation would be a dependency, and
// the alternative to a dependency is not "a smaller general implementation" but
// "the exact subset, with every keyword it does not know treated as an error".
// That last part is what makes the schema lint below possible.
// ---------------------------------------------------------------------------

const KNOWN_KEYWORDS = new Set([
  '$schema', '$id', '$ref', '$defs', 'title', 'description', 'deprecated', 'examples',
  'type', 'const', 'enum',
  'properties', 'patternProperties', 'required', 'additionalProperties', 'unevaluatedProperties',
  'propertyNames', 'minProperties', 'maxProperties',
  'items', 'prefixItems', 'minItems', 'maxItems', 'uniqueItems',
  'minLength', 'maxLength', 'pattern',
  'minimum', 'maximum', 'exclusiveMinimum', 'exclusiveMaximum', 'multipleOf',
  'allOf', 'anyOf', 'oneOf', 'not', 'if', 'then', 'else',
]);

const TYPES = new Set(['object', 'array', 'string', 'number', 'integer', 'boolean', 'null']);

const registry = new Map();

function loadSchemas() {
  for (const name of fs.readdirSync(SCHEMA_DIR).sort()) {
    if (!name.endsWith('.schema.json')) continue;
    registry.set(name, readJson(path.join(SCHEMA_DIR, name)));
  }
}

/** Resolve a $ref of the forms "file.schema.json", "#/$defs/x" and "file.schema.json#/$defs/x". */
function resolveRef(ref, fromFile) {
  const [file, pointer] = ref.split('#');
  const target = file === '' ? fromFile : file;
  const root = registry.get(target);
  if (root === undefined) return { error: `no schema named "${target}"` };
  if (pointer === undefined || pointer === '' || pointer === '/') return { schema: root, file: target };

  let node = root;
  for (const rawSegment of pointer.split('/').slice(1)) {
    const segment = rawSegment.replace(/~1/g, '/').replace(/~0/g, '~');
    if (node === null || typeof node !== 'object' || !(segment in node)) {
      return { error: `"${ref}" does not resolve` };
    }
    node = node[segment];
  }
  return { schema: node, file: target };
}

function typeOf(value) {
  if (value === null) return 'null';
  if (Array.isArray(value)) return 'array';
  if (Number.isInteger(value)) return 'integer';
  return typeof value;
}

function matchesType(value, expected) {
  const actual = typeOf(value);
  if (expected === 'number') return actual === 'number' || actual === 'integer';
  return actual === expected;
}

function deepEqual(a, b) {
  return JSON.stringify(a) === JSON.stringify(b);
}

/**
 * Validate `data` against `schema`.
 *
 * Returns the errors found and the set of property names that were evaluated,
 * which is the annotation `unevaluatedProperties` needs. Collecting it is the
 * whole reason this is not fifty lines: it is what lets a reply schema pull the
 * shared envelope in through allOf and still refuse a key nobody declared.
 */
function validate(schema, data, { file, at = '', errors = [] } = {}) {
  const evaluated = new Set();

  if (schema === true) return { errors, evaluated };
  if (schema === false) {
    errors.push(`${at || '<root>'}: no value is allowed here`);
    return { errors, evaluated };
  }

  const push = (message) => errors.push(`${at || '<root>'}: ${message}`);
  const before = errors.length;

  if ('$ref' in schema) {
    const resolved = resolveRef(schema.$ref, file);
    if (resolved.error) push(resolved.error);
    else {
      const sub = validate(resolved.schema, data, { file: resolved.file, at, errors });
      for (const key of sub.evaluated) evaluated.add(key);
    }
  }

  if ('type' in schema && !matchesType(data, schema.type)) {
    push(`expected ${schema.type}, got ${typeOf(data)}`);
  }
  if ('const' in schema && !deepEqual(data, schema.const)) {
    push(`expected ${JSON.stringify(schema.const)}, got ${JSON.stringify(data)}`);
  }
  if ('enum' in schema && !schema.enum.some((option) => deepEqual(option, data))) {
    push(`${JSON.stringify(data)} is not one of ${schema.enum.map((o) => JSON.stringify(o)).join(', ')}`);
  }

  if (typeOf(data) === 'string') {
    if ('minLength' in schema && data.length < schema.minLength) push(`shorter than ${schema.minLength}`);
    if ('maxLength' in schema && data.length > schema.maxLength) push(`longer than ${schema.maxLength}`);
    if ('pattern' in schema && !new RegExp(schema.pattern, 'u').test(data)) {
      push(`${JSON.stringify(data)} does not match ${schema.pattern}`);
    }
  }

  if (typeOf(data) === 'number' || typeOf(data) === 'integer') {
    if ('minimum' in schema && data < schema.minimum) push(`below ${schema.minimum}`);
    if ('maximum' in schema && data > schema.maximum) push(`above ${schema.maximum}`);
    if ('exclusiveMinimum' in schema && data <= schema.exclusiveMinimum) push(`not above ${schema.exclusiveMinimum}`);
    if ('exclusiveMaximum' in schema && data >= schema.exclusiveMaximum) push(`not below ${schema.exclusiveMaximum}`);
    if ('multipleOf' in schema && data % schema.multipleOf !== 0) push(`not a multiple of ${schema.multipleOf}`);
  }

  if (typeOf(data) === 'array') {
    if ('minItems' in schema && data.length < schema.minItems) push(`fewer than ${schema.minItems} items`);
    if ('maxItems' in schema && data.length > schema.maxItems) push(`more than ${schema.maxItems} items`);
    if (schema.uniqueItems === true) {
      const seen = new Set();
      for (const item of data) {
        const key = JSON.stringify(item);
        if (seen.has(key)) push(`repeated item ${key}`);
        seen.add(key);
      }
    }
    if ('prefixItems' in schema) {
      schema.prefixItems.forEach((sub, index) => {
        if (index < data.length) validate(sub, data[index], { file, at: `${at}[${index}]`, errors });
      });
    }
    if ('items' in schema) {
      const start = 'prefixItems' in schema ? schema.prefixItems.length : 0;
      for (let index = start; index < data.length; index += 1) {
        validate(schema.items, data[index], { file, at: `${at}[${index}]`, errors });
      }
    }
  }

  if (typeOf(data) === 'object') {
    for (const key of schema.required ?? []) {
      if (!(key in data)) push(`missing required key "${key}"`);
    }
    if ('minProperties' in schema && Object.keys(data).length < schema.minProperties) {
      push(`fewer than ${schema.minProperties} keys`);
    }
    if ('maxProperties' in schema && Object.keys(data).length > schema.maxProperties) {
      push(`more than ${schema.maxProperties} keys`);
    }
    if ('propertyNames' in schema) {
      for (const key of Object.keys(data)) {
        validate(schema.propertyNames, key, { file, at: `${at}/${key} (name)`, errors });
      }
    }

    const own = new Set();
    for (const [key, sub] of Object.entries(schema.properties ?? {})) {
      if (key in data) {
        validate(sub, data[key], { file, at: `${at}/${key}`, errors });
        own.add(key);
      }
    }
    for (const [pattern, sub] of Object.entries(schema.patternProperties ?? {})) {
      const regex = new RegExp(pattern, 'u');
      for (const key of Object.keys(data)) {
        if (!regex.test(key)) continue;
        validate(sub, data[key], { file, at: `${at}/${key}`, errors });
        own.add(key);
      }
    }
    for (const key of own) evaluated.add(key);

    if ('additionalProperties' in schema) {
      for (const key of Object.keys(data)) {
        if (own.has(key)) continue;
        if (schema.additionalProperties === false) push(`unexpected key "${key}"`);
        else validate(schema.additionalProperties, data[key], { file, at: `${at}/${key}`, errors });
        evaluated.add(key);
      }
    }
  }

  // In-place applicators. Each one that succeeds contributes its evaluated
  // properties, which is what unevaluatedProperties is then measured against.
  for (const sub of schema.allOf ?? []) {
    const result = validate(sub, data, { file, at, errors });
    for (const key of result.evaluated) evaluated.add(key);
  }

  if ('anyOf' in schema) {
    const branches = schema.anyOf.map((sub) => validate(sub, data, { file, at, errors: [] }));
    const matched = branches.filter((branch) => branch.errors.length === 0);
    if (matched.length === 0) {
      push(`matches none of the ${schema.anyOf.length} allowed shapes: ${branches.map((b) => b.errors[0]).join(' | ')}`);
    }
    for (const branch of matched) for (const key of branch.evaluated) evaluated.add(key);
  }

  if ('oneOf' in schema) {
    const branches = schema.oneOf.map((sub) => validate(sub, data, { file, at, errors: [] }));
    const matched = branches.filter((branch) => branch.errors.length === 0);
    if (matched.length !== 1) {
      const detail = matched.length === 0
        ? branches.map((b, i) => `[${i}] ${b.errors[0]}`).join(' | ')
        : 'it matches more than one, which makes the reply ambiguous';
      push(`must match exactly one of the ${schema.oneOf.length} shapes, matched ${matched.length}: ${detail}`);
    }
    for (const branch of matched) for (const key of branch.evaluated) evaluated.add(key);
  }

  if ('not' in schema) {
    const result = validate(schema.not, data, { file, at, errors: [] });
    if (result.errors.length === 0) push('matches a shape that is not allowed here');
  }

  if ('if' in schema) {
    const condition = validate(schema.if, data, { file, at, errors: [] });
    const branch = condition.errors.length === 0 ? schema.then : schema.else;
    if (condition.errors.length === 0) for (const key of condition.evaluated) evaluated.add(key);
    if (branch !== undefined) {
      const result = validate(branch, data, { file, at, errors });
      for (const key of result.evaluated) evaluated.add(key);
    }
  }

  if (schema.unevaluatedProperties === false && typeOf(data) === 'object' && errors.length === before) {
    for (const key of Object.keys(data)) {
      if (!evaluated.has(key)) push(`unexpected key "${key}"`);
    }
  }

  return { errors, evaluated };
}

// ---------------------------------------------------------------------------
// Pass 1: the schemas themselves.
// ---------------------------------------------------------------------------

function lintSchemas() {
  for (const [name, root] of registry) {
    if (root.$id !== name) fail(`schemas/${name}`, `$id is "${root.$id}", which is not the file name`);
    else pass();

    const walk = (node, at) => {
      if (Array.isArray(node)) {
        node.forEach((item, index) => walk(item, `${at}[${index}]`));
        return;
      }
      if (node === null || typeof node !== 'object') return;

      for (const key of Object.keys(node)) {
        // Inside `properties` and `$defs` the keys are names, not keywords.
        if (!KNOWN_KEYWORDS.has(key) && !at.endsWith('/properties') && !at.endsWith('/$defs') && !at.endsWith('/patternProperties')) {
          fail(`schemas/${name}`, `unknown keyword "${key}" at ${at || '<root>'}; a typo here makes the schema accept anything`);
        }
      }

      if (typeof node.$ref === 'string') {
        const resolved = resolveRef(node.$ref, name);
        if (resolved.error) fail(`schemas/${name}`, `${at || '<root>'}: ${resolved.error}`);
        else pass();
      }

      if ('type' in node && typeof node.type === 'string' && !TYPES.has(node.type)) {
        fail(`schemas/${name}`, `${at}: "${node.type}" is not a JSON Schema type`);
      }

      if ('pattern' in node) {
        try {
          new RegExp(node.pattern, 'u');
          pass();
        } catch (error) {
          fail(`schemas/${name}`, `${at}: pattern does not compile: ${error.message}`);
        }
      }

      // A `required` key that no `properties` entry describes is almost always a
      // rename that was only half applied. Conditional branches are exempt: a
      // `then` legitimately requires a key the enclosing schema declares, which
      // is the entire way a rule like "a conflict names what holds the lock" is
      // written.
      const inConditional = /\/(?:if|then|else)$/.test(at) || at.includes('/$defs/') && at.endsWith('Rule');
      if (Array.isArray(node.required) && node.properties && !inConditional) {
        for (const key of node.required) {
          const declaredHere = key in node.properties;
          const declaredNearby = (node.allOf ?? []).some((sub) => sub.$ref || (sub.properties && key in sub.properties));
          if (!declaredHere && !declaredNearby) {
            fail(`schemas/${name}`, `${at}: "${key}" is required but nothing describes it`);
          }
        }
      }

      if ('then' in node && !('if' in node)) fail(`schemas/${name}`, `${at}: "then" without "if" is ignored by every validator`);
      if ('else' in node && !('if' in node)) fail(`schemas/${name}`, `${at}: "else" without "if" is ignored by every validator`);

      for (const [key, child] of Object.entries(node)) {
        if (key === 'const' || key === 'enum' || key === 'examples' || key === 'required') continue;
        walk(child, `${at}/${key}`);
      }
    };

    walk(root, '');
  }
}

// ---------------------------------------------------------------------------
// Pass 2: fixtures against the index and against their schemas.
// ---------------------------------------------------------------------------

function checkFixtures(index) {
  const onDisk = fs
    .readdirSync(FIXTURE_DIR)
    .filter((name) => name.endsWith('.json') && name !== 'index.json')
    .sort();
  const listed = index.fixtures.map((entry) => entry.file);

  for (const name of onDisk) {
    if (!listed.includes(name)) {
      fail('fixtures/index.json', `fixtures/${name} is on disk and not in the index, so no client will load it`);
    }
  }
  for (const name of listed) {
    if (!onDisk.includes(name)) fail('fixtures/index.json', `index names fixtures/${name}, which does not exist`);
  }
  if (new Set(listed).size !== listed.length) fail('fixtures/index.json', 'the same file is listed twice');
  pass();

  const loaded = new Map();
  for (const entry of index.fixtures) {
    const file = path.join(FIXTURE_DIR, entry.file);
    if (!fs.existsSync(file)) continue;

    let data;
    try {
      data = readJson(file);
    } catch (error) {
      fail(`fixtures/${entry.file}`, `does not parse: ${error.message}`);
      continue;
    }
    loaded.set(entry.file, data);

    if (!registry.has(entry.schema)) {
      fail(`fixtures/${entry.file}`, `names schema "${entry.schema}", which does not exist`);
      continue;
    }

    const { errors } = validate(registry.get(entry.schema), data, { file: entry.schema });

    // A document fixture may be here precisely because it is wrong. The graph
    // rules a schema cannot express — a helper cycle, a machine pointing at a
    // site nobody defined — are checked in code, and both sources of problems
    // count toward whether the fixture is refused.
    const graph = entry.kind === 'document' && entry.schema === 'controller-document.schema.json'
      ? checkControllerDocument(data)
      : { errors: [], warnings: [] };
    const problems = [...errors, ...graph.errors];

    if (entry.expect === 'invalid') {
      // The point of an invalid fixture is that something rejects it. One that
      // quietly passes means the rule it was written for does not exist.
      if (problems.length === 0) {
        fail(`fixtures/${entry.file}`, `the index expects this to be refused and nothing refused it; the rule it was written for is missing from ${entry.schema} and from checkControllerDocument`);
      } else {
        pass();
      }
      loaded.delete(entry.file);
      continue;
    }

    if (problems.length > 0) {
      for (const problem of problems) fail(`fixtures/${entry.file} (${entry.schema})`, problem);
      // The checks in pass 3 read fields the schema has just said are missing or
      // the wrong type. Running them anyway turns a clear list of schema errors
      // into a stack trace, so a fixture gets one pass or the other, never both.
      loaded.delete(entry.file);
    } else {
      pass();
      for (const warning of graph.warnings) warnings.push(`fixtures/${entry.file}: ${warning}`);
    }

    // Everything below describes a reply. A document has no envelope, no exit
    // code and no action, and asserting it does would only ever be noise.
    if (entry.kind === 'document') {
      loaded.delete(entry.file);
      continue;
    }

    // The index is documentation the clients read, so it has to agree with the
    // fixture rather than describe what it used to be.
    if (typeof data.ok === 'boolean' && data.ok !== entry.ok) {
      fail(`fixtures/${entry.file}`, `index says ok ${entry.ok}, the fixture says ${data.ok}`);
    }
    const declaredContract = entry.contract === null ? undefined : entry.contract;
    if (data.contract !== declaredContract) {
      fail(`fixtures/${entry.file}`, `index says contract ${JSON.stringify(entry.contract)}, the fixture says ${JSON.stringify(data.contract ?? null)}`);
    }
    if (entry.exitCode !== (entry.ok ? 0 : 1)) {
      const allowed = ['accepted', 'deferred', 'queued', 'noop', 'conflict'];
      if (!allowed.includes(data.action)) {
        fail(`fixtures/${entry.file}`, `exit code ${entry.exitCode} does not follow from ok ${entry.ok}; only ${allowed.join(', ')} may differ`);
      }
    }
  }
  return loaded;
}

// ---------------------------------------------------------------------------
// The shared setup document: the rules that are about relationships.
//
// JSON Schema can say a helper has a machine id and an action id. It cannot say
// that the machine exists, that it is not the machine being woken, or that the
// helpers do not form a ring in which every machine waits for another to be
// awake first. Those are the mistakes an editor actually makes, so they are
// checked here and every client editor has to refuse the same documents.
// ---------------------------------------------------------------------------

function helpersOf(machine) {
  const list = machine.wake?.helpers ?? (machine.wake?.helper ? [machine.wake.helper] : []);
  return list.filter((helper) => helper && typeof helper.machine === 'string');
}

function checkControllerDocument(document) {
  const errors = [];
  const warnings = [];
  const machines = Array.isArray(document.machines) ? document.machines : [];
  const sites = Array.isArray(document.sites) ? document.sites : [];

  const machineIds = new Set();
  for (const machine of machines) {
    if (machineIds.has(machine.id)) errors.push(`two machines share the id "${machine.id}"`);
    machineIds.add(machine.id);

    const endpointIds = new Set();
    for (const endpoint of machine.endpoints ?? []) {
      if (endpoint.id === undefined) continue;
      if (endpointIds.has(endpoint.id)) {
        errors.push(`${machine.id}: two endpoints share the id "${endpoint.id}", so a remembered route is ambiguous`);
      }
      endpointIds.add(endpoint.id);
    }

    const systemIds = new Set();
    for (const system of machine.systems ?? []) {
      if (systemIds.has(system.id)) errors.push(`${machine.id}: two systems share the id "${system.id}"`);
      systemIds.add(system.id);
    }
  }

  const siteIds = new Set();
  for (const site of sites) {
    if (siteIds.has(site.id)) errors.push(`two sites share the id "${site.id}"`);
    siteIds.add(site.id);
  }

  // Identical private subnets on two sites are legal and expected: home routers
  // ship the same default range. It is a warning because it tells the reader
  // that a prefix match cannot decide where this device is, and that the client
  // has to fall back to the user's own answer or treat itself as off-site.
  const byPrefix = new Map();
  for (const site of sites) {
    for (const prefix of site.lanPrefixes ?? []) {
      if (!byPrefix.has(prefix)) byPrefix.set(prefix, []);
      byPrefix.get(prefix).push(site.id);
    }
  }
  for (const [prefix, owners] of byPrefix) {
    if (owners.length > 1) {
      warnings.push(`sites ${owners.join(' and ')} both use the prefix ${prefix}, so matching it does not say which site a device is on; the client must treat its location as unconfirmed unless the user has chosen one`);
    }
  }

  for (const machine of machines) {
    if (machine.site !== undefined && sites.length > 0 && !siteIds.has(machine.site)) {
      errors.push(`${machine.id}: site "${machine.site}" is not in sites[], so this machine drops out of every on-site decision`);
    }

    const helpers = helpersOf(machine);
    for (const helper of helpers) {
      if (helper.machine === machine.id) {
        errors.push(`${machine.id}: is its own wake helper, which asks a machine that is off to wake itself`);
      } else if (!machineIds.has(helper.machine)) {
        errors.push(`${machine.id}: wake helper "${helper.machine}" is not a machine in this document`);
      }
    }

    // The singular key is what a 1.2 client reads. If the two disagree, an old
    // and a new client try different machines from the same document.
    if (machine.wake?.helper && Array.isArray(machine.wake.helpers) && machine.wake.helpers.length > 0) {
      const first = machine.wake.helpers[0];
      if (first.machine !== machine.wake.helper.machine || first.action !== machine.wake.helper.action) {
        errors.push(`${machine.id}: wake.helper is ${machine.wake.helper.machine}/${machine.wake.helper.action} and wake.helpers[0] is ${first.machine}/${first.action}; the singular key is the compatibility alias and must repeat the first entry`);
      }
    }

    if (machine.wake && helpers.length > 0) {
      for (const helper of helpers) {
        const target = machines.find((entry) => entry.id === helper.machine);
        if (target && machine.site !== undefined && target.site !== undefined && target.site !== machine.site) {
          warnings.push(`${machine.id}: helper ${helper.machine} is at site ${target.site}, not ${machine.site}; this only works if the action reaches the target's network another way, such as a router or a VPN`);
        }
        if (target && target.alwaysOn !== true) {
          warnings.push(`${machine.id}: helper ${helper.machine} is not marked alwaysOn, so waking ${machine.id} may need ${helper.machine} to be woken first`);
        }
      }
    }

    if (machine.wake && helpers.length === 0 && machine.site === undefined && machine.wake.lanPrefix === undefined) {
      warnings.push(`${machine.id}: has a wake block but no site, no lanPrefix and no helper, so it can only be woken by a controller that happens to be on its LAN`);
    }
  }

  // A ring of helpers is the failure that looks fine one machine at a time.
  for (const machine of machines) {
    const seen = new Set([machine.id]);
    const walk = (id, trail) => {
      const current = machines.find((entry) => entry.id === id);
      if (!current) return;
      for (const helper of helpersOf(current)) {
        if (seen.has(helper.machine)) {
          if (helper.machine === machine.id && trail.length > 0) {
            errors.push(`wake helper cycle: ${[...trail, id, helper.machine].join(' needs ')}; every machine in it waits for another to be awake first`);
          }
          continue;
        }
        seen.add(helper.machine);
        walk(helper.machine, [...trail, id]);
      }
    };
    walk(machine.id, []);
  }

  return { errors: [...new Set(errors)], warnings: [...new Set(warnings)] };
}

// ---------------------------------------------------------------------------
// Pass 3: the rules a schema cannot express.
// ---------------------------------------------------------------------------

const ISO = /^\d{4}-\d{2}-\d{2}T/;

function order(where, label, earlier, later) {
  if (!earlier || !later) return;
  if (!ISO.test(earlier) || !ISO.test(later)) return;
  if (Date.parse(earlier) > Date.parse(later)) {
    fail(where, `${label}: ${earlier} is after ${later}, so the record runs backwards`);
  } else {
    pass();
  }
}

function checkOperationRecord(where, op) {
  if (!op) return;
  order(where, 'requestedAt to updatedAt', op.requestedAt, op.updatedAt);
  order(where, 'requestedAt to startedAt', op.requestedAt, op.startedAt);
  order(where, 'startedAt to finishedAt', op.startedAt, op.finishedAt);
  order(where, 'requestedAt to expiresAt', op.requestedAt, op.expiresAt);

  for (const entry of op.log ?? []) {
    order(where, 'requestedAt to a log line', op.requestedAt, entry.at);
    order(where, 'a log line to updatedAt', entry.at, op.updatedAt);
  }

  if (op.state === 'finished' && op.result) {
    if (op.result.ok !== (op.result.reasonCode === null)) {
      // A finished operation that went well has nothing to explain; one that did
      // not must say which of the closed reasons it was.
      if (op.result.ok && op.result.reasonCode !== null) {
        fail(where, `result.ok is true and reasonCode is "${op.result.reasonCode}"; a success explains nothing`);
      }
      if (!op.result.ok && op.result.reasonCode === null) {
        fail(where, 'result.ok is false with no reasonCode, which is the "nothing happened and we will not say why" answer contract 3 exists to remove');
      }
    } else {
      pass();
    }
  }

  if (op.kind === 'update' && op.result?.action === 'updated' && op.result.to !== op.to) {
    fail(where, `result.to (${op.result.to}) and the record's to (${op.to}) disagree about what was installed`);
  }

  if (op.state === 'running' && op.pid === null && op.detached) {
    fail(where, 'a running detached operation with no pid cannot be recovered when the worker dies');
  }
}

function checkStatus(where, status) {
  if (status.autoUpdate !== status.updates.automatic) {
    fail(where, `autoUpdate (${status.autoUpdate}) and updates.automatic (${status.updates.automatic}) disagree; 2.x clients read the first and 3.x the second`);
  } else {
    pass();
  }

  const first = status.services[0] ?? null;
  if (first === null) {
    if (status.t3 !== null) fail(where, 't3 is set on a machine with no services');
  } else {
    const expected = {
      installed: first.installed,
      nightly: first.latest,
      upToDate: first.upToDate,
      serverRunning: first.running,
      healthy: first.healthy,
      port: first.port,
    };
    for (const [key, value] of Object.entries(expected)) {
      if (status.t3 === null) {
        fail(where, 't3 is null on a machine that has services; a 2.x client would see nothing at all');
        break;
      }
      if (status.t3[key] !== value) {
        fail(where, `t3.${key} is ${JSON.stringify(status.t3[key])} but the first service says ${JSON.stringify(value)}`);
      }
    }
    if (status.pendingRestart !== first.pendingRestart) {
      fail(where, 'the top-level pendingRestart does not mirror the first service');
    }
    if (JSON.stringify(status.lastUpdate) !== JSON.stringify(first.lastUpdate)) {
      fail(where, 'the top-level lastUpdate does not mirror the first service');
    }
    if (JSON.stringify(status.connect) !== JSON.stringify(first.relay)) {
      fail(where, 'connect does not mirror the first service relay');
    }
    pass();
  }

  const monitored = status.services.filter((service) => service.busy.monitored).length;
  const unmonitored = status.services.length - monitored;
  if (status.busy.monitoredServices !== monitored || status.busy.unmonitoredServices !== unmonitored) {
    fail(where, `busy counts say ${status.busy.monitoredServices}/${status.busy.unmonitoredServices}, the services say ${monitored}/${unmonitored}`);
  } else {
    pass();
  }

  const blocking = status.services.some((service) => service.busy.busy);
  if (status.services.length > 0 && status.busy.busy !== blocking) {
    fail(where, `the aggregate says busy ${status.busy.busy} while ${blocking ? 'a service is busy' : 'no service is busy'}`);
  }
  const anyUnknown = status.services.some((service) => service.busy.unknown);
  if (status.services.length > 0 && status.busy.unknown !== anyUnknown) {
    fail(where, `the aggregate says unknown ${status.busy.unknown} while ${anyUnknown ? 'a probe could not be read' : 'every probe was read'}`);
  }
  for (const service of status.services) {
    if (service.busy.unknown && !service.busy.busy) {
      fail(where, `${service.id}: busy is unknown and not blocking, which is the fail-open the busy gate must never do`);
    }
    // The three states the busy gate must never blur into each other: nobody
    // configured a probe, somebody declared there is nothing to protect, and a
    // probe ran. The first blocks, the second permits, and only the second may
    // be silent about it.
    if (!service.busy.monitored && service.busy.evidence !== 'unmonitored') {
      fail(where, `${service.id}: not monitored but the evidence is "${service.busy.evidence}"`);
    }
    if (service.busy.evidence === 'unmonitored' && (service.busy.monitored || !service.busy.unknown || !service.busy.busy)) {
      fail(where, `${service.id}: an unmonitored service must read as monitored false, unknown true and busy true, so disruptive work waits for somebody to say what "busy" means here`);
    }
    if (service.busy.evidence === 'none' && (!service.busy.monitored || service.busy.busy || service.busy.unknown)) {
      fail(where, `${service.id}: busy type "none" is a declaration that there is nothing to protect, so it reads as monitored true, busy false and unknown false`);
    }
    if (service.upToDate !== null && service.latest === null) {
      fail(where, `${service.id}: upToDate is ${service.upToDate} with no latest version to compare against`);
    }
    if (service.latest !== null && service.installed !== null && service.upToDate !== (service.installed === service.latest)) {
      fail(where, `${service.id}: upToDate does not follow from installed ${service.installed} and latest ${service.latest}`);
    }
    if (service.endpoint.reachable !== null) {
      fail(where, `${service.id}: status reported endpoint reachability, which only doctor --deep probes`);
    }
    if (service.busy.evidence === 'timed-out' && !status.timing.partial) {
      fail(where, `${service.id}: a probe timed out and timing.partial is false`);
    }
  }

  if (!status.timing.partial && status.timing.elapsedMs > status.timing.budgetMs) {
    fail(where, `elapsed ${status.timing.elapsedMs}ms is over the ${status.timing.budgetMs}ms budget and partial is false`);
  }
  if (!status.config.ok && status.config.problems.length === 0) {
    fail(where, 'the configuration is not ok and no problem says why');
  }
  if (status.config.problems.some((problem) => problem.level === 'error') !== !status.config.ok) {
    fail(where, 'config.ok does not follow from the problems listed');
  }

  const seen = new Set();
  for (const group of ['running', 'queued', 'recent']) {
    for (const summary of status.operations[group]) {
      if (seen.has(summary.id)) fail(where, `operation ${summary.id} appears twice in status.operations`);
      seen.add(summary.id);
      if (group === 'running' && summary.state !== 'running') {
        fail(where, `${summary.id} is under operations.running with state ${summary.state}`);
      }
      if (group === 'queued' && summary.state !== 'queued') {
        fail(where, `${summary.id} is under operations.queued with state ${summary.state}`);
      }
      if (group === 'queued' && summary.expiresAt === null) {
        fail(where, `${summary.id} is queued with no expiry, so it would sit there forever`);
      }
      if (group === 'recent' && summary.state !== 'finished') {
        fail(where, `${summary.id} is under operations.recent with state ${summary.state}`);
      }
    }
  }
  pass();
}

function checkDoctor(where, doctor) {
  const counts = { ok: 0, warn: 0, fail: 0 };
  for (const check of doctor.checks) counts[check.level] += 1;
  for (const level of ['ok', 'warn', 'fail']) {
    if (doctor.counts[level] !== counts[level]) {
      fail(where, `counts.${level} is ${doctor.counts[level]}, the checks say ${counts[level]}`);
    }
  }
  if (doctor.ok !== (counts.fail === 0)) {
    fail(where, `ok is ${doctor.ok} with ${counts.fail} failing checks`);
  }
  for (const check of doctor.checks) {
    if (check.level !== 'ok' && !check.fix) {
      fail(where, `${check.id} is "${check.level}" and offers no fix, which leaves the reader with a complaint`);
    }
  }
  if (!doctor.deep && doctor.checks.some((check) => check.id.endsWith('.endpoint') || check.id.endsWith('.latest'))) {
    fail(where, 'a shallow doctor ran a network check');
  }
  pass();
}

function checkCrossReferences(loaded) {
  // An operation id that appears in more than one fixture has to describe the
  // same operation everywhere, or a client that stitches two fixtures together
  // in a test would be reconciling two different worlds.
  const byId = new Map();
  const remember = (where, summary) => {
    const known = byId.get(summary.id);
    if (!known) {
      byId.set(summary.id, { where, summary });
      return;
    }
    for (const key of ['kind', 'service', 'target', 'actionId', 'mode', 'requestedAt']) {
      if (JSON.stringify(known.summary[key]) !== JSON.stringify(summary[key])) {
        fail(where, `operation ${summary.id} has ${key} ${JSON.stringify(summary[key])} here and ${JSON.stringify(known.summary[key])} in ${known.where}; an id binds its intent`);
      }
    }
    pass();
  };

  for (const [name, data] of loaded) {
    const where = `fixtures/${name}`;
    if (data.operations && !Array.isArray(data.operations)) {
      for (const group of ['running', 'queued', 'recent']) {
        for (const summary of data.operations[group]) remember(where, summary);
      }
    }
    if (Array.isArray(data.operations)) {
      for (const summary of data.operations) remember(where, summary);
    }
    if (data.op) remember(where, data.op);
  }
}

// ---------------------------------------------------------------------------
// The canonical hash, implemented again.
//
// Written over bytes rather than over a decoded string, and not shared with
// contract/tools/canonical.mjs, because a vector file checked by the code that
// produced it checks nothing at all. The two implementations agreeing is the
// evidence; if they ever disagree, one of them is the bug the clients would
// otherwise have shipped.
// ---------------------------------------------------------------------------

function utf8Length(byte) {
  if (byte < 0x80) return 1;
  if (byte >= 0xc2 && byte <= 0xdf) return 2;
  if (byte >= 0xe0 && byte <= 0xef) return 3;
  if (byte >= 0xf0 && byte <= 0xf4) return 4;
  return 0;
}

function isValidUtf8Bytes(buffer) {
  let index = 0;
  while (index < buffer.length) {
    const length = utf8Length(buffer[index]);
    if (length === 0) return false;
    if (index + length > buffer.length) return false;
    for (let offset = 1; offset < length; offset += 1) {
      const byte = buffer[index + offset];
      if (byte < 0x80 || byte > 0xbf) return false;
    }
    // Overlong forms and the surrogate range are not valid UTF-8 either, and a
    // decoder that accepts them gives two byte strings the same hash.
    if (length === 3 && buffer[index] === 0xe0 && buffer[index + 1] < 0xa0) return false;
    if (length === 3 && buffer[index] === 0xed && buffer[index + 1] >= 0xa0) return false;
    if (length === 4 && buffer[index] === 0xf0 && buffer[index + 1] < 0x90) return false;
    if (length === 4 && buffer[index] === 0xf4 && buffer[index + 1] >= 0x90) return false;
    index += length;
  }
  return true;
}

const TRIM_BYTES = new Set([0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x20]);

function canonicalBytesAgain(buffer) {
  if (!isValidUtf8Bytes(buffer)) return null;

  let start = 0;
  if (buffer.length >= 3 && buffer[0] === 0xef && buffer[1] === 0xbb && buffer[2] === 0xbf) start = 3;

  const out = [];
  for (let index = start; index < buffer.length; index += 1) {
    const byte = buffer[index];
    if (byte === 0x0d) {
      out.push(0x0a);
      if (buffer[index + 1] === 0x0a) index += 1;
      continue;
    }
    out.push(byte);
  }

  let head = 0;
  let tail = out.length;
  while (head < tail && TRIM_BYTES.has(out[head])) head += 1;
  while (tail > head && TRIM_BYTES.has(out[tail - 1])) tail -= 1;

  return Buffer.from([...out.slice(head, tail), 0x0a]);
}

function checkHashVectors() {
  const vectors = readJson(VECTOR_FILE);

  const { errors } = validate(registry.get('hash-vectors.schema.json'), vectors, { file: 'hash-vectors.schema.json' });
  for (const error of errors) fail('hash-vectors.json', error);
  if (errors.length === 0) pass();

  const names = new Set();
  for (const vector of vectors.vectors) {
    const where = `hash-vectors.json/${vector.name}`;
    if (names.has(vector.name)) fail(where, 'the name is used twice');
    names.add(vector.name);

    const input = Buffer.from(vector.inputBase64, 'base64');
    if (input.length !== vector.inputBytes) {
      fail(where, `inputBytes says ${vector.inputBytes}, the base64 decodes to ${input.length}`);
    }

    const canonical = canonicalBytesAgain(input);
    if (canonical === null) {
      if (vector.sha256 !== null) fail(where, 'the bytes are not valid UTF-8 and the vector carries a hash anyway');
      else pass();
      continue;
    }
    if (vector.sha256 === null) {
      fail(where, 'the bytes are valid UTF-8 and the vector carries no hash');
      continue;
    }

    const expected = Buffer.from(vector.canonicalBase64, 'base64');
    if (Buffer.compare(canonical, expected) !== 0) {
      fail(where, `the canonical bytes disagree: expected ${JSON.stringify(expected.toString('utf8'))}, got ${JSON.stringify(canonical.toString('utf8'))}`);
      continue;
    }
    if (canonical.length !== vector.canonicalBytes) {
      fail(where, `canonicalBytes says ${vector.canonicalBytes}, the canonical form is ${canonical.length} bytes`);
    }

    const hash = crypto.createHash('sha256').update(canonical).digest('hex');
    if (hash !== vector.sha256) fail(where, `hash is ${hash}, the vector says ${vector.sha256}`);
    else pass();

    if (canonical[canonical.length - 1] !== 0x0a) fail(where, 'the canonical form does not end in exactly one newline');
    if (canonical.length >= 2 && canonical[canonical.length - 2] === 0x0a) fail(where, 'the canonical form ends in more than one newline');
    if (canonical.includes(0x0d)) fail(where, 'the canonical form still contains a carriage return');

    // The claim each vector is really making: whether it is accepted is decided
    // after canonicalisation, on the canonical bytes, and never on the input.
    let parses = false;
    try {
      const parsed = JSON.parse(canonical.toString('utf8'));
      parses = parsed !== null && typeof parsed === 'object' && !Array.isArray(parsed)
        && typeof parsed.version === 'number' && Array.isArray(parsed.machines);
    } catch {
      parses = false;
    }
    if (parses !== vector.accepted) {
      fail(where, `accepted is ${vector.accepted} but the canonical bytes ${parses ? 'are' : 'are not'} a controller document`);
    } else {
      pass();
    }
  }

  // The whole point of the file: many byte strings, one hash.
  const groups = new Map();
  for (const vector of vectors.vectors) {
    if (vector.sha256 === null) continue;
    if (!groups.has(vector.sha256)) groups.set(vector.sha256, []);
    groups.get(vector.sha256).push(vector.name);
  }
  const biggest = [...groups.values()].sort((a, b) => b.length - a.length)[0] ?? [];
  if (biggest.length < 6) {
    fail('hash-vectors.json', `only ${biggest.length} encodings of one document share a hash; the file is meant to prove that BOM, CRLF, CR, trailing newlines and surrounding whitespace all collapse to the same bytes`);
  } else {
    pass();
  }
}

/**
 * The setup document fixture and the replies that report its hash must agree.
 *
 * This is the one place where a client can check its own canonicalisation
 * against something real: canonicalise contract/fixtures/controller-document.
 * sites-helpers.json in Swift, Kotlin or C#, and the answer has to be the hash
 * config.read and config.meta report for it. If the two ever drift apart, the
 * cross-language check silently stops meaning anything.
 */
function checkSetupHashAgrees() {
  const documentFile = path.join(FIXTURE_DIR, 'controller-document.sites-helpers.json');
  if (!fs.existsSync(documentFile)) return;

  const bytes = fs.readFileSync(documentFile);
  const canonical = canonicalBytesAgain(bytes);
  if (canonical === null) {
    fail('fixtures/controller-document.sites-helpers.json', 'is not valid UTF-8');
    return;
  }
  const hash = crypto.createHash('sha256').update(canonical).digest('hex');

  for (const [file, at] of [['config.read.json', 'hash'], ['config.meta.json', 'meta.hash']]) {
    const target = path.join(FIXTURE_DIR, file);
    if (!fs.existsSync(target)) continue;
    const data = readJson(target);
    const reported = at === 'hash' ? data.hash : data.meta?.hash;
    if (reported !== hash) {
      fail(`fixtures/${file}`, `${at} is ${reported}, but the canonical hash of controller-document.sites-helpers.json is ${hash}; a client checking its own canonicalisation against this pair would be checking against nothing`);
    } else {
      pass();
    }
  }

  const read = path.join(FIXTURE_DIR, 'config.read.json');
  if (fs.existsSync(read)) {
    const data = readJson(read);
    if (data.bytes !== bytes.length) {
      fail('fixtures/config.read.json', `bytes is ${data.bytes} and the document is ${bytes.length} bytes`);
    } else {
      pass();
    }
    if (JSON.stringify(data.controller) !== JSON.stringify(readJson(documentFile))) {
      fail('fixtures/config.read.json', 'the document it returns is not the document fixture it reports the hash of');
    } else {
      pass();
    }
  }
}

// ---------------------------------------------------------------------------
// Coverage. A fixture set that never exercises a reason code is a reason code
// no client has ever drawn.
// ---------------------------------------------------------------------------

function collect(value, key, into) {
  if (Array.isArray(value)) {
    for (const item of value) collect(item, key, into);
    return;
  }
  if (value === null || typeof value !== 'object') return;
  for (const [name, child] of Object.entries(value)) {
    if (name === key && typeof child === 'string') into.add(child);
    collect(child, key, into);
  }
}

function checkCoverage(index, loaded) {
  const codes = new Set();
  const kinds = new Set();
  const phases = new Set();
  for (const data of loaded.values()) {
    collect(data, 'reasonCode', codes);
    collect(data, 'kind', kinds);
    collect(data, 'phase', phases);
  }

  const declared = registry.get('common.schema.json').$defs.reasonCode.enum;
  const exempt = new Map(Object.entries(index.coverageExemptions?.reasonCodes ?? {}));
  const missing = declared.filter((code) => !codes.has(code) && !exempt.has(code));
  if (missing.length > 0) {
    fail('fixtures/index.json', `no fixture shows these reason codes, so no client has anything to render them from: ${missing.join(', ')}. Add a fixture, or record why not under coverageExemptions.reasonCodes.`);
  } else {
    pass();
  }
  for (const code of exempt.keys()) {
    if (!declared.includes(code)) fail('fixtures/index.json', `coverageExemptions names "${code}", which is not a reason code`);
    if (codes.has(code)) warnings.push(`fixtures/index.json: "${code}" is exempted from coverage and is covered anyway; drop the exemption`);
  }

  const declaredKinds = registry.get('common.schema.json').$defs.opKind.enum;
  const missingKinds = declaredKinds.filter((kind) => !kinds.has(kind));
  if (missingKinds.length > 0) {
    fail('fixtures/index.json', `no fixture contains an operation of kind: ${missingKinds.join(', ')}`);
  } else {
    pass();
  }

  const commands = new Set(index.fixtures.map((entry) => entry.command));
  const required = ['status', 'busy', 'update', 'restart', 'boot', 'sleep', 'run', 'cycle', 'op', 'cancel', 'history', 'logs', 'doctor', 'bundle', 'config', 'config set', 'policy', 'self-update', 'version', 'help'];
  const missingCommands = required.filter((command) => !commands.has(command));
  if (missingCommands.length > 0) {
    fail('fixtures/index.json', `no fixture for: ${missingCommands.join(', ')}`);
  } else {
    pass();
  }

  return { codes: codes.size, declared: declared.length, phases: phases.size };
}

// ---------------------------------------------------------------------------

function main() {
  loadSchemas();
  if (registry.size === 0) {
    process.stderr.write('contract/schemas holds no schemas\n');
    process.exit(1);
  }

  lintSchemas();

  const index = readJson(INDEX_FILE);
  const loaded = checkFixtures(index);

  for (const [name, data] of loaded) {
    const where = `fixtures/${name}`;
    if (data.op) checkOperationRecord(where, data.op);
    if (data.operations && !Array.isArray(data.operations) && data.services) checkStatus(where, data);
    if (data.checks && data.counts) checkDoctor(where, data);
    if (data.status && data.doctor && data.history) {
      checkStatus(`${where}/status`, data.status);
      checkDoctor(`${where}/doctor`, data.doctor);
    }
    if (data.source && Array.isArray(data.lines)) {
      if (data.returned !== data.lines.length) fail(where, `returned is ${data.returned} and there are ${data.lines.length} lines`);
      else pass();
    }
    if (Array.isArray(data.operations)) {
      if (data.returned !== data.operations.length) fail(where, `returned is ${data.returned} and there are ${data.operations.length} operations`);
      if (data.returned > data.limit) fail(where, `returned ${data.returned} with a limit of ${data.limit}`);
      const times = data.operations.map((operation) => Date.parse(operation.requestedAt));
      for (let index = 1; index < times.length; index += 1) {
        if (times[index] > times[index - 1]) fail(where, 'history is not newest first');
      }
      pass();
    }
    if (Array.isArray(data.children)) {
      const services = data.children.map((child) => child.service);
      if (new Set(services).size !== services.length) fail(where, 'a cycle acted on the same service twice');
      else pass();
    }
  }

  checkCrossReferences(loaded);
  checkSetupHashAgrees();
  checkHashVectors();
  const coverage = checkCoverage(index, loaded);

  const out = process.stdout;
  const documents = index.fixtures.filter((entry) => entry.kind === 'document').length;
  const refused = index.fixtures.filter((entry) => entry.expect === 'invalid').length;
  out.write(
    `contract v3: ${registry.size} schemas, ${index.fixtures.length} fixtures `
    + `(${index.fixtures.length - documents} replies, ${documents} documents, ${refused} of them expected to be refused), `
    + `${coverage.codes}/${coverage.declared} reason codes shown, ${checks} checks\n`,
  );
  for (const warning of warnings) out.write(`  warning  ${warning}\n`);
  if (failures.length === 0) {
    out.write('everything holds\n');
    return;
  }
  out.write(`\n${failures.length} problem${failures.length === 1 ? '' : 's'}:\n`);
  for (const failure of failures) out.write(`  ${failure}\n`);
  process.exitCode = 1;
}

// Importable so contract/test can exercise the validator itself. A checking tool
// nobody checks is a tool that quietly stops checking.
export { validate, resolveRef, loadSchemas, registry, checkControllerDocument, canonicalBytesAgain, isValidUtf8Bytes };

// Compared through realpath, not through resolve. On macOS /var and /tmp are
// symlinks into /private, so the path a caller types and the path import.meta.url
// reports are different strings for the same file; a plain comparison silently
// skips main() and the check then "passes" by never running.
function invokedDirectly() {
  if (!process.argv[1]) return false;
  const real = (file) => {
    try {
      return fs.realpathSync(file);
    } catch {
      return path.resolve(file);
    }
  };
  return real(process.argv[1]) === real(fileURLToPath(import.meta.url));
}

if (invokedDirectly()) main();
