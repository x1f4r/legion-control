// Launch values and local approval, v3.md section 9.9, as cases.
//
// Every review case is a held document and a descendant of it: the offered
// document's lineage names the held hash, computed when the vectors are built,
// so the agent can store the one and be pushed the other exactly as a peer
// would push it. The held document is what the machine holds and, unless the
// case gives its own record, what this device approved. The cases are the
// edits a restricted publisher could make, the harmless ones that must go
// through with no prompt and the ones that must wait (T1): a changed launcher,
// a verb appended to a launcher, a grammar changed through `shell`, `platform`
// or `restricted`, a key path, an update source (T6), a local agent, a path
// that only looks the same, and the one migration every 3.1 upgrade makes.

import { canonicalHash } from '../canonical.mjs';
import { SETUP_ID, copyOf, documentText } from '../documents.mjs';

const EVERYONE = ['agent', 'mac', 'desktop', 'android'];
const T1 = ['finding:t1'];
const T6 = ['finding:t1', 'finding:t6'];

const NODE = '/usr/bin/node';
const LAUNCHER = '/home/me/.legion-control/bin/launcher.mjs';
const ENTRY = '/home/me/.legion-control/agent/src/index.mjs';
const WIN_NODE = 'C:\\Program Files\\nodejs\\node.exe';
const WIN_LAUNCHER = 'C:\\Users\\me\\.legion-control\\bin\\launcher.mjs';
const WIN_ENTRY = 'C:\\Users\\me\\.legion-control\\agent\\src\\index.mjs';
const MAC_LOCAL = '/Users/me/.legion-control/agent/src/index.mjs';

/**
 * What the machine holds: a tower with a Linux and a Windows system, and a pi
 * whose key is bound to the restricted dispatcher. Every launch value in it is
 * one a device would approve.
 */
const HELD = Object.freeze({
  version: 1,
  controller: { id: SETUP_ID, name: 'Home setup', revision: 7, source: 'desktop', device: 'Desk tower', lineage: [] },
  machines: [
    {
      id: 'tower',
      name: 'Tower',
      endpoints: [{ id: 'lan', kind: 'lan', host: 'desk-tower.local', user: 'me', port: 22 }],
      systems: [
        { id: 'linux', name: 'Linux', platform: 'linux', shell: 'posix', agent: [NODE, LAUNCHER] },
        { id: 'windows', name: 'Windows 11', platform: 'windows', shell: 'cmd', agent: [WIN_NODE, WIN_LAUNCHER] },
      ],
    },
    {
      id: 'pi',
      name: 'Beacon',
      alwaysOn: true,
      endpoints: [{ id: 'lan', kind: 'lan', host: '192.0.2.40', user: 'me', port: 22 }],
      systems: [{ id: 'linux', name: 'Raspberry Pi OS', platform: 'linux', restricted: true, agent: ['node', LAUNCHER] }],
    },
  ],
});

const tower = (document) => document.machines.find((machine) => machine.id === 'tower');
const pi = (document) => document.machines.find((machine) => machine.id === 'pi');
const towerLinux = (document) => tower(document).systems[0];
const towerWindows = (document) => tower(document).systems[1];
const piLinux = (document) => pi(document).systems[0];

const system = (machine, id) => `machines[${machine}].systems[${id}]`;
const identity = (machine) => `machines[${machine}].ssh.identityFile`;
const REPO = 'appUpdates.githubRepo';

/**
 * A review case. `held` changes the held document, `offered` changes a copy of
 * it into the descendant a peer publishes; `approvals` is this device's own
 * record when it is not simply the held document's values, and `enrolment`
 * means nothing is held and nothing approved.
 */
const review = (name, purpose, { held = () => {}, offered = () => {}, approvals = null, enrolment = false }, want, tags = T1) => ({
  name, purpose, tags,
  input: () => {
    const before = copyOf(HELD);
    held(before);
    const heldText = enrolment ? null : documentText(before);
    const after = copyOf(before);
    offered(after);
    if (heldText !== null) {
      after.controller = {
        ...after.controller,
        revision: before.controller.revision + 1,
        lineage: [canonicalHash(heldText), ...before.controller.lineage].slice(0, 32),
      };
    }
    return { heldText, approvals, offeredText: documentText(after) };
  },
  want,
});

