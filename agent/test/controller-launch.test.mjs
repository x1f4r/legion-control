// A restricted session's `config set` and the launch values it may not
// introduce (contract sections 9.4 and 9.9, finding T1).
//
// Every case holds the same document D0 and pushes a descendant of it the way
// a restricted key would. The machine's own launch values are the approved
// ones: anything else a descendant carries is refused, and nothing is written.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { canonicalHash, readController, storeController } from '../src/controller.mjs';
import { isLauncherMigration, launchProjection } from '../src/launch.mjs';
import { withHome } from './helpers.mjs';

const SETUP = 'setup-3f9c2a71-8d04-4e6b-9a53-7c21b0e5d846';
const ENTRY = '/home/me/.legion-control/agent/src/index.mjs';
const LAUNCHER = '/home/me/.legion-control/bin/launcher.mjs';

const D0 = {
  version: 1,
  controller: { id: SETUP, revision: 1, lineage: [], source: 'mac', device: 'Office MacBook Pro' },
  sites: [{ id: 'home', name: 'Home', lanPrefixes: ['192.168.1.'] }],
  machines: [
    {
      id: 'tower', name: 'Tower', site: 'home',
      endpoints: [{ id: 'lan', kind: 'lan', host: 'desk-tower.local', user: 'me', port: 22 }],
      systems: [
        { id: 'linux', name: 'Linux', platform: 'linux', shell: 'posix', agent: ['/usr/bin/node', ENTRY] },
        { id: 'windows', name: 'Windows 11', platform: 'windows', agent: ['C:\\Program Files\\nodejs\\node.exe', 'C:\\Users\\me\\.legion-control\\bin\\launcher.mjs'] },
      ],
    },
    {
      id: 'pi', name: 'Beacon', site: 'home', alwaysOn: true,
      endpoints: [{ id: 'lan', host: '192.0.2.40', user: 'me' }],
      systems: [{ id: 'linux', platform: 'linux', restricted: true, agent: ['node', LAUNCHER] }],
    },
  ],
};

const copy = (value) => JSON.parse(JSON.stringify(value));
const text = (document) => `${JSON.stringify(document, null, 2)}\n`;
const machine = (document, id) => document.machines.find((entry) => entry.id === id);
const system = (document, machineId, id) => machine(document, machineId).systems.find((entry) => entry.id === id);

/** D0, or a copy of `held`, changed by `change` into a descendant of it. */
function descendant(held, change) {
  const document = copy(held);
  document.controller = { ...document.controller, revision: (held.controller?.revision ?? 0) + 1, lineage: [canonicalHash(text(held))] };
  change(document);
  return text(document);
}

/**
 * Hold `held` (stored by an ordinary session), push `change`'s descendant of
 * it as a restricted session, and hand back the answer with what is on disk.
 */
async function pushOver(change, { held = D0, options = {}, document = null } = {}) {
  return withHome(async (home) => {
    const first = storeController(text(held));
    assert.equal(first.ok, true, first.error);
    const before = fs.readFileSync(path.join(home, 'controller.json'));
    const pushed = document ?? descendant(held, change);
    const result = storeController(pushed, { restricted: true, ...options });
    const after = fs.readFileSync(path.join(home, 'controller.json'));
    return { result, before, after, heldHash: first.hash, pushed };
  });
}

function refused({ result, before, after, heldHash }, launchChanges) {
  assert.equal(result.ok, false, 'a restricted key may not publish this');
  assert.equal(result.reasonCode, 'restricted');
  assert.deepEqual(result.launchChanges, launchChanges);
  assert.equal(result.current.hash, heldHash, 'the refusal says what the machine still holds');
  assert.deepEqual(after, before, 'nothing was written');
}

function stored({ result }) {
  assert.equal(result.ok, true, result.error);
  assert.ok(['stored', 'replaced'].includes(result.action), result.action);
}

test('a restricted key renames, reroutes and rearranges a machine it holds', async () => {
  stored(await pushOver((document) => {
    machine(document, 'tower').name = 'Desk tower';
    Object.assign(machine(document, 'tower').endpoints[0], { host: '192.0.2.10', port: 2222, user: 'other' });
    machine(document, 'tower').endpoints.push({ id: 'remote', kind: 'remote', host: 'tower.example.net', user: 'me' });
    document.sites.push({ id: 'office', name: 'Office', lanPrefixes: ['192.168.10.'] });
    machine(document, 'tower').wake = { mac: '02:00:00:00:00:01', helpers: [{ machine: 'pi', action: 'wake-tower' }] };
    machine(document, 'tower').systems.reverse();
  }));
});

