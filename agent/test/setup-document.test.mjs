// The setup document's structure, checked by `config set` before anything is
// written (contract section 9.4), in the order that section gives.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { canonicalBytes, canonicalHash, readController, storeController } from '../src/controller.mjs';
import {
  AGENT_ARG_PATTERN, HOST_GUARD_PATTERN, REPO_PATTERN, USER_PATTERN, findRepeatedMember, validateSetupDocument,
} from '../src/setup-document.mjs';
import { REPO, cli, withHome } from './helpers.mjs';

const CONTRACT = path.join(REPO, 'contract');
const read = (relative) => JSON.parse(fs.readFileSync(path.join(CONTRACT, relative), 'utf8'));

const SETUP = 'setup-3f9c2a71-8d04-4e6b-9a53-7c21b0e5d846';

/** A small valid document, with `change` applied to a copy of it. */
function setup({ revision = 1, lineage = [], change = () => {} } = {}) {
  const document = {
    version: 1,
    controller: { id: SETUP, revision, lineage, source: 'cli' },
    machines: [{
      id: 'tower',
      name: 'Tower',
      endpoints: [{ id: 'lan', kind: 'lan', host: 'desk-tower.local', user: 'me', port: 22 }],
      systems: [{ id: 'linux', platform: 'linux', shell: 'posix', agent: ['/usr/bin/node', '/home/me/.legion-control/bin/launcher.mjs'] }],
    }],
  };
  change(document);
  return `${JSON.stringify(document, null, 2)}\n`;
}

const held = (home) => (fs.existsSync(path.join(home, 'controller.json')) ? fs.readFileSync(path.join(home, 'controller.json')) : null);

test('the guard patterns are the strings common.schema.json holds', () => {
  const common = read('schemas/common.schema.json');
  assert.equal(HOST_GUARD_PATTERN, common.$defs.sshHostGuard.pattern);
  assert.equal(USER_PATTERN, common.$defs.sshUser.pattern);
  assert.equal(AGENT_ARG_PATTERN, common.$defs.agentArgItem.pattern);
  assert.equal(REPO_PATTERN, common.$defs.githubRepo.pattern);
});

test('every document fixture the agent enforces is refused, and every valid one is stored', async () => {
  const index = read('fixtures/index.json');
  const documents = index.fixtures.filter((entry) => entry.kind === 'document' && entry.schema === 'controller-document.schema.json');
  const enforced = documents.filter((entry) => entry.expect === 'invalid' && entry.tags.includes('agent-enforced'));
  assert.ok(enforced.length >= 10, 'the corpus carries the agent-enforced documents');
  for (const entry of documents) {
    await withHome(async (home) => {
      const text = fs.readFileSync(path.join(CONTRACT, 'fixtures', entry.file), 'utf8');
      const stored = storeController(text);
      if (entry.expect === 'valid') {
        assert.equal(stored.ok, true, `${entry.file}: ${stored.error}`);
      } else if (entry.tags.includes('agent-enforced')) {
        assert.equal(stored.reasonCode, 'config-invalid', `${entry.file} should be refused`);
        assert.ok(stored.problems.length > 0, entry.file);
        assert.equal(held(home), null, `${entry.file}: nothing was written`);
      }
    });
  }
});

test('a broken document is refused with every problem as a path, and nothing is written', async () => {
  await withHome(async (home) => {
    const text = setup({ change: (document) => {
      document.machines[0].endpoints[0].host = 'victim.example,*';
      document.machines[0].endpoints[0].port = 0;
      document.machines[0].systems[0].agent = ['/usr/bin/node', ''];
    } });
    const refused = storeController(text, { id: SETUP, revision: 1 });
    assert.equal(refused.ok, false);
    assert.equal(refused.reasonCode, 'config-invalid');
    assert.deepEqual(refused.problems.map((problem) => problem.path), [
      'machines[0].endpoints[0].host', 'machines[0].endpoints[0].port', 'machines[0].systems[0].agent[1]',
    ]);
    assert.match(refused.error, /breaks 3 rules .* the first is machines\[0\]\.endpoints\[0\]\.host$/);
    // The argv is named by position, never repeated back.
    assert.ok(refused.problems.every((problem) => !problem.message.includes('/usr/bin/node')));
    assert.equal(held(home), null);
    assert.equal(fs.existsSync(path.join(home, 'controller.meta.json')), false);
  });
});

test('at most fifty problems are listed', async () => {
  await withHome(async () => {
    const text = setup({ change: (document) => {
      document.machines = Array.from({ length: 50 }, (_, index) => ({ id: `m${index}`, endpoints: [{ host: '-oX', port: 0 }] }));
    } });
    const refused = storeController(text);
    assert.equal(refused.reasonCode, 'config-invalid');
    assert.equal(refused.problems.length, 50);
    assert.match(refused.error, /breaks 100 rules/);
  });
});

test('a member written twice is refused before the controller block or anything else is read', async () => {
  await withHome(async (home) => {
    const text = setup().replace('"version": 1,', '"version": 1,\n  "version": 1,');
    assert.deepEqual(findRepeatedMember(canonicalBytes(text).toString('utf8')), { path: 'version' });
    const refused = storeController(text, { id: 'other-id', revision: 9 });
    assert.equal(refused.reasonCode, 'config-invalid', 'the flags disagree too, and are not reached');
    assert.deepEqual(refused.problems, [{ path: 'version', message: 'is written twice in one object, and parsers disagree about which copy counts' }]);
    assert.equal(held(home), null);

    // Names are compared decoded, and nested objects keep their own names.
    const nested = setup().replace('"shell": "posix",', '"shell": "posix",\n "\\u0073hell": "cmd",');
    assert.deepEqual(storeController(nested).problems.map((problem) => problem.path), ['machines[0].systems[0].shell']);
  });
});