const waiting = (path, scope, kind, approvable = true) => ({ path, scope, kind, approvable });

/**
 * What this device runs for the held document as it is, entry by entry, with
 * `changes` applied by path. Stated by hand once, so each case only says what
 * it changes.
 */
function effective(changes = {}, extra = []) {
  const run = (agent, grammar) => ({ state: 'offered', agent, grammar, text: null });
  const list = [
    { path: system('tower', 'linux'), scope: 'system', ...run([NODE, LAUNCHER], 'posix') },
    { path: system('tower', 'windows'), scope: 'system', ...run([WIN_NODE, WIN_LAUNCHER], 'cmd') },
    { path: system('pi', 'linux'), scope: 'system', ...run(['node', LAUNCHER], 'dispatch') },
    { path: identity('tower'), scope: 'identityFile', state: 'none', agent: null, grammar: null, text: null },
    { path: identity('pi'), scope: 'identityFile', state: 'none', agent: null, grammar: null, text: null },
    { path: REPO, scope: 'repo', state: 'built-in', agent: null, grammar: null, text: null },
    { path: 'local', scope: 'local', state: 'none', agent: null, grammar: null, text: null },
  ];
  for (const [path, change] of Object.entries(changes)) {
    const entry = list.find((candidate) => candidate.path === path);
    if (entry === undefined) throw new Error(`effective() has no entry ${path}`);
    Object.assign(entry, change);
  }
  // Systems of machines the held document does not have go after the last
  // system, identityFiles after the last identityFile.
  const systems = list.filter((entry) => entry.scope === 'system');
  const rest = list.filter((entry) => entry.scope !== 'system');
  const identities = rest.filter((entry) => entry.scope === 'identityFile');
  const tail = rest.filter((entry) => entry.scope !== 'identityFile');
  return [...systems, ...extra.filter((entry) => entry.scope === 'system'), ...identities, ...extra.filter((entry) => entry.scope === 'identityFile'), ...tail];
}
const NOT_DRIVEN = { state: 'not-driven', agent: null, grammar: null };
const APPROVED = { state: 'approved' };

const NAS = (agent) => ({
  id: 'nas',
  name: 'Attic NAS',
  endpoints: [{ id: 'lan', kind: 'lan', host: '192.0.2.50', user: 'me', port: 22 }],
  systems: [{ id: 'linux', name: 'Debian', platform: 'linux', shell: 'posix', agent }],
});