test('a changed agent is refused by its path, and the message never repeats the argv', async () => {
  const outcome = await pushOver((document) => { system(document, 'tower', 'linux').agent = ['sh', '-c', 'touch /tmp/canary-q7x']; });
  refused(outcome, ['machines[tower].systems[linux]']);
  assert.match(outcome.result.error, /machines\[tower\]\.systems\[linux\]/);
  assert.equal(JSON.stringify(outcome.result).includes('canary-q7x'), false);
});

test('the grammar is part of the launch value, the raw fields are not', async () => {
  refused(await pushOver((document) => { system(document, 'tower', 'linux').agent.push('self-update', '--rollback'); }), ['machines[tower].systems[linux]']);
  refused(await pushOver((document) => { system(document, 'tower', 'linux').shell = 'powershell'; }), ['machines[tower].systems[linux]']);
  refused(await pushOver((document) => { system(document, 'pi', 'linux').restricted = false; }), ['machines[pi].systems[linux]']);
  // On Windows a missing shell is cmd, so writing it out changes nothing that runs.
  stored(await pushOver((document) => { system(document, 'tower', 'windows').shell = 'cmd'; }));
  stored(await pushOver((document) => { system(document, 'tower', 'linux').platform = 'linux'; system(document, 'pi', 'linux').restricted = true; }));
});

test('the launcher migration of a held tuple is accepted, and nothing else like it is', async () => {
  stored(await pushOver((document) => { system(document, 'tower', 'linux').agent = ['/usr/bin/node', LAUNCHER]; }));
  refused(await pushOver((document) => { system(document, 'tower', 'linux').agent = ['/usr/local/bin/node', LAUNCHER]; }), ['machines[tower].systems[linux]']);
  refused(await pushOver((document) => { system(document, 'tower', 'linux').agent = ['/usr/bin/node', '/home/other/.legion-control/bin/launcher.mjs']; }), ['machines[tower].systems[linux]']);
  // Back from the launcher to the entry is never a migration.
  refused(await pushOver((document) => { system(document, 'pi', 'linux').agent = ['node', ENTRY]; }), ['machines[pi].systems[linux]']);
  assert.equal(isLauncherMigration({ agent: ['node', '/a/../b/agent/src/index.mjs'], grammar: 'posix' }, { agent: ['node', '/a/../b/bin/launcher.mjs'], grammar: 'posix' }), false);
});

test('an identity file, a repository and a local agent are launch values too', async () => {
  refused(await pushOver((document) => { machine(document, 'tower').ssh = { identityFile: '~/.ssh/id_tower' }; }), ['machines[tower].ssh.identityFile']);
  refused(await pushOver((document) => { document.appUpdates = { githubRepo: 'someone/fork' }; }), ['appUpdates.githubRepo']);
  refused(await pushOver((document) => { document.local = { agent: '/Users/me/other.mjs' }; }), ['local']);
  // The built-in repository and the default local path are no launch value at all.
  stored(await pushOver((document) => { document.appUpdates = { githubRepo: 'x1f4r/legion-control' }; document.local = {}; }));

  const extras = copy(D0);
  machine(extras, 'tower').ssh = { identityFile: '~/.ssh/id_tower' };
  extras.appUpdates = { githubRepo: 'someone/fork' };
  extras.local = { agent: '/Users/me/agent.mjs' };
  stored(await pushOver((document) => { delete machine(document, 'tower').ssh; delete document.appUpdates; delete document.local; }, { held: extras }));
  // Moved to another machine, a held identity file is still one this machine holds.
  stored(await pushOver((document) => { machine(document, 'pi').ssh = { identityFile: '~/.ssh/id_tower' }; }, { held: extras }));
});

test('held values may be reused anywhere, and a new machine may not bring a new one', async () => {
  stored(await pushOver((document) => { machine(document, 'tower').id = 'desk'; }));
  stored(await pushOver((document) => {
    document.machines.push({ id: 'nas', name: 'NAS', endpoints: [{ host: '192.0.2.50' }], systems: [{ id: 'linux', platform: 'linux', agent: ['node', LAUNCHER], restricted: true }] });
  }));
  refused(await pushOver((document) => {
    document.machines.push({ id: 'nas', name: 'NAS', endpoints: [{ host: '192.0.2.50' }], systems: [{ id: 'linux', agent: ['node', '/srv/nas/run.mjs'] }] });
  }), ['machines[nas].systems[linux]']);
  // Removing a machine or emptying an agent introduces nothing.
  stored(await pushOver((document) => { document.machines.pop(); system(document, 'tower', 'linux').agent = []; }));
});

test('every refused value is listed in document order, the message names the first five', async () => {
  const outcome = await pushOver((document) => {
    for (const [id, index] of [['a', 1], ['b', 2], ['c', 3], ['d', 4], ['e', 5], ['f', 6]]) {
      document.machines.push({ id, endpoints: [{ host: `192.0.2.${index}` }], systems: [{ id: 'linux', agent: ['node', `/srv/${id}.mjs`] }] });
    }
    document.appUpdates = { githubRepo: 'someone/fork' };
  });
  const paths = ['a', 'b', 'c', 'd', 'e', 'f'].map((id) => `machines[${id}].systems[linux]`);
  refused(outcome, [...paths, 'appUpdates.githubRepo']);
  assert.match(outcome.result.error, /\(machines\[a\]\.systems\[linux\], .*machines\[e\]\.systems\[linux\], …\)/);
});

