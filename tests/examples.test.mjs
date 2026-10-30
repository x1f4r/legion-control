import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { normalizeConfig } from '../agent/src/config.mjs';
import { checkDocument } from '../contract/tools/document-rules.mjs';
import { isLaunchText, launchProjection } from '../contract/tools/launch.mjs';
import { GRAMMARS, serialize } from '../contract/tools/transport.mjs';
import { loadSchemas, validate } from '../contract/validate.mjs';

// examples/ is what people copy, so each file in it is held to the rules its kind of file is read
// with: the shared setup to section 9.7 of the contract and its schema, the bindings to theirs, and
// an agent config to what the agent itself accepts. An example that a client or the agent refuses,
// or one that only works by accident, is copied with its fault into every setup made from it.
const repository = path.resolve(import.meta.dirname, '..');
const examples = path.join(repository, 'examples');
const read = (name) => fs.readFileSync(path.join(examples, name));
const json = (name) => JSON.parse(read(name).toString('utf8'));

loadSchemas();
const schemaErrors = (schema, value) => validate({ $ref: schema }, value, { file: schema }).errors;

// The agent configs, each with the platform it is written for and that platform's placeholder base.
// An agent config has no schema in contract/: normalizeConfig is what the agent reads it with, and
// what refuses it.
const AGENT_CONFIGS = {
  'agent-config-helper.json': 'linux',
  'agent-config-linux.json': 'linux',
  'agent-config-mac.json': 'mac',
  'agent-config-minimal.json': 'linux',
  'agent-config-t3-code.json': 'linux',
  'agent-config-windows.json': 'windows',
};
const BASES = { linux: '/home/me/.legion-control', mac: '/Users/me/.legion-control', windows: 'C:\\Users\\me\\.legion-control' };
// The machine of the shared setup each agent config is the config of, where it is one.
const CONFIG_OF = { pi: 'agent-config-helper.json' };

const setup = json('controller-config.json');
const bindings = json('bindings.json');
const machine = (id) => setup.machines.find(each => each.id === id);

// An absolute path for the platform, through no `..`, and nothing in it a shell or the agent would
// expand: a launch value names the same file whichever account an endpoint reaches.
function absolute(platform, value) {
  const shape = platform === 'windows' ? /^[A-Za-z]:\\[^\\]/ : /^\/[^/]/;
  return shape.test(value) && !value.split(/[\\/]/).includes('..') && !/[~$%]/.test(value);
}
const launcher = (platform, value) => absolute(platform, value) && (platform === 'windows' ? /\\bin\\launcher\.mjs$/ : /\/bin\/launcher\.mjs$/).test(value);

// The placeholders the contract's newer fixtures and vectors are written with, for the values
// scripts/check-private-data.mjs cannot tell from real ones: a machine's name, an ssh user and a
// MAC address. 02: is the locally administered prefix, which no network card ships with.
const PLACEHOLDER_MACHINES = new Set(['tower', 'pi', 'nas', 'laptop', 'old-box', 'studio', 'shed', 'tv']);
const PLACEHOLDER_USER = 'me';
const PLACEHOLDER_MAC = /^02:00:00:00:00:0\d$/;

test('every example is one the tests here check', () => {
  const names = fs.readdirSync(examples).filter(name => name.endsWith('.json')).sort();
  assert.deepEqual(names, [...Object.keys(AGENT_CONFIGS), 'bindings.json', 'controller-config.json'].sort());
});

test('the shared setup example passes section 9.7 under peer intake, with every endpoint dialable', () => {
  const result = checkDocument(read('controller-config.json'), { intake: 'peer' });
  assert.deepEqual(result.errors, []);
  assert.equal(result.valid, true);
  assert.equal(result.assignsIdentity, false);
  // Two sites on the same router's default range is the case currentSite exists for, and
  // bindings.json makes that choice. Anything else a client would warn about, endpoint-undialable
  // above all, is a fault of the example.
  assert.deepEqual(result.warnings.map(warning => warning.code), ['shared-prefix'], JSON.stringify(result.warnings, null, 2));
  // document-rules.mjs writes the schema out by hand; the schema itself agrees.
  assert.deepEqual(schemaErrors('controller-document.schema.json', setup), []);
});