const reviews = [
  review('display-and-routing-only', 'A rename, a new route, another host and port, sites and a wake helper: none of it is a launch value, so it applies with no prompt and the new route is dialled.', {
    offered: (document) => {
      document.sites = [{ id: 'home', name: 'Home', lanPrefixes: ['192.168.1.'], broadcast: ['192.168.1.255'] }];
      Object.assign(tower(document), { name: 'Desk tower', site: 'home' });
      Object.assign(tower(document).endpoints[0], { host: '192.0.2.20', port: 2222 });
      tower(document).endpoints.push({ id: 'remote', kind: 'remote', host: 'tower.example.net', user: 'me' });
      tower(document).wake = { mac: '02:00:00:00:00:01', helper: { machine: 'pi', action: 'wake-tower' }, helpers: [{ machine: 'pi', action: 'wake-tower' }] };
      pi(document).site = 'home';
    },
  }, { review: [], effective: effective(), restrictedRefused: false }),
  review('agent-changed', 'A descendant that replaces the launcher with sh -c id. The document is still applied; this device keeps running the approved launcher and shows the new one for review, and a restricted key cannot publish it.', {
    offered: (document) => { towerLinux(document).agent = ['sh', '-c', 'id']; },
  }, {
    review: [waiting(system('tower', 'linux'), 'system', 'changed')],
    effective: effective({ [system('tower', 'linux')]: APPROVED }),
    restrictedRefused: true,
  }),
  review('verb-appended-to-launcher', 'The approved launcher with self-update --rollback appended. The command is appended to the argv, so every request would run a rollback first: a launch change like any other.', {
    offered: (document) => { towerLinux(document).agent = [NODE, LAUNCHER, 'self-update', '--rollback']; },
  }, { review: [waiting(system('tower', 'linux'), 'system', 'changed')], restrictedRefused: true }),
  review('shell-posix-to-powershell', 'The same argv under another shell is another command line: the grammar is part of the tuple.', {
    offered: (document) => { towerLinux(document).shell = 'powershell'; },
  }, {
    review: [waiting(system('tower', 'linux'), 'system', 'changed')],
    effective: effective({ [system('tower', 'linux')]: APPROVED }),
    restrictedRefused: true,
  }),
  review('restricted-cleared', 'Clearing restricted moves the pi from the dispatcher grammar to posix, so its line would be read by a shell: reviewed, and the approved dispatch tuple keeps running.', {
    offered: (document) => { piLinux(document).restricted = false; },
  }, {
    review: [waiting(system('pi', 'linux'), 'system', 'changed')],
    effective: effective({ [system('pi', 'linux')]: APPROVED }),
    restrictedRefused: true,
  }),
  review('restricted-absent-equals-false', 'restricted false and no restricted give the same grammar, so dropping the key changes nothing that runs.', {
    held: (document) => { towerLinux(document).restricted = false; },
    offered: (document) => { delete towerLinux(document).restricted; },
  }, { review: [], restrictedRefused: false }),
  review('platform-linux-to-windows-shell-null', 'With no shell, the platform decides the grammar: linux is posix and windows is cmd, so this is a change.', {
    held: (document) => { delete towerLinux(document).shell; },
    offered: (document) => { towerLinux(document).platform = 'windows'; },
  }, {
    review: [waiting(system('tower', 'linux'), 'system', 'changed')],
    effective: effective({ [system('tower', 'linux')]: APPROVED }),
    restrictedRefused: true,
  }),
  review('platform-absent-equals-linux', 'Without a shell, linux and no platform are both posix: no change.', {
    held: (document) => { delete towerLinux(document).shell; },
    offered: (document) => { delete towerLinux(document).platform; },
  }, { review: [], restrictedRefused: false }),
  review('windows-shell-null-to-cmd', 'An editor that writes shell cmd where it was absent on Windows changes nothing that runs, so nobody is asked.', {
    held: (document) => { delete towerWindows(document).shell; },
    offered: (document) => { towerWindows(document).shell = 'cmd'; },
  }, { review: [], effective: effective(), restrictedRefused: false }),
  review('launcher-migration-posix', 'The migration every 3.1 upgrade makes: agent/src/index.mjs to bin/launcher.mjs under the same base and interpreter. Accepted without asking.', {
    held: (document) => { towerLinux(document).agent = [NODE, ENTRY]; },
    offered: (document) => { towerLinux(document).agent = [NODE, LAUNCHER]; },
  }, { review: [], effective: effective(), restrictedRefused: false }),
  review('launcher-migration-windows-mixed-separators', 'The desktop writes the new launcher path with forward slashes into a backslashed Windows base; paths are compared after replacing \\ with /.', {
    held: (document) => { towerWindows(document).agent = [WIN_NODE, WIN_ENTRY]; },
    offered: (document) => { towerWindows(document).agent = [WIN_NODE, 'C:\\Users\\me\\.legion-control/bin/launcher.mjs']; },
  }, {
    review: [],
    effective: effective({ [system('tower', 'windows')]: { agent: [WIN_NODE, 'C:\\Users\\me\\.legion-control/bin/launcher.mjs'] } }),
    restrictedRefused: false,
  }),
  review('migration-with-new-interpreter', 'A migration that also changes the interpreter is two changes: reviewed.', {
    held: (document) => { towerLinux(document).agent = [NODE, ENTRY]; },
    offered: (document) => { towerLinux(document).agent = ['/usr/local/bin/node', LAUNCHER]; },
  }, {
    review: [waiting(system('tower', 'linux'), 'system', 'changed')],
    effective: effective({ [system('tower', 'linux')]: { state: 'approved', agent: [NODE, ENTRY] } }),
    restrictedRefused: true,
  }),
  review('migration-with-grammar-change', 'A migration that also changes the shell changes the grammar: reviewed.', {
    held: (document) => { towerWindows(document).agent = [WIN_NODE, WIN_ENTRY]; },
    offered: (document) => { Object.assign(towerWindows(document), { agent: [WIN_NODE, WIN_LAUNCHER], shell: 'powershell' }); },
  }, { review: [waiting(system('tower', 'windows'), 'system', 'changed')], restrictedRefused: true }),
  review('launcher-back-to-entry', 'The reverse of the migration is not a migration: a launcher replaced by a direct entry is reviewed.', {
    offered: (document) => { towerLinux(document).agent = [NODE, ENTRY]; },
  }, { review: [waiting(system('tower', 'linux'), 'system', 'changed')], restrictedRefused: true }),
  review('migration-with-dotdot-base', 'A base with a .. segment can name another directory than it seems to: never accepted as a migration.', {
    held: (document) => { towerLinux(document).agent = [NODE, '/home/me/../me/.legion-control/agent/src/index.mjs']; },
    offered: (document) => { towerLinux(document).agent = [NODE, '/home/me/../me/.legion-control/bin/launcher.mjs']; },
  }, { review: [waiting(system('tower', 'linux'), 'system', 'changed')], restrictedRefused: true }),
  review('agent-removed', 'A system without an agent runs nothing, so removing one needs no approval; the system is simply not driven.', {
    offered: (document) => { delete towerLinux(document).agent; },
  }, {
    review: [],
    effective: effective({ [system('tower', 'linux')]: NOT_DRIVEN }),
    restrictedRefused: false,
  }),
  review('agent-emptied', 'An empty agent is the same as none.', {
    offered: (document) => { towerLinux(document).agent = []; },
  }, { review: [], effective: effective({ [system('tower', 'linux')]: NOT_DRIVEN }), restrictedRefused: false }),
  review('machine-id-renamed-reusing-tuple', 'A value approved anywhere is accepted anywhere: renaming the tower to studio keeps both its tuples without a prompt.', {
    offered: (document) => { tower(document).id = 'studio'; },
  }, {
    review: [],
    effective: [
      { path: system('studio', 'linux'), scope: 'system', state: 'offered', agent: [NODE, LAUNCHER], grammar: 'posix', text: null },
      { path: system('studio', 'windows'), scope: 'system', state: 'offered', agent: [WIN_NODE, WIN_LAUNCHER], grammar: 'cmd', text: null },
      { path: system('pi', 'linux'), scope: 'system', state: 'offered', agent: ['node', LAUNCHER], grammar: 'dispatch', text: null },
      { path: identity('studio'), scope: 'identityFile', state: 'none', agent: null, grammar: null, text: null },
      { path: identity('pi'), scope: 'identityFile', state: 'none', agent: null, grammar: null, text: null },
      { path: REPO, scope: 'repo', state: 'built-in', agent: null, grammar: null, text: null },
      { path: 'local', scope: 'local', state: 'none', agent: null, grammar: null, text: null },
    ],
    restrictedRefused: false,
  }),
  review('new-machine-reusing-launcher', 'A new machine whose system uses a tuple already approved elsewhere is driven at once.', {
    offered: (document) => { document.machines.push(NAS([NODE, LAUNCHER])); },
  }, {
    review: [],
    effective: effective({}, [
      { path: system('nas', 'linux'), scope: 'system', state: 'offered', agent: [NODE, LAUNCHER], grammar: 'posix', text: null },
      { path: identity('nas'), scope: 'identityFile', state: 'none', agent: null, grammar: null, text: null },
    ]),
    restrictedRefused: false,
  }),
  review('new-machine-new-launcher', 'A new machine with a launcher nobody approved: reviewed as new, and not driven until it is approved. No ssh process is started for it.', {
    offered: (document) => { document.machines.push(NAS([NODE, '/srv/legion-control/bin/launcher.mjs'])); },
  }, {
    review: [waiting(system('nas', 'linux'), 'system', 'new')],
    effective: effective({}, [
      { path: system('nas', 'linux'), scope: 'system', ...NOT_DRIVEN, text: null },
      { path: identity('nas'), scope: 'identityFile', state: 'none', agent: null, grammar: null, text: null },
    ]),
    restrictedRefused: true,
  }),
  review('migration-accepted-at-another-location', 'The launcher migration of an approved tuple is accepted on a new machine as well: acceptance is by value, not by place.', {
    held: (document) => { towerLinux(document).agent = [NODE, ENTRY]; },
    offered: (document) => { document.machines.push(NAS([NODE, LAUNCHER])); },
  }, { review: [], restrictedRefused: false }),
  review('identity-file-added', 'A shared identityFile chooses which private key this device uses: a launch value, reviewed as new.', {
    offered: (document) => { tower(document).ssh = { identityFile: '~/.ssh/id_tower' }; },
  }, {
    review: [waiting(identity('tower'), 'identityFile', 'new')],
    effective: effective(),
    restrictedRefused: true,
  }),
  review('identity-file-removed', 'Removing a shared identityFile needs no approval: the device falls back to its own bindings and default key.', {
    held: (document) => { tower(document).ssh = { identityFile: '~/.ssh/id_tower' }; },
    offered: (document) => { delete tower(document).ssh; },
  }, { review: [], effective: effective(), restrictedRefused: false }),
  review('identity-file-changed', 'Another key path where an approved one was: the approved path keeps being used until a person decides.', {
    held: (document) => { tower(document).ssh = { identityFile: '~/.ssh/id_tower' }; },
    offered: (document) => { tower(document).ssh = { identityFile: '~/.ssh/id_other' }; },
  }, {
    review: [waiting(identity('tower'), 'identityFile', 'changed')],
    effective: effective({ [identity('tower')]: { state: 'approved', text: '~/.ssh/id_tower' } }),
    restrictedRefused: true,
  }),
  review('identity-file-invisible-character', 'An identityFile with a zero-width space looks like an approved path and is not one: it can only be reverted or left.', {
    offered: (document) => { tower(document).ssh = { identityFile: '~/.ssh/id_\u200btower' }; },
  }, { review: [waiting(identity('tower'), 'identityFile', 'new', false)], restrictedRefused: true }),
  review('repo-changed', 'Another update repository is another source of this device\'s own updates. Signatures stop a forged build, not a frozen or redirected source, so it waits (T6).', {
    held: (document) => { document.appUpdates = { githubRepo: 'me/legion-control' }; },
    offered: (document) => { document.appUpdates = { githubRepo: 'me/legion-fork' }; },
  }, {
    review: [waiting(REPO, 'repo', 'changed')],
    effective: effective({ [REPO]: { state: 'approved', text: 'me/legion-control' } }),
    restrictedRefused: true,
  }, T6),
  review('repo-explicit-default', 'Writing the built-in repository explicitly is the same as not writing it.', {
    offered: (document) => { document.appUpdates = { githubRepo: 'x1f4r/legion-control' }; },
  }, { review: [], effective: effective(), restrictedRefused: false }, T6),
  review('repo-removed', 'Removing the repository goes back to the built-in one, which needs no approval.', {
    held: (document) => { document.appUpdates = { githubRepo: 'me/legion-control' }; },
    offered: (document) => { delete document.appUpdates; },
  }, { review: [], effective: effective(), restrictedRefused: false }, T6),
  review('local-agent-changed', 'The deprecated local block makes a Mac run node with this path on itself: another path waits.', {
    held: (document) => { document.local = { enabled: true, agent: MAC_LOCAL }; },
    offered: (document) => { document.local = { enabled: true, agent: '/Users/me/Downloads/index.mjs' }; },
  }, {
    review: [waiting('local', 'local', 'changed')],
    effective: effective({ local: { state: 'approved', text: MAC_LOCAL } }),
    restrictedRefused: true,
  }),
  review('local-default-path', 'A local block without an agent means the default path, which is accepted without asking.', {
    offered: (document) => { document.local = {}; },
  }, { review: [], effective: effective({ local: { state: 'offered', text: null } }), restrictedRefused: false }),
  review('local-disabled', 'A local block with enabled false is no local agent, whatever path it names.', {
    offered: (document) => { document.local = { enabled: false, agent: '/Users/me/Downloads/index.mjs' }; },
  }, { review: [], effective: effective(), restrictedRefused: false }),
  review('local-agent-bidi-control', 'A local agent path holding a right-to-left override cannot be shown faithfully, so it cannot be approved.', {
    offered: (document) => { document.local = { agent: '/Users/me/\u202eindex.mjs' }; },
  }, { review: [waiting('local', 'local', 'new', false)], restrictedRefused: true }),
  review('unicode-path-not-normalised', 'é written precomposed and é written as e plus a combining accent look the same and are different paths: strings are compared by code point, with no Unicode normalisation.', {
    held: (document) => { towerLinux(document).agent = [NODE, '/home/me/caf\u00e9/bin/launcher.mjs']; },
    offered: (document) => { towerLinux(document).agent = [NODE, '/home/me/cafe\u0301/bin/launcher.mjs']; },
  }, { review: [waiting(system('tower', 'linux'), 'system', 'changed')], restrictedRefused: true }),
  review('argv-with-bidi-control', 'A right-to-left override makes an argv item display as something else. Not approvable: only Revert and Not now are offered, and the approved tuple keeps running.', {
    offered: (document) => { towerLinux(document).agent = [NODE, `${LAUNCHER}\u202e`]; },
  }, {
    review: [waiting(system('tower', 'linux'), 'system', 'changed', false)],
    effective: effective({ [system('tower', 'linux')]: APPROVED }),
    restrictedRefused: true,
  }),
  review('restricted-three-element-agent', 'A restricted system with three argv items has no grammar at all (refused:restricted-agent-shape): not approvable, and the approved dispatch tuple keeps running.', {
    offered: (document) => { piLinux(document).agent = ['node', LAUNCHER, 'run']; },
  }, {
    review: [waiting(system('pi', 'linux'), 'system', 'changed', false)],
    effective: effective({ [system('pi', 'linux')]: APPROVED }),
    restrictedRefused: true,
  }),
  review('held-refused-grammar-reused', 'A refused tuple the machine already holds is accepted when it is published again, so a restricted key can still rename around it; it is not runnable, so the system stays not driven.', {
    held: (document) => { piLinux(document).agent = ['node', LAUNCHER, 'run']; },
    offered: (document) => { pi(document).name = 'Beacon 2'; },
  }, {
    review: [],
    effective: effective({ [system('pi', 'linux')]: NOT_DRIVEN }),
    restrictedRefused: false,
  }),
  review('fallback-to-location-record', 'This device approved a different launcher at tower/linux than the machine holds. The offered one waits, and the device keeps running what it approved at that location; values approved nowhere are not driven.', {
    held: (document) => { towerLinux(document).agent = [NODE, '/opt/legion-control/bin/launcher.mjs']; },
    offered: (document) => { tower(document).name = 'Desk tower'; },
    approvals: { schema: 1, seededFrom: 'none', machines: { tower: { systems: { linux: { agent: [NODE, LAUNCHER], grammar: 'posix' } } } } },
  }, {
    review: [
      waiting(system('tower', 'linux'), 'system', 'changed'),
      waiting(system('tower', 'windows'), 'system', 'new'),
      waiting(system('pi', 'linux'), 'system', 'new'),
    ],
    effective: effective({
      [system('tower', 'linux')]: APPROVED,
      [system('tower', 'windows')]: NOT_DRIVEN,
      [system('pi', 'linux')]: NOT_DRIVEN,
    }),
    restrictedRefused: false,
  }),
  review('first-enrolment', 'A device that holds nothing and approved nothing: every launch value is new, nothing is driven until the enrolment summary is confirmed, and with nothing held a restricted push is not refused.', {
    enrolment: true,
  }, {
    review: [
      waiting(system('tower', 'linux'), 'system', 'new'),
      waiting(system('tower', 'windows'), 'system', 'new'),
      waiting(system('pi', 'linux'), 'system', 'new'),
    ],
    effective: effective({
      [system('tower', 'linux')]: NOT_DRIVEN,
      [system('tower', 'windows')]: NOT_DRIVEN,
      [system('pi', 'linux')]: NOT_DRIVEN,
    }),
    restrictedRefused: false,
  }),
];