test('a member written twice is refused in an ordinary and in a restricted session alike', async () => {
  for (const env of [{}, { LEGIONCTL_RESTRICTED: '1' }]) {
    await withHome(async (home) => {
      const text = setup().replace('"name": "Tower",', '"name": "Tower",\n "name": "Other",');
      const reply = cli(['config', 'set'], { home, stdin: text, env }).payload;
      assert.equal(reply.reasonCode, 'config-invalid', JSON.stringify(env));
      assert.deepEqual(reply.problems.map((problem) => problem.path), ['machines[0].name']);
      assert.equal(held(home), null);
    });
  }
});

test('the checks answer in the order of contract section 9.4', async () => {
  await withHome(async (home) => {
    // Step 1 before everything: bytes that are not UTF-8.
    assert.equal(storeController(Buffer.from([0x7b, 0xff, 0x7d])).reasonCode, 'bad-argument');
    // Step 3: the canonical text parses as a JSON object. A byte order mark is
    // part of no document, so a document saved with one is parsed without it.
    assert.equal(storeController('[]').reasonCode, 'bad-argument');
    assert.equal(storeController('{ not json').reasonCode, 'bad-argument');
    const bom = storeController(`\uFEFF${setup()}`);
    assert.equal(bom.ok, true, bom.error);

    const first = canonicalHash(setup());
    // Step 5 before step 8: a controller block that cannot be read, and flags
    // that disagree with the document, answer before the structure.
    const badBlock = storeController(setup({ revision: 2, lineage: [first], change: (document) => {
      document.controller.source = 'android';
      document.machines[0].endpoints[0].host = 'a b';
    } }));
    assert.equal(badBlock.reasonCode, 'config-invalid');
    assert.deepEqual(badBlock.problems.map((problem) => problem.path), ['controller.source']);
    const flags = storeController(setup({ revision: 2, lineage: [first], change: (document) => { document.machines[0].endpoints[0].host = 'a b'; } }), { id: SETUP, revision: 3 });
    assert.equal(flags.reasonCode, 'bad-argument');

    // Step 6 before step 8: a push this machine would turn away anyway is
    // told why, not what else is wrong with it.
    const second = storeController(setup({ revision: 2, lineage: [first] }));
    assert.equal(second.action, 'stored');
    const stale = storeController(setup({ change: (document) => { document.machines[0].endpoints[0].port = 0; } }));
    assert.equal(stale.reasonCode, 'controller-conflict');
    assert.equal(stale.divergent, true);

    // Step 8: only a push that would be stored is held to the structure.
    const broken = storeController(setup({ revision: 3, lineage: [canonicalHash(setup({ revision: 2, lineage: [first] })), first], change: (document) => { delete document.version; } }));
    assert.equal(broken.reasonCode, 'config-invalid');
    assert.deepEqual(broken.problems.map((problem) => problem.path), ['version']);
  });
});

test('a byte-identical push of an invalid copy an older agent stored is a noop, never re-checked', async () => {
  await withHome(async (home) => {
    // What a 3.0 agent would have stored: a document whose host no client may dial.
    const text = canonicalBytes(setup({ change: (document) => { document.machines[0].endpoints[0].host = 'victim.example,*'; } }));
    const hash = canonicalHash(text);
    fs.writeFileSync(path.join(home, 'controller.json'), text);
    fs.writeFileSync(path.join(home, 'controller.meta.json'), JSON.stringify({
      id: SETUP, revision: 1, updatedAt: new Date().toISOString(), source: 'cli', hash, lineage: [], device: null, bytes: text.length,
    }));
    assert.equal(readController().hash, hash);
    const again = storeController(text, { id: SETUP, revision: 1 });
    assert.equal(again.ok, true, again.error);
    assert.equal(again.action, 'noop');

    // Its descendant is held to the rules the old agent never applied.
    const next = storeController(setup({ revision: 2, lineage: [hash], change: (document) => { document.machines[0].endpoints[0].host = 'victim.example,*'; } }));
    assert.equal(next.reasonCode, 'config-invalid');
    assert.deepEqual(fs.readFileSync(path.join(home, 'controller.json')), text);
  });
});

test('warnings are stored all the same and come back as notes', async () => {
  await withHome(async (home) => {
    const text = setup({ change: (document) => {
      document.machines[0].systems.push({ id: 'windows', platform: 'windows', agent: [] });
      document.machines.push({ id: 'pi', name: 'Beacon', wake: { mac: '02:00:00:00:00:01' } });
    } });
    const reply = cli(['config', 'set'], { home, stdin: text }).payload;
    assert.equal(reply.ok, true, reply.message);
    assert.equal(reply.action, 'stored');
    assert.deepEqual(reply.notes.map((note) => note.split(':')[0]), ['machines[0].systems[1]', 'machines[1].wake']);
    assert.match(reply.notes[0], /tower\/windows has no agent command/);
    assert.equal(validateSetupDocument(JSON.parse(text)).errors.length, 0);
  });
});

test('the helper alias counts as a helper beside an empty helpers list (contract section 9.7)', () => {
  const document = JSON.parse(setup({ change: (doc) => {
    doc.machines[0].wake = { mac: '02:00:00:00:00:01', helper: { machine: 'tower', action: 'wake' }, helpers: [] };
  } }));
  assert.deepEqual(validateSetupDocument(document).errors.map((problem) => problem.code), ['helper-self']);
});
