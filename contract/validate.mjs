#!/usr/bin/env node
// Checks the shared contract: node contract/validate.mjs
//
// Four passes, and each one exists because the others cannot see the fault it
// catches.
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
//      its checks, that a stated outcome is the one the reducer gives, and that
//      a config reply's text really is the document it reports the hash of.
//   4. Every vector family the index lists is validated against its schema,
//      counted against the index, and re-run through its reference
//      implementation; then the checks this file has its own implementation for
//      are repeated, because a vector checked only by the code that produced it
//      checks nothing at all.
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
import { fileURLToPath, pathToFileURL } from 'node:url';
import { reduceOutcome, recordInput } from './tools/outcome.mjs';
import { checkControllerDocument, checkDocument } from './tools/document-rules.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const SCHEMA_DIR = path.join(HERE, 'schemas');
const FIXTURE_DIR = path.join(HERE, 'fixtures');
const INDEX_FILE = path.join(FIXTURE_DIR, 'index.json');
const VECTOR_DIR = path.join(HERE, 'vectors');
const VECTOR_INDEX_FILE = path.join(VECTOR_DIR, 'index.json');

// Where failures go. main() reports into one sink for the whole run, and
// checkReply() swaps in a fresh one for the length of a single reply so a
// caller such as the agent's conformance test gets that reply's problems back
// as a list instead of a process exit.
let sink = { failures: [], warnings: [], checks: 0 };

function fail(where, message) {
  sink.failures.push(`${where}: ${message}`);
}

function pass() {
  sink.checks += 1;
}

function warn(message) {
  sink.warnings.push(message);
}