const migration = (name, approved, offered, result, purpose) => ({
  name, purpose, tags: T1, input: { approved, offered }, want: { migration: result },
});
const tuple = (agent, grammar) => ({ agent, grammar });

const migrations = [
  migration('migration-posix', tuple([NODE, ENTRY], 'posix'), tuple([NODE, LAUNCHER], 'posix'), true,
    'The direct entry replaced by the stable launcher under the same base.'),
  migration('migration-windows-mixed-separators', tuple([WIN_NODE, WIN_ENTRY], 'cmd'), tuple([WIN_NODE, 'C:\\Users\\me\\.legion-control/bin/launcher.mjs'], 'cmd'), true,
    'Separators are compared after replacing \\ with /, because the desktop mixes them.'),
  migration('migration-dispatch', tuple(['node', ENTRY], 'dispatch'), tuple(['node', LAUNCHER], 'dispatch'), true,
    'A restricted system migrates the same way.'),
  migration('migration-reverse', tuple([NODE, LAUNCHER], 'posix'), tuple([NODE, ENTRY], 'posix'), false,
    'Back from the launcher to the entry is never a migration.'),
  migration('migration-different-grammar', tuple([NODE, ENTRY], 'posix'), tuple([NODE, LAUNCHER], 'powershell'), false,
    'The grammar must be the same.'),
  migration('migration-dotdot-base', tuple([NODE, '/home/me/../me/.legion-control/agent/src/index.mjs'], 'posix'), tuple([NODE, '/home/me/../me/.legion-control/bin/launcher.mjs'], 'posix'), false,
    'A base with a .. segment is refused.'),
  migration('migration-empty-prefix', tuple([NODE, '/agent/src/index.mjs'], 'posix'), tuple([NODE, '/bin/launcher.mjs'], 'posix'), false,
    'An empty base would make /bin/launcher.mjs at the root of the file system acceptable.'),
  migration('migration-different-bases', tuple([NODE, ENTRY], 'posix'), tuple([NODE, '/srv/legion-control/bin/launcher.mjs'], 'posix'), false,
    'The launcher must be under the same base as the entry it replaces.'),
  migration('migration-two-indices', tuple([NODE, ENTRY], 'posix'), tuple(['/usr/local/bin/node', LAUNCHER], 'posix'), false,
    'Exactly one item may differ.'),
  migration('migration-different-lengths', tuple([NODE, ENTRY], 'posix'), tuple([NODE, LAUNCHER, 'status'], 'posix'), false,
    'An item added is not a migration.'),
  migration('migration-same-tuple', tuple([NODE, LAUNCHER], 'posix'), tuple([NODE, LAUNCHER], 'posix'), false,
    'Equal tuples are accepted as equal, not as a migration: exactly one item must differ.'),
  migration('migration-inert', tuple([NODE, ENTRY], 'posix'), tuple(null, null), false,
    'An inert tuple is never a migration of anything.'),
];

export default {
  family: 'launch',
  section: '9.9',
  reference: 'tools/launch.mjs',
  notes: [
    'review: the record is input.approvals when it is not null, otherwise the record holding every launch value of heldText, otherwise {"schema":1,"seededFrom":"none"}. offeredText descends from heldText: its lineage starts with the held hash.',
    'review lists each offered launch value that is not accepted without asking, in this order: per machine its systems then its identityFile, then the repository, then local. kind is changed when the record holds a value at that path, new otherwise.',
    'effective lists one entry per offered system, then one identityFile entry per machine, then the repository, then local. state is offered, approved, not-driven (systems), none (identityFile, local) or built-in (repository). agent and grammar are set only for a system that runs; text holds the identityFile, the repository or local.agent, and is null for the built-in repository and for local\'s default path.',
    'A value that is accepted but cannot run (approvable false) falls back to the record at the same location, and only to a recorded value that can run itself.',
    'restrictedRefused is what agent 3.1.0 answers a restricted config set of offeredText over heldText: true exactly when heldText is not null and a launch value would wait for review against heldText\'s own values.',
  ],
  consumers: { review: EVERYONE, migration: EVERYONE },
  groups: { review: reviews, migration: migrations },
};