test('every system of the shared setup example runs the stable launcher by absolute paths', () => {
  const projection = launchProjection(setup);
  let systems = 0;
  for (const { id: machineId, systems: projected, identityFile } of projection.machines) {
    assert.equal(identityFile, null, `${machineId} names a key path, which belongs in bindings.json`);
    for (const { id: systemId, tuple } of projected) {
      systems += 1;
      const where = `machines[${machineId}].systems[${systemId}]`;
      const { platform } = machine(machineId).systems.find(each => each.id === systemId);
      assert.ok(GRAMMARS.includes(tuple.grammar), `${where} has no grammar to be written in: ${tuple.grammar}`);
      assert.ok(tuple.agent.every(isLaunchText), `${where} holds text a person could not approve`);
      const { line, refusal } = serialize(tuple.grammar, [...tuple.agent, 'status']);
      assert.notEqual(line, null, `${where} cannot be written as a ${tuple.grammar} command line: ${refusal}`);
      assert.equal(tuple.agent.length, 2, `${where} runs ${JSON.stringify(tuple.agent)}`);
      assert.ok(absolute(platform, tuple.agent[0]), `${where} runs the interpreter ${tuple.agent[0]}, which is not an absolute path`);
      assert.ok(launcher(platform, tuple.agent[1]), `${where} runs ${tuple.agent[1]}, which is not <base>/bin/launcher.mjs by an absolute path`);
    }
  }
  assert.equal(systems, 5);
  assert.equal(projection.local, null);
});

test('the bindings example passes its schema and names what the shared setup example has', () => {
  assert.deepEqual(schemaErrors('bindings.schema.json', bindings), []);
  const self = machine(bindings.self.machine);
  assert.ok(self, `bindings.self names ${bindings.self.machine}, which the setup has no machine for`);
  const system = self.systems.find(each => each.id === bindings.self.system);
  assert.ok(system, `bindings.self names the system ${bindings.self.system}, which ${self.id} does not have`);
  for (const id of Object.keys(bindings.machines)) assert.ok(machine(id), `bindings.machines names ${id}, which the setup has no machine for`);
  assert.ok(setup.sites.some(site => site.id === bindings.currentSite), `bindings.currentSite names ${bindings.currentSite}, which the setup has no site for`);
  const [interpreter, module] = bindings.localAgent.argv;
  assert.ok(absolute(system.platform, interpreter), `bindings.localAgent runs the interpreter ${interpreter}, which is not an absolute path`);
  assert.ok(launcher(system.platform, module), `bindings.localAgent runs ${module}, which is not <base>/bin/launcher.mjs by an absolute path`);
});

test('every agent config example loads cleanly on the platform it is written for', () => {
  for (const [name, platform] of Object.entries(AGENT_CONFIGS)) {
    const result = normalizeConfig(json(name), { platform, base: BASES[platform] });
    assert.equal(result.ok, true, `${name}: ${JSON.stringify(result.errors, null, 2)}`);
    assert.deepEqual(result.warnings, [], name);
    assert.deepEqual(result.migrations, [], name);
  }
});

test('an agent config example of a helper holds every wake action the shared setup asks of it', () => {
  for (const [helperId, name] of Object.entries(CONFIG_OF)) {
    const helper = machine(helperId);
    const config = json(name);
    assert.ok(helper, `${name} is the config of ${helperId}, which the setup has no machine for`);
    assert.ok(helper.systems.some(system => system.id === config.system.id), `${name} is for the system ${config.system.id}, which ${helperId} does not have`);
    let asked = 0;
    for (const target of setup.machines) {
      const helpers = [...(target.wake?.helpers ?? []), ...(target.wake?.helper ? [target.wake.helper] : [])];
      for (const { action: id } of helpers.filter(each => each.machine === helperId)) {
        asked += 1;
        const action = config.actions.find(each => each.id === id);
        assert.ok(action?.wol, `${target.id} asks ${helperId} to run ${id}, which ${name} has no wake action for`);
        assert.equal(action.wol.mac.toLowerCase(), target.wake.mac.toLowerCase(), `${name}: ${id} wakes ${action.wol.mac}, not ${target.id}`);
      }
    }
    assert.ok(asked > 0, `nothing in the setup asks ${helperId} to wake anything`);
  }
});

test('the examples name only placeholder machines, users and MAC addresses', () => {
  // A helper, bindings.self and bindings.machines may only name a machine of the setup, which the
  // tests above hold them to, so the setup's own machines are the ones to check.
  for (const { id } of setup.machines) assert.ok(PLACEHOLDER_MACHINES.has(id), `controller-config.json has the machine ${id}, which is not a placeholder name`);
  let seen = 0;
  const visit = (name, where, value) => {
    if (Array.isArray(value)) value.forEach((each, index) => visit(name, `${where}[${index}]`, each));
    else if (value && typeof value === 'object') {
      for (const [key, each] of Object.entries(value)) {
        if (key === 'user' && typeof each === 'string') {
          seen += 1;
          assert.equal(each, PLACEHOLDER_USER, `${name}: ${where}.user is ${each}`);
        } else if (key === 'mac' && typeof each === 'string') {
          seen += 1;
          assert.match(each, PLACEHOLDER_MAC, `${name}: ${where}.mac is ${each}`);
        } else visit(name, `${where}.${key}`, each);
      }
    }
  };
  for (const name of fs.readdirSync(examples).filter(each => each.endsWith('.json'))) visit(name, '$', json(name));
  assert.ok(seen > 0, 'no example names a user or a MAC address');
});