/** Run `check` against a fresh sink and hand that sink back. */
function collecting(check) {
  const saved = sink;
  sink = { failures: [], warnings: [], checks: 0 };
  try {
    check();
    return sink;
  } finally {
    sink = saved;
  }
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

/**
 * Whether `value` is of the type a schema names. `expected` may be a list, as
 * in `"type": ["object", "null"]`, which the shared document uses for its
 * deprecated keys: reading it as one string would refuse every document that
 * carries them.
 */
function matchesType(value, expected) {
  if (Array.isArray(expected)) return expected.some((option) => matchesType(value, option));
  const actual = typeOf(value);
  if (expected === 'number') return actual === 'number' || actual === 'integer';
  return actual === expected;
}

/** Equality for const and enum, where the schema's own key order is the order that counts. */
function deepEqual(a, b) {
  return JSON.stringify(a) === JSON.stringify(b);
}

/**
 * JSON equality: objects by key set in any order, arrays element by element,
 * strings, booleans and null by identity, numbers by value. This is the
 * comparison contract/vectors/index.json tells every consumer to use, and the
 * one a re-run reference is held to.
 */
function jsonEqual(a, b) {
  if (a === b) return true;
  if (typeof a !== 'object' || typeof b !== 'object' || a === null || b === null) return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  if (Array.isArray(a)) return a.length === b.length && a.every((item, index) => jsonEqual(item, b[index]));
  const keys = Object.keys(a);
  if (keys.length !== Object.keys(b).length) return false;
  return keys.every((key) => Object.hasOwn(b, key) && jsonEqual(a[key], b[key]));
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
    push(`expected ${[schema.type].flat().join(' or ')}, got ${typeOf(data)}`);
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

      // A key called "type" inside `properties` is a property name holding a
      // schema, not the keyword, which is why only strings and lists count.
      const declaredTypes = typeof node.type === 'string' ? [node.type] : Array.isArray(node.type) ? node.type : [];
      for (const type of declaredTypes) {
        if (!TYPES.has(type)) fail(`schemas/${name}`, `${at}: "${type}" is not a JSON Schema type`);
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

    // A setup document fixture may be here precisely because it is wrong. It
    // goes through section 9.7 as a client's peer intake reads it, from the
    // file's own bytes: tools/document-rules.mjs writes the schema out by hand
    // and adds the rules a schema cannot express (a helper cycle, a machine
    // pointing at a site nobody defined, a member written twice). Both sources
    // count toward whether the fixture is refused, and the two readings of the
    // schema have to agree with each other as well.
    const isSetupDocument = entry.kind === 'document' && entry.schema === 'controller-document.schema.json';
    const graph = isSetupDocument ? checkDocument(fs.readFileSync(file), { intake: 'peer' }) : { errors: [], warnings: [] };
    const describe = (problem) => `${problem.path === '' ? '<document>' : problem.path}: ${problem.message} (${problem.code})`;
    const problems = [...errors, ...graph.errors.map(describe)];
    if (isSetupDocument && !graph.errors.some((problem) => problem.code === 'duplicate-member')) {
      const written = graph.errors.some((problem) => problem.code === 'schema');
      if (written !== (errors.length > 0)) {
        fail(`fixtures/${entry.file}`, `${entry.schema} ${errors.length > 0 ? 'refuses' : 'accepts'} this document and tools/document-rules.mjs ${written ? 'reports' : 'reports no'} schema error; the schema written out by hand has drifted from the schema`);
      } else {
        pass();
      }
    }

    if (entry.expect === 'invalid') {
      // The point of an invalid fixture is that something rejects it. One that
      // quietly passes means the rule it was written for does not exist.
      if (problems.length === 0) {
        fail(`fixtures/${entry.file}`, `the index expects this to be refused and nothing refused it; the rule it was written for is missing from ${entry.schema} and from tools/document-rules.mjs`);
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
      for (const warning of graph.warnings) warn(`fixtures/${entry.file}: ${warning.message} (${warning.code})`);
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
    for (const key of ['action', 'reasonCode', 'outcome']) {
      if (entry[key] !== (data[key] ?? null)) {
        fail(`fixtures/${entry.file}`, `index says ${key} ${JSON.stringify(entry[key])}, the fixture says ${JSON.stringify(data[key] ?? null)}`);
      }
    }
    if (entry.exitCode !== exitCodeOf(data)) {
      fail(`fixtures/${entry.file}`, `index says exit code ${entry.exitCode}, and rule 7 gives ${exitCodeOf(data)}: 0 when ok is true or the action is one of ${OK_EXIT_ACTIONS.join(', ')}, 1 otherwise`);
    }
    // A client picks its decoder by schema, and the schema a reply needs follows
    // from the command and the reply's own shape. An index that names another
    // one would send the client's test down a path its production code never
    // takes.
    const expectedSchema = schemaForCommand(entry.command, data);
    if (entry.schema !== expectedSchema) {
      fail(`fixtures/${entry.file}`, `index names ${entry.schema}, and a ${entry.command} reply of this shape is read with ${expectedSchema}`);
    }
    checkOutcomePresence(`fixtures/${entry.file}`, entry, data);
    checkFloorAtMostVersion(`fixtures/${entry.file}`, data);
    pass();
  }
  return loaded;
}

/** An x.y.z version as three numbers, compared part by part. */
const versionParts = (version) => version.split('.').map(Number);
const isBelow = (a, b) => {
  const [x, y] = [versionParts(a), versionParts(b)];
  const index = x.findIndex((part, at) => part !== y[at]);
  return index >= 0 && x[index] < y[index];
};

/**
 * A fixture shows an agent that can run: the version floor it reports is at
 * most the version it runs. A signed agent declares a floor no higher than
 * itself and the launcher refuses a signed tree below the floor (v3.md section
 * 13), so a status, bundle or self-update fixture that says otherwise shows a
 * reply no agent sends in the normal course, and a client test written against
 * it would learn the wrong thing. This is a rule for the fixtures only: a real
 * agent can report a live tree below its floor (an unsigned tree is never
 * floor-checked, and doctor's agent.floor check exists to say so), which is
 * why checkReply() does not apply it.
 */
function checkFloorAtMostVersion(where, data) {
  const pairs = [];
  const agentPair = (label, agent) => {
    if (agent !== null && typeof agent === 'object') pairs.push([`${label}.floor`, agent.floor, `${label}.version`, agent.version]);
  };
  agentPair('agent', data.agent);
  agentPair('status.agent', data.status?.agent);
  if (data.current !== null && typeof data.current === 'object') pairs.push(['floor', data.floor, 'current.version', data.current.version]);
  for (const [floorAt, floor, versionAt, version] of pairs) {
    if (typeof floor !== 'string' || typeof version !== 'string') continue;
    if (isBelow(version, floor)) fail(where, `${versionAt} ${version} is below ${floorAt} ${floor}; an agent runs at or above its floor`);
    else pass();
  }
}

/** The actions rule 7 answers with exit 0 whatever `ok` says (v3.md section 1). */
const OK_EXIT_ACTIONS = ['accepted', 'already-running', 'queued', 'deferred', 'noop', 'conflict'];

/** The exit code rule 7 gives a reply. Written here apart from the generator, so the index is checked rather than echoed. */
function exitCodeOf(reply) {
  return reply.ok === true || OK_EXIT_ACTIONS.includes(reply.action) ? 0 : 1;
}

/**
 * The schema a reply to `command` validates against. A refusal that carries
 * the refusal shape (a `command` and no `action`) is an error reply whatever
 * command it answers, and a reply with no `contract` is from a 2.x agent.
 */
function schemaForCommand(command, reply) {
  if (reply.contract === undefined) return command === 'status' ? 'legacy-2x-status.schema.json'
    : command === 'config' ? 'legacy-2x-config.schema.json' : 'legacy-2x-mutation.schema.json';
  if (reply.ok === false && 'command' in reply && !('action' in reply)) return 'error.schema.json';
  if (command === 'config') return 'config-read.schema.json';
  if (command === 'config meta') return 'config-meta.schema.json';
  if (command === 'config set') return 'config-set.schema.json';
  if (['policy', 'policy set', 'auto-update'].includes(command)) return 'policy.schema.json';
  if (command.startsWith('service-config')) return 'service-config.schema.json';
  return `${command}.schema.json`;
}

/**
 * Whether a fixture answers a mutating invocation (v3.md section 5.6): the
 * replies that carry `outcome` from agent 3.1.0 on. A dry-run cycle and
 * `self-update --check` change nothing, so they carry none.
 */
function isMutatingReply(command, reply) {
  if (['update', 'restart', 'boot', 'sleep', 'run', 'cancel', 'auto-update', 'config set', 'policy set', 'service-config set'].includes(command)) return true;
  if (command === 'cycle') return reply.dryRun !== true;
  if (command === 'self-update') return reply.action !== 'checked';
  return false;
}

/**
 * A 3.1.0 reply to a mutating invocation states its outcome, and a reply that
 * changed nothing does not. Fixtures that model an older agent say so with the
 * agent-3.0 tag and are exempt from the first half.
 */
function checkOutcomePresence(where, entry, data) {
  if (data.contract !== 3) return;
  const mutating = isMutatingReply(entry.command, data);
  if (mutating && !('outcome' in data) && !(entry.tags ?? []).includes('agent-3.0')) {
    fail(where, `a ${entry.command} reply from agent 3.1.0 carries outcome (section 5.6), and this one does not; state it, or tag the fixture agent-3.0 if it models an older agent`);
  }
  if (!mutating && 'outcome' in data) {
    fail(where, `a ${entry.command} reply changes nothing, so it carries no outcome`);
  }
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
// Outcomes. A stated outcome is a second statement of what the fields beside
// it already say, so it is held to the reducer of v3.md section 5.6: equal to
// what the reducer gives without it, or `unknown` where the reducer says
// `failed`. That is the one direction a producer that knows more is allowed to
// move it, and it is never more optimistic than the function.
// ---------------------------------------------------------------------------

let outcomesCompared = 0;

function without(object, key) {
  const { [key]: _omitted, ...rest } = object;
  return rest;
}

function checkStatedOutcome(where, label, stated, derived) {
  outcomesCompared += 1;
  if (stated === derived || (stated === 'unknown' && derived === 'failed')) {
    pass();
    return;
  }
  fail(where, `${label} is ${JSON.stringify(stated)}, and the reducer of section 5.6 gives ${JSON.stringify(derived)} for the fields beside it; only failed may be stated as unknown`);
}

/** Every operation record and summary inside `value`, wherever a reply nests them. */
function forEachOperation(value, at, visit) {
  if (Array.isArray(value)) {
    value.forEach((item, index) => forEachOperation(item, `${at}[${index}]`, visit));
    return;
  }
  if (value === null || typeof value !== 'object') return;
  const isOperation = typeof value.id === 'string' && typeof value.kind === 'string'
    && typeof value.state === 'string' && typeof value.requestedAt === 'string';
  if (isOperation) visit(at, value, 'result' in value ? 'record' : 'summary');
  for (const [key, child] of Object.entries(value)) {
    if (isOperation && key === 'result') continue;
    forEachOperation(child, `${at}/${key}`, visit);
  }
}

function checkRecordOutcome(where, op) {
  const result = op.result !== null && typeof op.result === 'object' ? op.result : null;
  if ('outcome' in op) {
    const bare = { ...without(op, 'outcome'), result: result === null ? null : without(result, 'outcome') };
    checkStatedOutcome(where, `op ${op.id} outcome`, op.outcome, reduceOutcome(recordInput(bare)));
  }
  if (result !== null && 'outcome' in result) {
    checkStatedOutcome(where, `op ${op.id} result.outcome`, result.outcome, reduceOutcome(without(result, 'outcome')));
  }
  // A finished record is reduced from either field, so the two must say the
  // same thing, and one of them alone is a record half written.
  if (op.state === 'finished' && ('outcome' in op || (result !== null && 'outcome' in result))) {
    if (op.outcome !== result?.outcome) {
      fail(where, `op ${op.id} states outcome ${JSON.stringify(op.outcome)} and its result ${JSON.stringify(result?.outcome)}; a finished record states the same outcome in both places`);
    } else {
      pass();
    }
  }
}

function checkOutcomes(where, data) {
  if ('outcome' in data) checkStatedOutcome(where, 'outcome', data.outcome, reduceOutcome(without(data, 'outcome')));
  forEachOperation(data, '', (at, operation, form) => {
    if (form === 'record') checkRecordOutcome(`${where}${at}`, operation);
    else if ('outcome' in operation) {
      checkStatedOutcome(`${where}${at}`, `summary ${operation.id} outcome`, operation.outcome, reduceOutcome(without(operation, 'outcome')));
    }
  });
}

/**
 * A config reply's `raw` is the document it reports the hash of: null exactly
 * when there is no document, otherwise canonical text whose UTF-8 length is
 * `bytes`, whose sha256 is `hash`, and which parses to `controller`. A client
 * adopts these bytes as they are, so any one of the four being false makes it
 * adopt something other than what the machine holds.
 */
function checkConfigText(where, reply) {
  if (!('raw' in reply)) return;
  if ((reply.raw === null) !== (reply.controller === null)) {
    fail(where, `raw is ${reply.raw === null ? 'null' : 'text'} and controller is ${reply.controller === null ? 'null' : 'a document'}; raw is null exactly when controller is`);
    return;
  }
  if (typeof reply.raw !== 'string') {
    pass();
    return;
  }
  const bytes = Buffer.from(reply.raw, 'utf8');
  if (bytes.length !== reply.bytes) fail(where, `bytes is ${reply.bytes} and raw is ${bytes.length} bytes of UTF-8`);
  if (bytes.length > 1048576) fail(where, `raw is ${bytes.length} bytes, over the 1048576 a stored document may be`);
  const hash = crypto.createHash('sha256').update(bytes).digest('hex');
  if (hash !== reply.hash) fail(where, `the sha256 of raw is ${hash} and the reply reports ${reply.hash}; a client adopting raw would adopt a document under the wrong hash`);
  const canonical = canonicalBytesAgain(bytes);
  if (canonical === null || Buffer.compare(canonical, bytes) !== 0) {
    fail(where, 'raw is not in canonical form, so it is not the bytes the agent stores');
  }
  let parsed;
  try {
    parsed = JSON.parse(reply.raw);
  } catch (error) {
    fail(where, `raw does not parse: ${error.message}`);
    return;
  }
  if (!jsonEqual(parsed, reply.controller)) fail(where, 'raw parses to a different document from controller');
  else pass();
}

/**
 * A `busy` reply's counts and aggregate follow from the services it reports,
 * as the status aggregate does in checkStatus. In the one-service shape there
 * is at most one service to count, and none at all is an answer of its own:
 * nothing to protect, so monitored, idle and known, never the shape of a
 * service nobody described.
 */
function checkBusy(where, data) {
  if (Array.isArray(data.services)) {
    const monitored = data.services.filter((service) => service.monitored).length;
    if (data.monitoredServices !== monitored || data.unmonitoredServices !== data.services.length - monitored) {
      fail(where, `busy counts say ${data.monitoredServices}/${data.unmonitoredServices}, the services say ${monitored}/${data.services.length - monitored}`);
    }
    if (data.busy !== data.services.some((service) => service.busy)) fail(where, `the aggregate says busy ${data.busy} and the services do not`);
    if (data.unknown !== data.services.some((service) => service.unknown)) fail(where, `the aggregate says unknown ${data.unknown} and the services do not`);
    pass();
    return;
  }
  const counted = data.monitoredServices + data.unmonitoredServices;
  if (counted > 1) fail(where, `the one-service shape counts ${counted} services`);
  else if (counted === 1 && data.monitoredServices !== (data.monitored ? 1 : 0)) {
    fail(where, `the one service is ${data.monitored ? '' : 'not '}monitored and the counts say ${data.monitoredServices}/${data.unmonitoredServices}`);
  } else if (counted === 0 && (data.evidence !== 'none' || !data.monitored || data.busy || data.unknown)) {
    fail(where, 'a machine with no services answers monitored true, busy false, unknown false and evidence "none"');
  } else {
    pass();
  }
}

/**
 * The rules a schema cannot express about one reply. The fixture pass runs it
 * on every reply fixture, and checkReply() runs it on a reply an agent really
 * produced.
 */
function replySemantics(where, data) {
  if (typeof data.monitoredServices === 'number' && typeof data.unmonitoredServices === 'number') checkBusy(where, data);
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
  checkConfigText(where, data);
  checkOutcomes(where, data);
}

/**
 * Check one reply as the contract does a fixture: against `schemaName`, then
 * against the rules a schema cannot express. Returns the problems, as lines,
 * and an empty list when the reply holds. The agent's conformance test runs
 * every reply its commands really produce through this.
 */
function checkReply(schemaName, data, { where = schemaName } = {}) {
  if (registry.size === 0) loadSchemas();
  const schema = registry.get(schemaName);
  if (schema === undefined) return [`${where}: there is no schema named ${schemaName}`];
  const { errors } = validate(schema, data, { file: schemaName });
  // As in the fixture pass, the semantic rules read fields the schema has just
  // said are missing, so a reply gets one list or the other.
  if (errors.length > 0) return errors.map((error) => `${where}: ${error}`);
  return collecting(() => replySemantics(where, data)).failures;
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

/**
 * Why a document is refused before it is stored or sent (v3.md section 12),
 * again over bytes: the first of too-large, invalid-utf8, empty and
 * not-a-json-object that applies, or null.
 */
function rejectionAgain(input) {
  if (input.length > 1048576) return 'too-large';
  const canonical = canonicalBytesAgain(input);
  if (canonical === null) return 'invalid-utf8';
  if (canonical.length === 1) return 'empty';
  try {
    const parsed = JSON.parse(canonical.toString('utf8'));
    const isDocument = parsed !== null && typeof parsed === 'object' && !Array.isArray(parsed)
      && typeof parsed.version === 'number' && Array.isArray(parsed.machines);
    return isDocument ? null : 'not-a-json-object';
  } catch {
    return 'not-a-json-object';
  }
}

/** The hash family's own checks, over the parsed hash-vectors.json. Its schema is checked with the other families. */
function checkHashVectors(vectors) {
  const names = new Set();
  for (const vector of vectors.vectors) {
    const where = `hash-vectors.json/${vector.name}`;
    if (names.has(vector.name)) fail(where, 'the name is used twice');
    names.add(vector.name);

    const input = Buffer.from(vector.inputBase64, 'base64');
    if (input.length !== vector.inputBytes) {
      fail(where, `inputBytes says ${vector.inputBytes}, the base64 decodes to ${input.length}`);
    }

    // The order the rules apply in is the whole of the rule: a document that
    // breaks two of them is refused for the first, on every side.
    if (vector.accepted !== (vector.reject === null)) {
      fail(where, `accepted is ${vector.accepted} and reject is ${JSON.stringify(vector.reject)}; a vector is refused exactly when it says why`);
    }
    const reject = rejectionAgain(input);
    if (reject !== vector.reject) {
      fail(where, `reject is ${JSON.stringify(vector.reject)}, and section 12 refuses these bytes first for ${JSON.stringify(reject)}`);
    } else {
      pass();
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
    // raw is the stored canonical text, and that text is the document fixture,
    // byte for byte: the four rules of a config reply's raw (null with
    // controller, length, hash, parse) hold for it only if it is.
    if (typeof data.raw !== 'string') {
      fail('fixtures/config.read.json', 'carries no raw; a 3.1.0 read returns the stored text, and clients adopt that, never controller re-serialised');
    } else if (data.raw !== bytes.toString('utf8')) {
      fail('fixtures/config.read.json', 'raw is not the text of controller-document.sites-helpers.json, the document it reports the hash of');
    } else {
      checkConfigText('fixtures/config.read.json', data);
    }
  }

  // An agent before 3.1.0 sends no raw. The fallback a client uses then is to
  // rebuild two-space JSON in received key order and adopt only on a matching
  // hash; the fixture that models that reply has to be one the fallback adopts,
  // or it teaches a client that the fallback never works.
  const older = path.join(FIXTURE_DIR, 'config.read-without-raw.json');
  if (fs.existsSync(older)) {
    const data = readJson(older);
    const rebuilt = canonicalBytesAgain(Buffer.from(`${JSON.stringify(data.controller, null, 2)}\n`, 'utf8'));
    const rebuiltHash = rebuilt === null ? null : crypto.createHash('sha256').update(rebuilt).digest('hex');
    if ('raw' in data) fail('fixtures/config.read-without-raw.json', 'carries raw, which no agent before 3.1.0 sends');
    else if (rebuiltHash !== data.hash) fail('fixtures/config.read-without-raw.json', `rebuilding controller gives ${rebuiltHash}, not the reported ${data.hash}`);
    else pass();
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
  const outcomes = new Set();
  for (const data of loaded.values()) {
    collect(data, 'reasonCode', codes);
    collect(data, 'kind', kinds);
    collect(data, 'phase', phases);
    collect(data, 'outcome', outcomes);
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
    if (codes.has(code)) warn(`fixtures/index.json: "${code}" is exempted from coverage and is covered anyway; drop the exemption`);
  }

  const declaredKinds = registry.get('common.schema.json').$defs.opKind.enum;
  const missingKinds = declaredKinds.filter((kind) => !kinds.has(kind));
  if (missingKinds.length > 0) {
    fail('fixtures/index.json', `no fixture contains an operation of kind: ${missingKinds.join(', ')}`);
  } else {
    pass();
  }

  // An outcome no fixture shows is one no client has decided anything from.
  const declaredOutcomes = registry.get('common.schema.json').$defs.outcome.enum;
  const missingOutcomes = declaredOutcomes.filter((outcome) => !outcomes.has(outcome));
  if (missingOutcomes.length > 0) {
    fail('fixtures/index.json', `no fixture shows these outcomes: ${missingOutcomes.join(', ')}`);
  } else {
    pass();
  }

  const commands = new Set(index.fixtures.map((entry) => entry.command));
  const required = ['status', 'busy', 'update', 'restart', 'boot', 'sleep', 'run', 'cycle', 'op', 'cancel', 'history', 'logs', 'doctor', 'bundle', 'config', 'config meta', 'config set', 'policy', 'self-update', 'version', 'help'];
  const missingCommands = required.filter((command) => !commands.has(command));
  if (missingCommands.length > 0) {
    fail('fixtures/index.json', `no fixture for: ${missingCommands.join(', ')}`);
  } else {
    pass();
  }

  return { codes: codes.size, declared: declared.length, phases: phases.size };
}

// ---------------------------------------------------------------------------
// Pass 4: the vectors.
//
// contract/vectors/index.json names every family, the file it lives in, the
// schema it validates against, its reference implementation and how many cases
// each consumer runs. A consumer trusts those counts to tell it whether it ran
// everything, so the index and the files are held to each other exactly; then
// each case is run again through the reference, and each family is checked
// once more by code in this file that shares nothing with the reference.
// ---------------------------------------------------------------------------

const CONSUMERS = ['agent', 'mac', 'desktop', 'android'];

/** contract/vectors/index.json, parsed. */
function loadVectorIndex() {
  return readJson(VECTOR_INDEX_FILE);
}

function checkVectorIndex(index) {
  const where = 'vectors/index.json';
  const { errors } = validate(registry.get('vector-index.schema.json'), index, { file: 'vector-index.schema.json' });
  for (const error of errors) fail(where, error);
  if (errors.length > 0) return false;
  pass();

  // Exactly the files that are there, the way the fixture index is held to its
  // directory: a family file nobody lists is a family no consumer runs.
  const listed = index.families.map((family) => family.file);
  const onDisk = fs.readdirSync(VECTOR_DIR)
    .filter((name) => name.endsWith('.json') && name !== 'index.json')
    .map((name) => `vectors/${name}`);
  if (fs.existsSync(path.join(HERE, 'hash-vectors.json'))) onDisk.push('hash-vectors.json');
  for (const file of onDisk) {
    if (!listed.includes(file)) fail(where, `${file} is on disk and not in the index, so no consumer will run it`);
  }
  for (const file of listed) {
    if (!onDisk.includes(file)) fail(where, `the index names ${file}, which does not exist`);
  }
  if (!listed.includes('hash-vectors.json')) {
    fail(where, 'hash-vectors.json is not listed; the hash family is listed like every other family');
  }
  if (new Set(listed).size !== listed.length) fail(where, 'the same file is listed twice');
  const names = index.families.map((family) => family.family);
  if (new Set(names).size !== names.length) fail(where, 'a family is listed twice');
  for (const family of index.families) {
    for (const key of ['reference', 'generator']) {
      if (!fs.existsSync(path.join(HERE, family[key]))) fail(where, `${family.family}: its ${key} ${family[key]} does not exist`);
    }
    if (!registry.has(family.schema)) fail(where, `${family.family}: its schema ${family.schema} does not exist`);
  }
  pass();
  return true;
}

/**
 * The cases of a family file by group, in the one shape the checks below read.
 * In the flat layout each vector is its own input and the four fields every
 * implementation reproduces are its expectation, and every listed consumer
 * runs every vector.
 */
function casesByGroup(entry, data) {
  if (entry.layout === 'flat') {
    if (!Array.isArray(data.vectors)) return null;
    const consumers = Object.keys(entry.groups[0]?.consumers ?? {});
    return new Map([[entry.groups[0]?.group ?? 'vectors', data.vectors.map((vector) => ({
      name: vector.name,
      consumers,
      tags: vector.tags ?? [],
      input: vector,
      expect: { canonicalBase64: vector.canonicalBase64, sha256: vector.sha256, accepted: vector.accepted, reject: vector.reject },
    }))]]);
  }
  if (data.groups === null || typeof data.groups !== 'object' || Array.isArray(data.groups)) return null;
  const groups = new Map();
  for (const [group, cases] of Object.entries(data.groups)) {
    groups.set(group, Array.isArray(cases) ? cases.filter((entry) => entry !== null && typeof entry === 'object') : []);
  }
  return groups;
}

function countConsumers(cases) {
  const counts = {};
  for (const consumer of CONSUMERS) {
    const count = cases.filter((entry) => Array.isArray(entry.consumers) && entry.consumers.includes(consumer)).length;
    if (count > 0) counts[consumer] = count;
  }
  return counts;
}

function unsafeNumbers(value, at, found = []) {
  if (typeof value === 'number' && !Number.isSafeInteger(value)) found.push(`${at}: ${value}`);
  else if (Array.isArray(value)) value.forEach((item, index) => unsafeNumbers(item, `${at}[${index}]`, found));
  else if (value !== null && typeof value === 'object') {
    for (const [key, child] of Object.entries(value)) unsafeNumbers(child, `${at}.${key}`, found);
  }
  return found;
}

// ---------------------------------------------------------------------------
// Command lines, read back (section 15.2).
//
// A line is decoded the way the program at the other end reads it, by code
// written here from the rules of that program and not from the serialiser: a
// POSIX shell's single quotes (which the dispatcher shares), the C runtime's
// reading of a Windows command line after cmd strips its @, PowerShell's
// single-quoted strings with its five quote characters. The decoded words must
// be the argv, and every rule a grammar exists for must hold of the line, so a
// serialiser that drops the @ or leaves a tilde bare fails here even when its
// own vectors agree with it.
// ---------------------------------------------------------------------------

// What a POSIX shell, zsh and fish included, read as plain characters outside quotes.
const SHELL_PLAIN = /^[A-Za-z0-9._/:@+,=-]$/;

/** A POSIX line as sh reads it: words separated by one space, each made of plain characters, '...' runs and \'. */
function decodePosixLine(line) {
  const words = [];
  const runs = [];
  let index = 0;
  while (index < line.length) {
    let word = '';
    let bare = true;
    let first = true;
    while (index < line.length && line[index] !== ' ') {
      const character = line[index];
      if (character === "'") {
        const close = line.indexOf("'", index + 1);
        if (close < 0) return { error: 'a quoted run is never closed' };
        runs.push(line.slice(index + 1, close));
        word += line.slice(index + 1, close);
        bare = false;
        index = close + 1;
      } else if (character === '\\' && line[index + 1] === "'") {
        word += "'";
        bare = false;
        index += 2;
      } else if (SHELL_PLAIN.test(character)) {
        // zsh expands a word that starts with = to the path of a command.
        if (first && character === '=') return { error: 'a word starts with an unquoted =' };
        word += character;
        index += 1;
      } else {
        return { error: `${JSON.stringify(character)} stands outside quotes, where a shell reads it as syntax` };
      }
      first = false;
    }
    // sh reads an unquoted first word with = in it as a variable assignment.
    if (words.length === 0 && bare && word.includes('=')) return { error: 'the first word is an unquoted assignment' };
    words.push(word);
    if (index < line.length) {
      index += 1;
      if (index === line.length || line[index] === ' ') return { error: 'words are not separated by exactly one space' };
    }
  }
  return { words, runs };
}

/** A cmd line: @, then double-quoted elements separated by one space, read with the C runtime's backslash rule. */
function decodeCmdLine(line) {
  if (!line.startsWith('@')) return { error: 'a cmd line starts with @, so cmd /c runs it the same whether or not sshd quoted it' };
  if (/[%!]/.test(line)) return { error: 'cmd expands % and ! even inside double quotes' };
  const body = line.slice(1);
  const words = [];
  let index = 0;
  while (index < body.length) {
    if (body[index] !== '"') return { error: 'every element of a cmd line is double-quoted' };
    index += 1;
    let word = '';
    for (;;) {
      let slashes = 0;
      while (body[index] === '\\') {
        slashes += 1;
        index += 1;
      }
      if (index >= body.length) return { error: 'a quoted element is never closed' };
      if (body[index] === '"') {
        // 2n backslashes and a quote: n backslashes, and the quote closes.
        if (slashes % 2 === 1) return { error: 'an element carries an escaped double quote' };
        word += '\\'.repeat(slashes / 2);
        index += 1;
        break;
      }
      word += '\\'.repeat(slashes) + body[index];
      index += 1;
    }
    words.push(word);
    if (index < body.length) {
      if (body[index] !== ' ' || body[index + 1] !== '"') return { error: 'elements are not separated by exactly one space' };
      index += 1;
    }
  }
  return { words };
}

const POWERSHELL_QUOTES = new Set(["'", '\u2018', '\u2019', '\u201a', '\u201b']);

/** A PowerShell line: the call operator, then single-quoted strings in which any of five quote characters doubled is one. */
function decodePowershellLine(line) {
  if (!line.startsWith('& ')) return { error: 'a PowerShell line starts with the call operator and one space' };
  const body = line.slice(2);
  const words = [];
  let index = 0;
  while (index < body.length) {
    if (!POWERSHELL_QUOTES.has(body[index])) return { error: 'every element of a PowerShell line is single-quoted' };
    index += 1;
    let word = '';
    for (;;) {
      if (index >= body.length) return { error: 'a quoted element is never closed' };
      const character = body[index];
      if (POWERSHELL_QUOTES.has(character)) {
        // The tokenizer reads two quote characters in a row as one, the second.
        if (POWERSHELL_QUOTES.has(body[index + 1])) {
          word += body[index + 1];
          index += 2;
          continue;
        }
        index += 1;
        break;
      }
      word += character;
      index += 1;
    }
    words.push(word);
    if (index < body.length) {
      if (body[index] !== ' ' || index + 1 === body.length) return { error: 'elements are not separated by exactly one space' };
      index += 1;
    }
  }
  return { words };
}

/** What each refusal code claims about the element it names, restated from section 15.2. */
const ELEMENT_REFUSALS = {
  'empty-argument': (value) => value === '',
  'control-character': (value) => /[\u0000-\u001f\u007f]/.test(value),
  'posix-backslash': (value) => value.includes('\\\\') || value.includes("\\'") || value.endsWith('\\'),
  'argument-too-long': (value) => Buffer.byteLength(value, 'utf8') > 4096,
  'double-quote': (value) => value.includes('"'),
  percent: (value) => value.includes('%'),
  exclamation: (value) => value.includes('!'),
  'trailing-backslash': (value) => value.endsWith('\\'),
  'double-space': (value) => value.includes('  '),
  'stop-parsing': (value) => value === '--%',
};

function checkCommandLine(where, entry) {
  const { grammar, argv } = entry.input ?? {};
  const { line, refusal, index } = entry.expect ?? {};
  if (typeof line !== 'string') {
    // A refusal names the element it is about, and that element has to show
    // what the code says; the elements before it must not.
    if (index !== null) {
      const holds = ELEMENT_REFUSALS[refusal];
      if (holds === undefined || typeof argv?.[index] !== 'string' || !holds(argv[index])) {
        fail(where, `the refusal ${refusal} names element ${index}, ${JSON.stringify(argv?.[index])}, which does not show it`);
      } else {
        pass();
      }
    } else if (refusal === 'no-command' && argv.length !== 0) fail(where, 'no-command is for an empty argv');
    else if (refusal === 'too-many-arguments' && argv.length <= 64) fail(where, 'too-many-arguments is for more than 64 elements');
    else pass();
    return;
  }
  const decoded = grammar === 'cmd' ? decodeCmdLine(line)
    : grammar === 'powershell' ? decodePowershellLine(line)
      : decodePosixLine(line);
  if (decoded.error) {
    fail(where, `the ${grammar} line does not read back: ${decoded.error}`);
    return;
  }
  if (!jsonEqual(decoded.words, argv)) {
    fail(where, `the ${grammar} line reads back as ${JSON.stringify(decoded.words)}, not the argv ${JSON.stringify(argv)}`);
    return;
  }
  const problems = [];
  if (grammar === 'posix') {
    // fish reads \\ and \' inside single quotes as escapes, so neither may
    // appear in a run, and a run may not end in a backslash.
    if (decoded.runs.some((run) => run.includes('\\\\') || run.endsWith('\\'))) problems.push('a quoted run holds a sequence fish reads as an escape');
  }
  if (grammar === 'dispatch') {
    if (Buffer.byteLength(line, 'utf8') > 16384) problems.push('the line is over the 16384 bytes the dispatcher reads');
    if (decoded.words.length > 64) problems.push('the line has more than the 64 words the dispatcher reads');
    if (decoded.words.some((word) => Buffer.byteLength(word, 'utf8') > 4096)) problems.push('a word is over the 4096 bytes the dispatcher reads');
  }
  if (grammar === 'powershell' && decoded.words.some((word) => word.includes('"') || word.endsWith('\\') || word.includes('  '))) {
    problems.push('an element holds what PowerShell native-argument passing mangles');
  }
  if (problems.length > 0) fail(where, problems.join('; '));
  else pass();
}

const ROUTE_LEVEL_KINDS = new Set(['noRoute', 'launchFailed', 'hostUnreachable', 'connectionRefused',
  'authenticationFailed', 'hostKeyChanged', 'hostKeyUnknown', 'timedOut', 'linkLost']);
const isBlank = (text) => /^[\t\n\r ]*$/.test(text);

/** Whether stdout holds a JSON object from its first { to its last }, decided here again. */
function holdsReply(stdout) {
  const from = stdout.indexOf('{');
  const to = stdout.lastIndexOf('}');
  if (from < 0 || to <= from) return false;
  try {
    const value = JSON.parse(stdout.slice(from, to + 1));
    return value !== null && typeof value === 'object' && !Array.isArray(value);
  } catch {
    return false;
  }
}

/**
 * The safety claims a classification makes, checked against its evidence: a
 * reply is a reply exactly when stdout holds one, and `never` is only claimed
 * when something shows the command cannot have run. That second claim is the
 * one a mutation is resent on, so it is the one worth checking twice.
 */
function checkClassification(where, entry) {
  const evidence = entry.input ?? {};
  const expect = entry.expect ?? {};
  const problems = [];
  if ((expect.outcome === 'reply') !== holdsReply(evidence.stdout ?? '')) problems.push('the outcome does not follow from whether stdout holds a JSON object');
  if ((expect.outcome === 'reply') !== (expect.dispatch === 'acknowledged')) problems.push('only a reply is acknowledged');
  if (expect.outcome === 'failure' && expect.routeLevel !== ROUTE_LEVEL_KINDS.has(expect.kind)) problems.push(`routeLevel is ${expect.routeLevel} for ${expect.kind}`);
  if (expect.dispatch === 'never') {
    const proven = evidence.launchFailed === true
      || (evidence.exitCode === 255 && isBlank(evidence.stdout) && !isBlank(evidence.sshLog))
      || ([126, 127, 9009].includes(evidence.exitCode) && isBlank(evidence.stdout));
    if (!proven || evidence.timedOut === true && !evidence.launchFailed) problems.push('never is claimed without evidence that the command cannot have run');
  }
  if (problems.length > 0) fail(where, problems.join('; '));
  else pass();
}

/** A walk may move a mutation on only from an attempt that was never dispatched. */
function checkWalk(where, entry) {
  const { safety, attempts } = entry.input ?? {};
  const { sent, ending } = entry.expect ?? {};
  const problems = [];
  if (!sent.every((value, position) => position === 0 || value > sent[position - 1])) problems.push('attempts are not made in order');
  if (safety === 'mutation') {
    for (const position of sent.slice(0, -1)) {
      if (attempts[position]?.result?.dispatch !== 'never') problems.push(`attempt ${position} may have run and a later one was made anyway`);
    }
  }
  if (ending === 'reply' && attempts[sent.at(-1)]?.result?.outcome !== 'reply') problems.push('a walk that ends in a reply ends on one');
  if (problems.length > 0) fail(where, problems.join('; '));
  else pass();
}

// ---------------------------------------------------------------------------
// Document acceptance, against the schema (section 9.7).
// ---------------------------------------------------------------------------

/** Whether any argv item of a document is over 4096 UTF-8 bytes: the one guard a schema cannot count. */
function argvOverBytes(document) {
  return (Array.isArray(document?.machines) ? document.machines : []).some((machine) =>
    (Array.isArray(machine?.systems) ? machine.systems : []).some((system) =>
      (Array.isArray(system?.agent) ? system.agent : []).some((item) => typeof item === 'string' && Buffer.byteLength(item, 'utf8') > 4096)));
}

function checkAcceptanceCase(where, entry) {
  const { intake, text } = entry.input ?? {};
  const expect = entry.expect ?? {};
  const problems = [];
  for (const key of ['errors', 'warnings']) {
    const list = expect[key] ?? [];
    if (!list.every((code, position) => position === 0 || code > list[position - 1])) problems.push(`${key} are not sorted and unique`);
  }
  if (expect.valid !== (expect.errors.length === 0)) problems.push('valid is not "no errors"');
  if (expect.errors.length > 0 && expect.warnings.length > 0) problems.push('warnings are only reported for a document without errors');
  if ((entry.tags ?? []).includes('agent-enforced') && expect.valid) problems.push('an agent-enforced case must be refused, because agent 3.1.0 refuses it');

  const bytes = Buffer.from(text, 'utf8');
  const refused = rejectionAgain(bytes) !== null;
  const keep = intake === 'applied' ? !refused : expect.valid;
  if (expect.keep !== keep) problems.push(`keep is ${expect.keep}; under ${intake} intake it is ${keep}`);

  // Whether the schema refuses the document, decided by this file's validator.
  // A text section 12 refuses is no document and breaks the schema; local
  // intake first gives a document without an id the identity of section 9.5.
  if (!expect.errors.includes('duplicate-member')) {
    let schemaRefuses = refused;
    let identity = false;
    if (!refused) {
      let document = JSON.parse(canonicalBytesAgain(bytes).toString('utf8'));
      const block = document.controller;
      identity = intake === 'local' && (!('controller' in document)
        || (block !== null && typeof block === 'object' && !Array.isArray(block) && (block.id === undefined || block.id === null)));
      if (identity) document = { ...document, controller: { ...(block ?? {}), id: 'setup-assigned', revision: 1, lineage: [] } };
      schemaRefuses = validate(registry.get('controller-document.schema.json'), document, { file: 'controller-document.schema.json' }).errors.length > 0
        || argvOverBytes(document);
    }
    if (expect.errors.includes('schema') !== schemaRefuses) {
      problems.push(`controller-document.schema.json ${schemaRefuses ? 'refuses' : 'accepts'} the document and the case ${expect.errors.includes('schema') ? 'says' : 'does not say'} schema`);
    }
    if (expect.assignsIdentity !== identity) problems.push(`assignsIdentity is ${expect.assignsIdentity} and section 9.5 gives ${identity}`);
  }
  if (problems.length > 0) fail(where, problems.join('; '));
  else pass();
}

// ---------------------------------------------------------------------------
// Launch values (section 9.9).
// ---------------------------------------------------------------------------

/** Section 15.1 without a restricted session, restated: the grammar a launch tuple records. */
function grammarAgain(system, length) {
  if (system.restricted === true) return length === 2 ? 'dispatch' : 'refused:restricted-agent-shape';
  if (system.shell === undefined || system.shell === null) return system.platform === 'windows' ? 'cmd' : 'posix';
  return ['posix', 'cmd', 'powershell'].includes(system.shell) ? system.shell : 'refused:unknown-shell';
}

/** The record that approves every launch value of a document, built here apart from tools/launch.mjs. */
function approvalsAgain(document) {
  const record = { schema: 1, seededFrom: 'none', machines: {} };
  const isObject = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);
  for (const machine of Array.isArray(document.machines) ? document.machines : []) {
    if (!isObject(machine) || typeof machine.id !== 'string' || Object.hasOwn(record.machines, machine.id)) continue;
    const entry = { systems: {} };
    for (const system of Array.isArray(machine.systems) ? machine.systems : []) {
      if (!isObject(system) || typeof system.id !== 'string' || Object.hasOwn(entry.systems, system.id)) continue;
      const { agent } = system;
      if (!Array.isArray(agent) || agent.length === 0 || !agent.every((item) => typeof item === 'string')) continue;
      entry.systems[system.id] = { agent, grammar: grammarAgain(system, agent.length) };
    }
    if (isObject(machine.ssh) && typeof machine.ssh.identityFile === 'string' && machine.ssh.identityFile !== '') entry.identityFile = machine.ssh.identityFile;
    record.machines[machine.id] = entry;
  }
  const repo = isObject(document.appUpdates) ? document.appUpdates.githubRepo : undefined;
  if (typeof repo === 'string' && repo !== '' && repo !== 'x1f4r/legion-control') record.githubRepo = repo;
  if (isObject(document.local) && document.local.enabled !== false) {
    record.local = { agent: typeof document.local.agent === 'string' && document.local.agent !== '' ? document.local.agent : null };
  }
  return record;
}

function checkLaunchCase(where, entry, reference) {
  const { heldText, offeredText } = entry.input ?? {};
  const problems = [];
  // The agent stores the held document and is pushed the offered one, so both
  // must be documents section 9.7 accepts, and the offered one must descend
  // from the held one.
  for (const [label, text] of [['heldText', heldText], ['offeredText', offeredText]]) {
    if (text === null) continue;
    const document = JSON.parse(text);
    if (validate(registry.get('controller-document.schema.json'), document, { file: 'controller-document.schema.json' }).errors.length > 0) {
      problems.push(`${label} breaks controller-document.schema.json`);
    }
    const accepted = checkDocument(text, { intake: 'peer' });
    if (!accepted.valid) problems.push(`${label} is refused by section 9.7: ${accepted.errors.map((problem) => problem.code).join(', ')}`);
  }
  const offered = JSON.parse(offeredText);
  if (heldText !== null) {
    const held = JSON.parse(heldText);
    const heldHash = crypto.createHash('sha256').update(canonicalBytesAgain(Buffer.from(heldText, 'utf8'))).digest('hex');
    if (offered.controller?.id !== held.controller?.id || !(offered.controller?.revision > held.controller?.revision)
      || !(offered.controller?.lineage ?? []).includes(heldHash)) {
      problems.push('offeredText does not descend from heldText, so the agent would refuse it before reading its launch values');
    }
    // What agent 3.1.0 answers a restricted push, from a record built here.
    const refused = reference.launchReview(approvalsAgain(held), reference.launchProjection(offered)).length > 0;
    if (refused !== entry.expect?.restrictedRefused) problems.push(`with the held document's own values approved, a restricted push is ${refused ? '' : 'not '}refused, and the case says restrictedRefused ${entry.expect?.restrictedRefused}`);
  } else if (entry.expect?.restrictedRefused !== false) {
    problems.push('with nothing held, the ordinary rules apply and a restricted push is not refused for launch values');
  }
  if (problems.length > 0) fail(where, problems.join('; '));
  else pass();
}

// ---------------------------------------------------------------------------
// Setup reconciliation (section 9.5).
//
// The rules are a table, and the one independent statement of a table is the
// table typed again by a person: every case's hand-written want, which
// generation refuses to disagree with. What is checked here holds of any
// answer whatever the rule: the shape each decision has, that an automatic
// write only ever follows strict descent, that no guardrail lets a second
// request or a held push out, and that only a refusal saying what the machine
// holds is filed.
// ---------------------------------------------------------------------------

const SENDING_DECISIONS = ['push', 'readMeta', 'fetch'];
const DECISION_REASONS = {
  push: ['machine-empty', 'machine-unidentified', 'machine-behind'],
  unknown: ['no-local', 'local-unidentified'],
};

function checkReconcileCase(where, group, entry) {
  const input = entry.input ?? {};
  const expect = entry.expect ?? {};
  const problems = [];
  if (group === 'decide') {
    const remote = input.status?.controller?.hash ?? null;
    if (!(DECISION_REASONS[expect.decision] ?? [null]).includes(expect.reason)) problems.push(`${expect.decision} never comes with the reason ${expect.reason}`);
    if ((expect.expect !== null) !== (expect.decision === 'fetch')) problems.push('expect is set for a fetch and for nothing else');
    if (expect.decision === 'fetch') {
      const filed = Object.hasOwn(input.metaByHash ?? {}, remote) ? input.metaByHash[remote] : null;
      if (expect.expect !== remote) problems.push('a fetch expects the hash the status reported');
      if (!(filed?.lineage ?? []).includes(input.local?.hash)) problems.push('a fetch whose filed lineage does not hold the applied hash is not strict descent');
    }
    if (expect.reason === 'machine-behind' && !(input.local?.lineage ?? []).includes(remote)) problems.push('a push to a machine whose hash is not an ancestor is not strict descent');
    if (expect.decision !== 'unsupported' && !(input.status?.contract >= 3 && input.status?.controller)) problems.push('only a contract 3 machine that reports its copy takes part');
  } else if (group === 'admit') {
    const { expected = {}, fetched = {} } = input;
    if ((expect.admission === 'headMoved') !== (input.currentLocalHash !== expected.localHash)) problems.push('headMoved is exactly an applied hash that changed during the read');
    if (expect.admission === 'adopt' && !(fetched.hash === expected.remoteHash && input.accepted === true
      && fetched.id === expected.localId && (fetched.lineage ?? []).includes(expected.localHash))) {
      problems.push('adopted bytes are not the expected, accepted descendant of the applied document');
    }
  } else if (group === 'gate') {
    const state = input.state ?? {};
    const failed = Object.hasOwn(state.failedAt ?? {}, input.localHash) ? state.failedAt[input.localHash] : null;
    if ((expect.action === 'none') !== !SENDING_DECISIONS.includes(input.decision)) problems.push('only push, readMeta and fetch send anything');
    if ((expect.reason === null) !== (expect.action !== 'hold')) problems.push('a hold, and only a hold, has a reason');
    if (expect.action === 'act') {
      if (state.inFlight) problems.push('a second request while one is in flight');
      if (failed !== null && input.now - failed < 60000) problems.push('a request inside the 60 s back-off');
      if (input.decision === 'push' && (state.localInvalid || state.localMutation || state.runningOperations > 0
        || Object.hasOwn(state.held ?? {}, input.localHash))) {
        problems.push('a push let out past a guardrail');
      }
    }
  } else if (group === 'pushReply') {
    const { reply, localHash } = input;
    if ((expect.metaHash !== null) !== (expect.effect === 'fileMeta')) problems.push('metaHash is set for fileMeta and for nothing else');
    if (expect.effect === 'fileMeta' && (!['stale-revision', 'controller-conflict'].includes(reply?.reasonCode) || expect.metaHash !== reply?.current?.hash)) {
      problems.push('only the current of a stale-revision or controller-conflict refusal is filed, under its own hash');
    }
    if (expect.effect === 'clear' && !(reply?.ok === true && reply.hash === localHash)) problems.push('only a reply that holds the applied hash clears a failure');
    if (reply === null && expect.effect !== 'fail') problems.push('a push whose reply was never read is a failure');
  }
  if (problems.length > 0) fail(where, problems.join('; '));
  else pass();
}

// ---------------------------------------------------------------------------
// Merging (section 9.8).
// ---------------------------------------------------------------------------

/** Whether string a comes before b in code-point order, compared here apart from the reference. */
function beforeByCodePoint(a, b) {
  const x = Array.from(a, (character) => character.codePointAt(0));
  const y = Array.from(b, (character) => character.codePointAt(0));
  for (let index = 0; index < Math.min(x.length, y.length); index += 1) {
    if (x[index] !== y[index]) return x[index] < y[index];
  }
  return x.length < y.length;
}

const hashAgain = (text) => crypto.createHash('sha256').update(canonicalBytesAgain(Buffer.from(text, 'utf8'))).digest('hex');

/**
 * The section 9.7 rules mine may break and still be merged: the ones about how
 * parts that are well formed on their own refer to each other. Written out
 * here rather than imported, so a reference that let a structural error into a
 * merge would disagree with this list.
 */
const RELATIONSHIP_RULES = ['unknown-site', 'helper-self', 'helper-unknown', 'helper-alias', 'helper-cycle'];

function checkMergeCase(where, entry) {
  const { baseText, mineText, theirsText, mineHash, theirsHash, writer = {} } = entry.input ?? {};
  const { differences = [], resultText, error } = entry.expect ?? {};
  const problems = [];
  if (hashAgain(mineText) !== mineHash) problems.push('mineHash is not the canonical hash of mineText');
  if (hashAgain(theirsText) !== theirsHash) problems.push('theirsHash is not the canonical hash of theirsText');
  for (const [label, text] of [['baseText', baseText], ['theirsText', theirsText]]) {
    if (text !== null && !checkDocument(text, { intake: 'peer' }).valid) problems.push(`${label} is refused by section 9.7, and a theirs or a base is merged only when peer intake accepts it`);
  }
  // Mine is the applied document, which applied intake keeps with its errors.
  // Only an error that leaves its entries undefined keeps it out of a merge.
  const structural = checkDocument(mineText, { intake: 'applied' }).errors.filter((problem) => !RELATIONSHIP_RULES.includes(problem.code));
  if (structural.length > 0) {
    problems.push(`mineText breaks section 9.7 (${structural.map((problem) => problem.code).join(', ')}), and a mine with an error other than ${RELATIONSHIP_RULES.join(', ')} is not merged`);
  }
  const keys = differences.map((difference) => difference.entry);
  if (!keys.every((key, index) => index === 0 || beforeByCodePoint(keys[index - 1], key))) problems.push('differences are not listed once each in code-point order of their keys');
  if (differences.some((difference) => (difference.kind === 'noBase') !== (baseText === null))) problems.push('noBase is the kind of every difference of a merge without a base, and of no other');
  if ((resultText === null) !== (error !== null)) problems.push('a merge either writes a document or names the rule it breaks');
  if (resultText !== null) {
    const verdict = checkDocument(resultText, { intake: 'peer' });
    if (!verdict.valid) problems.push(`the merged document is refused by section 9.7: ${verdict.errors.map((problem) => problem.code).join(', ')}`);
    const result = JSON.parse(resultText).controller ?? {};
    const mine = JSON.parse(mineText).controller ?? {};
    const theirs = JSON.parse(theirsText).controller ?? {};
    const lineage = Array.isArray(result.lineage) ? result.lineage : [];
    if (result.id !== mine.id) problems.push('the merged document does not keep mine\'s setup id');
    if (result.revision !== Math.max(mine.revision ?? 0, theirs.revision ?? 0) + 1) problems.push('the merged revision is not one more than the higher of the two');
    if (result.updatedAt !== writer.now || result.source !== writer.source || result.device !== writer.device) problems.push('updatedAt, source and device are not the writer\'s');
    if (lineage[0] !== mineHash || lineage[1] !== theirsHash || lineage.length > 32 || new Set(lineage).size !== lineage.length) {
      problems.push('the merged lineage does not start with both heads, or holds a hash twice, or is longer than 32');
    }
  }
  if (problems.length > 0) fail(where, problems.join('; '));
  else pass();
}

/** A JSON number lexeme as an exact BigInt mantissa and power of ten, read here apart from the reference. */
function decimalAgain(lexeme) {
  const match = /^(-?)([0-9]+)(?:\.([0-9]+))?(?:[eE]([+-]?[0-9]+))?$/.exec(lexeme);
  const fraction = match[3] ?? '';
  return { mantissa: BigInt(`${match[1]}${match[2]}${fraction}`), exponent: Number(match[4] ?? 0) - fraction.length };
}

function sameDecimal(a, b) {
  const x = decimalAgain(a);
  const y = decimalAgain(b);
  if (x.mantissa === 0n || y.mantissa === 0n) return x.mantissa === y.mantissa;
  const low = Math.min(x.exponent, y.exponent);
  return x.mantissa * 10n ** BigInt(x.exponent - low) === y.mantissa * 10n ** BigInt(y.exponent - low);
}

// ---------------------------------------------------------------------------
// Wake (sections 10.1 and 10.2).
// ---------------------------------------------------------------------------

const OCTET_AGAIN = '(?:25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])';
const IPV4_AGAIN = new RegExp(`^${OCTET_AGAIN}(?:\\.${OCTET_AGAIN}){3}$`);
const DOTTED_PREFIX_AGAIN = new RegExp(`^${OCTET_AGAIN}(?:\\.${OCTET_AGAIN}){0,2}\\.?$`);
const CIDR_PREFIX_AGAIN = new RegExp(`^(${OCTET_AGAIN}(?:\\.${OCTET_AGAIN}){3})/(8|9|[12][0-9]|3[0-2])$`);

/** An IPv4 address as a number, by arithmetic rather than the reference's bit operations. */
const ipv4Again = (text) => text.split('.').reduce((value, part) => value * 256 + Number(part), 0);

/** Whether a section 10.1 prefix holds a dotted-quad address; null when the text is no prefix. */
function prefixHoldsAgain(prefix, address) {
  if (DOTTED_PREFIX_AGAIN.test(prefix)) {
    const octets = prefix.replace(/\.$/, '').split('.');
    return IPV4_AGAIN.test(address) && octets.every((octet, index) => address.split('.')[index] === octet);
  }
  const cidr = CIDR_PREFIX_AGAIN.exec(prefix);
  if (cidr === null) return null;
  const block = 2 ** (32 - Number(cidr[2]));
  const network = ipv4Again(cidr[1]);
  if (network % block !== 0) return null;
  return IPV4_AGAIN.test(address) && Math.floor(ipv4Again(address) / block) === network / block;
}

function checkWakePlan(where, entry, fixture) {
  const problems = [];
  const { documentText, target } = entry.input ?? {};
  const expect = entry.expect ?? {};
  // Every plan runs against the one setup clients can also load as a fixture.
  if (!jsonEqual(JSON.parse(documentText), fixture)) problems.push('documentText is not fixtures/controller-document.wake-defaults.json');
  if (!checkDocument(documentText, { intake: 'peer' }).valid) problems.push('documentText is refused by section 9.7');
  const document = JSON.parse(documentText);
  const siteIds = (document.sites ?? []).map((site) => site.id);
  const machine = (document.machines ?? []).find((candidate) => candidate.id === target);
  if (!expect.matching.every((id, index) => siteIds.includes(id) && (index === 0 || siteIds.indexOf(expect.matching[index - 1]) < siteIds.indexOf(id)))) {
    problems.push('matching is not a list of sites in document order');
  }
  if (expect.effective !== null && !siteIds.includes(expect.effective)) problems.push('the effective site is not in the setup');
  const directSteps = expect.steps.filter((step) => step.kind === 'direct');
  const directSkips = expect.skipped.filter((skip) => skip.kind === 'direct');
  if (directSteps.length + directSkips.length !== 1) problems.push('the direct step is either planned or skipped, exactly once');
  for (const step of directSteps) {
    if (!step.broadcast.every((address) => IPV4_AGAIN.test(address)) || !step.ports.every((port) => port >= 1 && port <= 65535) || step.repeats !== 3) {
      problems.push('a direct step sends three packets to dotted IPv4 addresses on ports from 1 to 65535');
    }
  }
  if (directSteps.length > 0 && typeof machine?.wake?.mac !== 'string') problems.push('a direct step for a machine with no MAC address');
  const list = machine?.wake?.helpers?.length > 0 ? machine.wake.helpers : machine?.wake?.helper === undefined ? [] : [machine.wake.helper];
  const nothing = expect.skipped.some((skip) => skip.kind === 'helpers');
  if (nothing !== (list.length === 0 && directSteps.length === 0)) problems.push('"helpers: none" is said exactly when there is no direct step and no helper at all');
  if (expect.steps.filter((step) => step.kind === 'helper').length + expect.skipped.filter((skip) => skip.kind === 'helper').length !== list.length) {
    problems.push('every helper entry is either a step or skipped, exactly once');
  }
  for (const id of expect.offerWakeFirst) {
    const helper = (document.machines ?? []).find((candidate) => candidate.id === id);
    if (!expect.skipped.some((skip) => skip.machine === id && skip.reason === 'asleep') || helper?.wake === null || typeof helper?.wake !== 'object') {
      problems.push(`${id} is offered to be woken first, and only an asleep helper with a wake block of its own is`);
    }
  }
  if (problems.length > 0) fail(where, problems.join('; '));
  else pass();
}

/**
 * The checks each family gets from code that is not its reference. A family
 * the index lists and this table does not is a failure: a vector checked only
 * by the code that produced it checks nothing at all.
 */
const INDEPENDENT = {
  hash: (where, data) => checkHashVectors(data),

  // Each value against the schema definition the group is named after, through
  // this file's own validator rather than the reference's regular expression.
  identifiers: (where, data, groups) => {
    for (const [group, cases] of groups) {
      const schema = { $ref: `common.schema.json#/$defs/${group}` };
      for (const entry of cases) {
        const valid = validate(schema, entry.input?.value, { file: 'common.schema.json' }).errors.length === 0;
        if (valid !== entry.expect?.valid) {
          fail(`${where}/${group}/${entry.name}`, `common.schema.json#/$defs/${group} says ${valid ? 'valid' : 'invalid'} and the vector says ${entry.expect?.valid ? 'valid' : 'invalid'}`);
        } else {
          pass();
        }
      }
    }
  },

  // The reducer is also held to every outcome a fixture states, in pass 3. A
  // run in which nothing was compared there would make that check vacuous.
  outcome: (where) => {
    if (outcomesCompared === 0) fail(where, 'no fixture outcome was compared with the reducer, so the outcome family was checked by its own reference only');
    else pass();
  },

  transport: (where, data, groups) => {
    for (const entry of groups.get('commandLines') ?? []) checkCommandLine(`${where}/commandLines/${entry.name}`, entry);
    for (const entry of groups.get('classifications') ?? []) checkClassification(`${where}/classifications/${entry.name}`, entry);
    for (const entry of groups.get('walks') ?? []) checkWalk(`${where}/walks/${entry.name}`, entry);
    for (const entry of groups.get('statusBudgets') ?? []) {
      const { remainingMs, attemptsMade } = entry.input ?? {};
      const { attempt, budgetMs } = entry.expect ?? {};
      const at = `${where}/statusBudgets/${entry.name}`;
      if (attempt && !(budgetMs >= 1000 && budgetMs <= 20000 && budgetMs <= Math.max(1000, remainingMs - 5000))) {
        fail(at, `a budget of ${budgetMs} ms with ${remainingMs} ms left breaks the 1000 to 20000 ms range or the 5 s margin`);
      } else if (!attempt && (attemptsMade === 0 || budgetMs !== null)) {
        fail(at, 'only a later attempt may be skipped, and a skipped attempt has no budget');
      } else {
        pass();
      }
    }
  },

  'setup-acceptance': (where, data, groups) => {
    for (const entry of groups.get('documents') ?? []) checkAcceptanceCase(`${where}/documents/${entry.name}`, entry);
  },

  launch: (where, data, groups, reference) => {
    for (const entry of groups.get('review') ?? []) checkLaunchCase(`${where}/review/${entry.name}`, entry, reference);
  },

  'setup-reconcile': (where, data, groups) => {
    for (const [group, cases] of groups) {
      for (const entry of cases) checkReconcileCase(`${where}/${group}/${entry.name}`, group, entry);
    }
  },

  'setup-merge': (where, data, groups) => {
    for (const entry of groups.get('merges') ?? []) checkMergeCase(`${where}/merges/${entry.name}`, entry);
    for (const entry of groups.get('bases') ?? []) {
      const { mineLineage = [], theirsLineage = [], kept = {} } = entry.input ?? {};
      // Kept bytes that hash to their name and that peer intake accepts: a refused revision is
      // skipped like one that is not kept (section 9.8).
      const usable = (hash) => theirsLineage.includes(hash) && typeof kept[hash] === 'string' && hashAgain(kept[hash]) === hash
        && checkDocument(kept[hash], { intake: 'peer' }).errors.length === 0;
      const base = mineLineage.find(usable) ?? null;
      if (base !== entry.expect?.base) fail(`${where}/bases/${entry.name}`, `the newest hash of mine's lineage in theirs' with kept bytes that hash to it and that peer intake accepts is ${base}, and the case says ${entry.expect?.base}`);
      else pass();
    }
    for (const entry of groups.get('numbers') ?? []) {
      const equal = sameDecimal(entry.input?.a, entry.input?.b);
      if (equal !== entry.expect?.equal) fail(`${where}/numbers/${entry.name}`, `${entry.input?.a} and ${entry.input?.b} are ${equal ? 'one number' : 'two numbers'}, and the case says otherwise`);
      else pass();
    }
  },

  wake: (where, data, groups) => {
    for (const entry of groups.get('prefixes') ?? []) {
      const { prefix, addresses = [] } = entry.input ?? {};
      const valid = prefixHoldsAgain(prefix, '0.0.0.0') !== null;
      const contains = addresses.map((address) => valid && prefixHoldsAgain(prefix, address) === true);
      if (valid !== entry.expect?.valid || !jsonEqual(contains, entry.expect?.contains)) {
        fail(`${where}/prefixes/${entry.name}`, `read here, ${JSON.stringify(prefix)} is ${valid ? '' : 'not '}a prefix holding ${JSON.stringify(contains)}, and the case says ${JSON.stringify(entry.expect)}`);
      } else {
        pass();
      }
    }
    for (const entry of groups.get('broadcasts') ?? []) {
      if (IPV4_AGAIN.test(entry.input?.text) !== entry.expect?.valid) fail(`${where}/broadcasts/${entry.name}`, 'whether the entry is a dotted IPv4 address disagrees with the case');
      else pass();
    }
    const fixture = readJson(path.join(FIXTURE_DIR, 'controller-document.wake-defaults.json'));
    for (const entry of groups.get('plans') ?? []) checkWakePlan(`${where}/plans/${entry.name}`, entry, fixture);
    for (const entry of groups.get('readiness') ?? []) {
      const starts = entry.expect?.pollStartsMs ?? [];
      const gaps = starts.map((start, index) => start - (index === 0 ? 0 : starts[index - 1]));
      const shaped = entry.input?.sentAnything
        ? starts[0] === 5000 && gaps.every((gap, index) => index === 0 || (gap >= gaps[index - 1] && gap <= 20000)) && starts.at(-1) <= 120000 && starts.at(-1) + 20000 > 120000
        : starts.length === 0;
      if (!shaped) fail(`${where}/readiness/${entry.name}`, 'polls start 5 s after a send, at growing intervals of at most 20 s, until 120 s; after no send there are none');
      else pass();
    }
  },
};

/**
 * Rules that tie one family to the schemas the fixtures are held to. Each
 * applies once its family is in the index.
 */
function checkCrossFamilyInvariants(families) {
  const transport = families.get('transport');
  if (!transport) return;
  const verdict = (ref, value) => validate({ $ref: ref }, value, { file: 'common.schema.json' }).errors.length === 0;
  for (const entry of transport.groups.get('hosts') ?? []) {
    const where = `vectors/transport.json/hosts/${entry.name}`;
    const guarded = verdict('common.schema.json#/$defs/sshHostGuard', entry.input?.value);
    if (entry.expect?.valid === true && !guarded) fail(where, 'a dialable host must also pass sshHostGuard, or the agent refuses a document every client would dial');
    if ((entry.tags ?? []).includes('injection') && guarded) fail(where, 'an injection host passes sshHostGuard, so a document carrying it would be stored');
    pass();
  }
  for (const entry of transport.groups.get('users') ?? []) {
    const guarded = verdict('common.schema.json#/$defs/sshUser', entry.input?.value);
    if (guarded !== entry.expect?.valid) fail(`vectors/transport.json/users/${entry.name}`, `sshUser says ${guarded} and the vector says ${entry.expect?.valid}`);
    else pass();
  }
  for (const entry of transport.groups.get('ports') ?? []) {
    const guarded = verdict('controller-document.schema.json#/$defs/endpoint/properties/port', entry.input?.value);
    if (guarded !== entry.expect?.valid) fail(`vectors/transport.json/ports/${entry.name}`, `the endpoint port rule says ${guarded} and the vector says ${entry.expect?.valid}`);
    else pass();
  }
}

async function checkVectorFamilies(index) {
  const families = new Map();
  let cases = 0;
  for (const entry of index.families) {
    const where = entry.file;
    const file = path.join(HERE, entry.file);
    if (!fs.existsSync(file) || !registry.has(entry.schema)) continue;

    let data;
    try {
      data = readJson(file);
    } catch (error) {
      fail(where, `does not parse: ${error.message}`);
      continue;
    }
    const { errors } = validate(registry.get(entry.schema), data, { file: entry.schema });
    for (const error of errors) fail(where, error);
    if (errors.length === 0) pass();

    const groups = casesByGroup(entry, data);
    if (groups === null) continue;
    if (entry.layout === 'groups') {
      for (const key of ['family', 'section', 'reference']) {
        if (data[key] !== entry[key]) fail(where, `says ${key} ${JSON.stringify(data[key])} and the index says ${JSON.stringify(entry[key])}`);
      }
    }

    // The counts a consumer compares its own run with.
    const present = [...groups.keys()];
    const listed = entry.groups.map((group) => group.group);
    if (!jsonEqual(present, listed)) fail(where, `holds the groups ${present.join(', ')} and the index lists ${listed.join(', ')}`);
    for (const group of entry.groups) {
      const groupCases = groups.get(group.group) ?? [];
      if (groupCases.length !== group.cases) fail(where, `${group.group} holds ${groupCases.length} cases and the index says ${group.cases}`);
      const counts = countConsumers(groupCases);
      if (!jsonEqual(counts, group.consumers)) {
        fail(where, `${group.group}: the cases name ${JSON.stringify(counts)} and the index says ${JSON.stringify(group.consumers)}; a consumer that runs a different number of cases than the index says fails`);
      } else {
        pass();
      }
    }

    // One name, one case, across the whole family.
    const seen = new Set();
    for (const [group, groupCases] of groups) {
      for (const entryCase of groupCases) {
        if (seen.has(entryCase.name)) fail(`${where}/${group}/${entryCase.name}`, 'the name is used twice in the family');
        seen.add(entryCase.name);
        const unsafe = unsafeNumbers(entryCase.expect, 'expect');
        if (unsafe.length > 0) fail(`${where}/${group}/${entryCase.name}`, `not a safe integer at ${unsafe.join(', ')}; four languages compare these numbers as integers`);
      }
    }

    let reference;
    try {
      reference = await import(pathToFileURL(path.join(HERE, entry.reference)).href);
    } catch (error) {
      fail(where, `its reference ${entry.reference} does not load: ${error.message}`);
      continue;
    }
    if (!Array.isArray(reference.FAMILIES) || !reference.FAMILIES.includes(entry.family) || typeof reference.run !== 'function') {
      fail(where, `${entry.reference} does not export run() with ${entry.family} in FAMILIES`);
      continue;
    }
    for (const [group, groupCases] of groups) {
      for (const entryCase of groupCases) {
        cases += 1;
        let actual;
        try {
          actual = reference.run(group, structuredClone(entryCase.input));
        } catch (error) {
          fail(`${where}/${group}/${entryCase.name}`, `the reference throws: ${error.message}`);
          continue;
        }
        if (!jsonEqual(actual, entryCase.expect)) {
          fail(`${where}/${group}/${entryCase.name}`, `${entry.reference} gives ${JSON.stringify(actual)} and the file says ${JSON.stringify(entryCase.expect)}`);
        } else {
          pass();
        }
      }
    }

    const independent = INDEPENDENT[entry.family];
    if (independent === undefined) {
      fail(where, `validate.mjs has no independent check for the ${entry.family} family; a family checked only by its own reference checks nothing`);
    } else {
      independent(where, data, groups, reference);
    }
    families.set(entry.family, { data, groups });
  }
  checkCrossFamilyInvariants(families);
  return { families: families.size, cases };
}

// ---------------------------------------------------------------------------

async function main() {
  loadSchemas();
  if (registry.size === 0) {
    process.stderr.write('contract/schemas holds no schemas\n');
    process.exit(1);
  }

  lintSchemas();

  const index = readJson(INDEX_FILE);
  const loaded = checkFixtures(index);
  for (const [name, data] of loaded) replySemantics(`fixtures/${name}`, data);

  checkCrossReferences(loaded);
  checkSetupHashAgrees();
  const coverage = checkCoverage(index, loaded);

  let vectors = { families: 0, cases: 0 };
  if (!fs.existsSync(VECTOR_INDEX_FILE)) {
    fail('vectors/index.json', 'is missing; it lists every vector family, the hash vectors included (run node contract/tools/generate.mjs)');
  } else {
    let vectorIndex = null;
    try {
      vectorIndex = loadVectorIndex();
    } catch (error) {
      fail('vectors/index.json', `does not parse: ${error.message}`);
    }
    if (vectorIndex !== null && checkVectorIndex(vectorIndex)) vectors = await checkVectorFamilies(vectorIndex);
  }

  const out = process.stdout;
  const documents = index.fixtures.filter((entry) => entry.kind === 'document').length;
  const refused = index.fixtures.filter((entry) => entry.expect === 'invalid').length;
  out.write(
    `contract v3: ${registry.size} schemas, ${index.fixtures.length} fixtures `
    + `(${index.fixtures.length - documents} replies, ${documents} documents, ${refused} of them expected to be refused), `
    + `${coverage.codes}/${coverage.declared} reason codes shown, `
    + `${vectors.families} vector families with ${vectors.cases} cases, ${sink.checks} checks\n`,
  );
  for (const warning of sink.warnings) out.write(`  warning  ${warning}\n`);
  if (sink.failures.length === 0) {
    out.write('everything holds\n');
    return;
  }
  out.write(`\n${sink.failures.length} problem${sink.failures.length === 1 ? '' : 's'}:\n`);
  for (const failure of sink.failures) out.write(`  ${failure}\n`);
  process.exitCode = 1;
}

// Importable so contract/test can exercise the validator itself. A checking tool
// nobody checks is a tool that quietly stops checking.
export {
  validate, resolveRef, loadSchemas, registry, checkControllerDocument, canonicalBytesAgain, isValidUtf8Bytes,
  checkReply, reduceOutcome, schemaForCommand, jsonEqual, loadVectorIndex,
};

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

if (invokedDirectly()) {
  main().catch((error) => {
    process.stderr.write(`contract/validate.mjs failed: ${error.stack ?? error.message}\n`);
    process.exitCode = 1;
  });
}