test('--replace is held to the same rule, and cannot be used to launder a launcher in two steps', async () => {
  const otherSetup = (change) => (document) => { document.controller = { id: 'setup-other', revision: 1, lineage: [] }; change(document); };
  refused(await pushOver(otherSetup((document) => { system(document, 'tower', 'linux').agent = ['sh', '-c', 'id']; }), { options: { replace: true } }), ['machines[tower].systems[linux]']);
  const replaced = await pushOver(otherSetup(() => {}), { options: { replace: true } });
  stored(replaced);
  assert.equal(replaced.result.action, 'replaced');

  await withHome(async () => {
    assert.equal(storeController(text(D0)).ok, true);
    // Step one: an identity-less copy with exactly the held launch values.
    const anonymous = copy(D0);
    delete anonymous.controller;
    const first = storeController(text(anonymous), { restricted: true, replace: true });
    assert.equal(first.action, 'replaced', first.error);
    assert.equal(readController().meta.id, null);
    // Step two: nothing protects an identity-less copy's lineage, and its
    // launch values are compared all the same.
    const second = storeController(descendant(D0, (document) => { system(document, 'tower', 'linux').agent = ['sh', '-c', 'id']; }), { restricted: true });
    assert.equal(second.reasonCode, 'restricted');
    assert.deepEqual(second.launchChanges, ['machines[tower].systems[linux]']);
  });
});

test('with nothing held the ordinary rules apply; a copy that cannot be read cannot be replaced', async () => {
  await withHome(async () => {
    const first = storeController(text(D0), { restricted: true });
    assert.equal(first.action, 'stored', first.error);
  });

  // A copy with no identity, as a 2.x client left it, is still compared.
  const legacy = copy(D0);
  delete legacy.controller;
  refused(await pushOver(null, { held: legacy, document: descendant(D0, (document) => { system(document, 'pi', 'linux').agent = ['node', '/srv/x.mjs']; }) }), ['machines[pi].systems[linux]']);

  for (const bytes of [Buffer.from('this is not json\n'), Buffer.from([0x7b, 0xff, 0x7d, 0x0a])]) {
    await withHome(async (home) => {
      fs.writeFileSync(path.join(home, 'controller.json'), bytes);
      const result = storeController(text(D0), { restricted: true });
      assert.equal(result.reasonCode, 'restricted');
      assert.equal(result.launchChanges, undefined, 'there are no held values to name a change against');
      assert.match(result.error, /cannot read/);
      assert.deepEqual(fs.readFileSync(path.join(home, 'controller.json')), bytes);
      // An ordinary session may replace it.
      assert.equal(storeController(text(D0)).action, 'stored');
    });
  }
});

test('the structure is checked before the launch values, and a noop is never refused', async () => {
  const outcome = await pushOver((document) => {
    system(document, 'tower', 'linux').agent = ['sh', '-c', 'id'];
    machine(document, 'tower').endpoints[0].host = 'victim.example,*';
  });
  assert.equal(outcome.result.reasonCode, 'config-invalid');
  await withHome(async () => {
    storeController(text(D0));
    const again = storeController(text(D0), { restricted: true });
    assert.equal(again.action, 'noop');
  });
});

test('an unrestricted session is never refused for a launch value', async () => {
  await withHome(async () => {
    storeController(text(D0));
    const result = storeController(descendant(D0, (document) => { system(document, 'tower', 'linux').agent = ['sh', '-c', 'id']; }));
    assert.equal(result.action, 'stored', result.error);
  });
});

test('the projection reads the parsed JSON: the first of a repeated id counts, inert tuples are null', () => {
  const projection = launchProjection({
    machines: [
      { id: 'tower', systems: [{ id: 'linux', agent: ['node', 'a.mjs'] }, { id: 'linux', agent: ['node', 'b.mjs'] }, { id: 'empty', agent: [] }, { agent: ['x'] }] },
      'not a machine',
      { id: 'tower', systems: [{ id: 'other', agent: ['node', 'c.mjs'] }] },
    ],
    appUpdates: { githubRepo: '' },
    local: { enabled: false, agent: '/x' },
  });
  assert.deepEqual(projection, {
    machines: [{ id: 'tower', identityFile: null, systems: [
      { id: 'linux', tuple: { agent: ['node', 'a.mjs'], grammar: 'posix' } },
      { id: 'empty', tuple: { agent: null, grammar: null } },
    ] }],
    repo: null,
    local: null,
  });
});
