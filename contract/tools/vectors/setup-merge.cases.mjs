// Merging, v3.md section 9.8, as cases.
//
// Every merge case starts from one base setup: a tower woken by an always-on
// pi, and a nas, all at one site. Mine and theirs are edits of copies of it,
// written by a desktop and a phone, both revision 21 with the base as their
// parent, so each case says only what the two sides changed. The cases are the
// ways the three clients' merges lost data (C6): a deletion that silently beat
// an edit, endpoints without an id that vanished, unknown keys kept from one
// side only, merged entries moved to the end of their arrays, and `22.0`
// printed as `22`. Each states the differences a person is shown and the
// document the merge writes, built here from mine with the expected changes
// applied, so the statement does not run through the reference.

import crypto from 'node:crypto';
import { canonicalHash } from '../canonical.mjs';
import { H_GRANDPARENT, H_PARENT, SETUP_ID, TOWER_DOC, at, copyOf, documentText } from '../documents.mjs';

const CLIENTS = ['mac', 'desktop', 'android'];
const C6 = ['finding:c6'];

const NODE = '/usr/bin/node';
const LAUNCHER = '/home/me/.legion-control/bin/launcher.mjs';
const linux = (name) => ({ id: 'linux', name, platform: 'linux', shell: 'posix', agent: [NODE, LAUNCHER] });
const lan = (host) => ({ id: 'lan', kind: 'lan', host, user: 'me', port: 22 });

const BASE = Object.freeze({
  version: 1,
  controller: { id: SETUP_ID, name: 'Home setup', revision: 20, updatedAt: at(-600), source: 'mac', device: 'Office MacBook Pro', lineage: [H_PARENT, H_GRANDPARENT] },
  sites: [{ id: 'home', name: 'Home', lanPrefixes: ['192.168.1.'], broadcast: ['192.168.1.255'] }],
  machines: [
    {
      id: 'tower',
      name: 'Tower',
      site: 'home',
      endpoints: [lan('192.0.2.20'), { id: 'remote', kind: 'remote', host: 'tower.example.net', user: 'me' }],
      systems: [linux('Linux')],
      wake: { mac: '02:00:00:00:00:01', helper: { machine: 'pi', action: 'wake-tower' }, helpers: [{ machine: 'pi', action: 'wake-tower' }] },
    },
    { id: 'pi', name: 'Beacon', site: 'home', alwaysOn: true, endpoints: [lan('192.0.2.40')], systems: [linux('Raspberry Pi OS')] },
    { id: 'nas', name: 'NAS', site: 'home', alwaysOn: true, endpoints: [lan('192.0.2.10')], systems: [linux('Debian')] },
  ],
});

const MINE = Object.freeze({ revision: 21, updatedAt: at(-30), source: 'desktop', device: 'Desk tower' });
const THEIRS = Object.freeze({ revision: 21, updatedAt: at(-20), source: 'phone', device: 'Pixel 9' });
const WRITER = Object.freeze({ source: 'desktop', device: 'Desk tower', now: at(0) });

const machine = (document, id) => document.machines.find((entry) => entry.id === id);
const endpoint = (document, machineId, id) => machine(document, machineId).endpoints.find((entry) => entry.id === id);
const removeMachine = (document, id) => document.machines.splice(document.machines.findIndex((entry) => entry.id === id), 1);
const d = (entry, kind) => ({ entry, kind });
const same = (text) => text;
/** A stand-in hash for a revision no case needs the document of. */
const standIn = (label) => crypto.createHash('sha256').update(`setup-merge ${label}`).digest('hex');

/** Replace the one occurrence of `from` in a document text: how a case spells a number JSON.stringify would not. */
function respell(text, from, to) {
  if (text.split(from).length !== 2) throw new Error(`${JSON.stringify(from)} is not in the text exactly once`);
  return text.replace(from, to);
}

/** A side of the merge: a copy of the base, edited, stamped as a child of the base. */
function branch(base, baseHash, change, stamp) {
  const document = copyOf(base);
  change(document);
  document.controller = {
    ...document.controller,
    revision: stamp.revision,
    updatedAt: stamp.updatedAt,
    source: stamp.source,
    device: stamp.device,
    lineage: stamp.lineage === undefined ? [baseHash, ...base.controller.lineage] : stamp.lineage(baseHash),
  };
  return document;
}

/**
 * A merge case. `base` edits a copy of the base setup; `mine` and `theirs`
 * edit copies of that; `spell` rewrites the texts of base, mine, theirs and
 * the stated result where a number must be spelled the way no JSON.stringify
 * writes it. `want` is a function of the built documents, so a case can state
 * its result as "mine, with these changes, under the merged identity": the
 * id is mine's, the revision one more than the higher, the writer's time,
 * source and device, and `lineage` (by default both heads followed by the
 * shared ancestry, each once).
 */
function mergeCase(name, purpose, { base: changeBase = () => {}, mine = () => {}, theirs = () => {}, noBase = false, choices = {}, spell = {}, stamps = {} }, want, tags = C6) {
  const base = copyOf(BASE);
  changeBase(base);
  const baseText = (spell.base ?? same)(documentText(base));
  const baseHash = canonicalHash(baseText);
  const mineDoc = branch(base, baseHash, mine, { ...MINE, ...stamps.mine });
  const theirsDoc = branch(base, baseHash, theirs, { ...THEIRS, ...stamps.theirs });
  const mineText = (spell.mine ?? same)(documentText(mineDoc));
  const theirsText = (spell.theirs ?? same)(documentText(theirsDoc));
  const mineHash = canonicalHash(mineText);
  const theirsHash = canonicalHash(theirsText);
  const lineage = stamps.lineage?.({ mineHash, theirsHash }) ?? [mineHash, theirsHash, baseHash, ...base.controller.lineage];
  const revision = Math.max(mineDoc.controller.revision, theirsDoc.controller.revision) + 1;
  const result = (change = () => {}) => {
    const document = copyOf(mineDoc);
    change(document);
    document.controller = { ...document.controller, revision, updatedAt: WRITER.now, source: WRITER.source, device: WRITER.device, lineage };
    return (spell.result ?? same)(documentText(document));
  };
  return {
    name, purpose, tags,
    input: { baseText: noBase ? null : baseText, mineText, theirsText, mineHash, theirsHash, writer: { ...WRITER }, choices },
    want: want({ mineDoc, theirsDoc, result }),
  };
}

// The lineages of long-lineages: mine m19 … m1 and then the base, theirs
// t19 … t1 and then the base, and the merged order after both heads, stated
// by hand rather than computed.
const LONG = (() => {
  const mine = [];
  const theirs = [];
  for (let index = 19; index >= 1; index -= 1) {
    mine.push(standIn(`m${index}`));
    theirs.push(standIn(`t${index}`));
  }
  const stated = [
    'm19', 't19', 'm18', 't18', 'm17', 't17', 'm16', 't16', 'm15', 't15',
    'm14', 't14', 'm13', 't13', 'm12', 't12', 'm11', 't11', 'm10', 't10',
    'm9', 't9', 'm8', 't8', 'm7', 't7', 'm6', 't6', 'm5', 't5',
  ].map(standIn);
  return { mine, theirs, stated };
})();

const merges = [
  mergeCase('both-sides-applied', 'Theirs renames pi and mine adds an endpoint to tower. Each side changed a different entry, so each is taken from the side that changed it and nobody is asked.', {
    mine: (document) => machine(document, 'tower').endpoints.push({ id: 'lan2', kind: 'lan', host: '192.0.2.21', user: 'me', port: 22 }),
    theirs: (document) => { machine(document, 'pi').name = 'Pi'; },
  }, ({ result }) => ({
    differences: [d('machines[pi].fields', 'theirs'), d('machines[tower].endpoints[lan2]', 'mine')],
    resultText: result((document) => { machine(document, 'pi').name = 'Pi'; }),
    error: null,
  })),

  mergeCase('delete-versus-edit-keep-deletion', 'Mine deletes tower while theirs edits its lan endpoint. Delete against edit is one question about the whole machine, never a silent winner; answered mine, tower is gone.', {
    mine: (document) => removeMachine(document, 'tower'),
    theirs: (document) => { endpoint(document, 'tower', 'lan').host = '192.0.2.22'; },
    choices: { 'machines[tower]': 'mine' },
  }, ({ result }) => ({
    differences: [d('machines[tower]', 'both')],
    resultText: result(),
    error: null,
  })),

  mergeCase('delete-versus-edit-take-edit', 'The same question left at its default, theirs: the edited tower comes back, at the position it has in theirs, which is first.', {
    mine: (document) => removeMachine(document, 'tower'),
    theirs: (document) => { endpoint(document, 'tower', 'lan').host = '192.0.2.22'; },
  }, ({ theirsDoc, result }) => ({
    differences: [d('machines[tower]', 'both')],
    resultText: result((document) => { document.machines.unshift(copyOf(machine(theirsDoc, 'tower'))); }),
    error: null,
  })),

  mergeCase('theirs-deletes-what-mine-edited', 'Theirs deletes tower while mine edits its wake block. The question is shown, and its default, theirs, removes the machine.', {
    mine: (document) => { machine(document, 'tower').wake.ports = [9]; },
    theirs: (document) => removeMachine(document, 'tower'),
  }, ({ result }) => ({
    differences: [d('machines[tower]', 'both')],
    resultText: result((document) => removeMachine(document, 'tower')),
    error: null,
  })),

  mergeCase('mine-deletes-unchanged-machine', 'Mine deletes a tower theirs left as the base had it, so the deletion is mine alone and is kept; theirs\' rename of nas is taken.', {
    mine: (document) => removeMachine(document, 'tower'),
    theirs: (document) => { machine(document, 'nas').name = 'Attic NAS'; },
  }, ({ result }) => ({
    differences: [d('machines[nas].fields', 'theirs'), d('machines[tower]', 'mine')],
    resultText: result((document) => { machine(document, 'nas').name = 'Attic NAS'; }),
    error: null,
  })),

  mergeCase('delete-versus-edit-endpoint', 'Mine deletes the remote endpoint of tower, theirs changes its host. One endpoint, both changed it: asked, and theirs by default.', {
    mine: (document) => machine(document, 'tower').endpoints.pop(),
    theirs: (document) => { endpoint(document, 'tower', 'remote').host = 'desk-tower.example.net'; },
  }, ({ theirsDoc, result }) => ({
    differences: [d('machines[tower].endpoints[remote]', 'both')],
    resultText: result((document) => machine(document, 'tower').endpoints.push(copyOf(endpoint(theirsDoc, 'tower', 'remote')))),
    error: null,
  })),

  mergeCase('theirs-adds-idless-endpoint', 'Theirs adds an endpoint without an id between lan and remote. It is kept, Android dropped it, and lands after its predecessor in theirs.', {
    theirs: (document) => machine(document, 'tower').endpoints.splice(1, 0, { kind: 'lan', host: '192.0.2.23', user: 'me' }),
  }, ({ result }) => ({
    differences: [d('machines[tower].endpoints[]', 'theirs')],
    resultText: result((document) => machine(document, 'tower').endpoints.splice(1, 0, { kind: 'lan', host: '192.0.2.23', user: 'me' })),
    error: null,
  })),

  mergeCase('both-change-idless-endpoints', 'Both sides change the endpoint without an id. The endpoints without an id are one entry, so this is one question; theirs by default replaces mine\'s.', {
    base: (document) => machine(document, 'tower').endpoints.push({ host: '192.0.2.23', user: 'me' }),
    mine: (document) => { machine(document, 'tower').endpoints[2].port = 2222; },
    theirs: (document) => { machine(document, 'tower').endpoints[2].host = '192.0.2.24'; },
  }, ({ result }) => ({
    differences: [d('machines[tower].endpoints[]', 'both')],
    resultText: result((document) => { machine(document, 'tower').endpoints[2] = { host: '192.0.2.24', user: 'me' }; }),
    error: null,
  })),

  mergeCase('unknown-top-level-keys', 'Theirs edits the unknown top-level notes, and both add an unknown extra with different values. Unknown keys are entries like any other: notes from theirs, extra asked.', {
    base: (document) => { document.notes = 'Ask before rebooting tower'; },
    mine: (document) => { document.extra = { color: 'blue' }; },
    theirs: (document) => {
      document.notes = 'Ask before rebooting tower or nas';
      document.extra = { color: 'green' };
    },
  }, ({ result }) => ({
    differences: [d('extra', 'both'), d('notes', 'theirs')],
    resultText: result((document) => {
      document.notes = 'Ask before rebooting tower or nas';
      document.extra = { color: 'green' };
    }),
    error: null,
  })),

  mergeCase('controller-keys', 'Theirs renames the setup and changes an unknown controller key, mine renames nas. Both controller keys are entries taken from theirs; the identity is the merge\'s own.', {
    base: (document) => { document.controller.policy = 'ask'; },
    mine: (document) => { machine(document, 'nas').name = 'Attic NAS'; },
    theirs: (document) => {
      document.controller.name = 'Family setup';
      document.controller.policy = 'merge';
    },
  }, ({ result }) => ({
    differences: [d('controller.name', 'theirs'), d('controller.policy', 'theirs'), d('machines[nas].fields', 'mine')],
    resultText: result((document) => {
      document.controller.name = 'Family setup';
      document.controller.policy = 'merge';
    }),
    error: null,
  })),

  mergeCase('insert-after-predecessor', 'Endpoints lan, wifi, remote. Theirs edits wifi and adds vpn after lan; mine renames tower. The result is lan, vpn, wifi as theirs has it, remote: replaced in place, inserted after its predecessor, never moved to the end.', {
    base: (document) => machine(document, 'tower').endpoints.splice(1, 0, { id: 'wifi', kind: 'lan', host: '192.0.2.25', user: 'me' }),
    mine: (document) => { machine(document, 'tower').name = 'Desk tower'; },
    theirs: (document) => {
      endpoint(document, 'tower', 'wifi').host = '192.0.2.26';
      machine(document, 'tower').endpoints.splice(1, 0, { id: 'vpn', kind: 'remote', host: '198.51.100.27', user: 'me' });
    },
  }, ({ result }) => ({
    differences: [d('machines[tower].endpoints[vpn]', 'theirs'), d('machines[tower].endpoints[wifi]', 'theirs'), d('machines[tower].fields', 'mine')],
    resultText: result((document) => {
      endpoint(document, 'tower', 'wifi').host = '192.0.2.26';
      machine(document, 'tower').endpoints.splice(1, 0, { id: 'vpn', kind: 'remote', host: '198.51.100.27', user: 'me' });
    }),
    error: null,
  })),

  mergeCase('reorder-only', 'Theirs only reorders tower\'s endpoints. A reorder alone is not a difference: nothing is asked, and mine\'s order stays.', {
    theirs: (document) => machine(document, 'tower').endpoints.reverse(),
  }, ({ result }) => ({
    differences: [],
    resultText: result(),
    error: null,
  })),

  mergeCase('number-spelling', 'nas carries "weight": 22.0 on every side and is untouched; mine writes the unknown threshold as 1e2 where theirs keeps 100. Equal numbers are no difference, and nothing not replaced is printed differently: 22.0 and 1e2 stay as written.', {
    base: (document) => {
      machine(document, 'nas').weight = 22;
      document.threshold = 100;
    },
    theirs: (document) => { document.controller.name = 'Family setup'; },
    spell: {
      base: (text) => respell(text, '"weight": 22\n', '"weight": 22.0\n'),
      mine: (text) => respell(respell(text, '"weight": 22\n', '"weight": 22.0\n'), '"threshold": 100\n', '"threshold": 1e2\n'),
      theirs: (text) => respell(text, '"weight": 22\n', '"weight": 22.0\n'),
      result: (text) => respell(respell(text, '"weight": 22\n', '"weight": 22.0\n'), '"threshold": 100\n', '"threshold": 1e2\n'),
    },
  }, ({ result }) => ({
    differences: [d('controller.name', 'theirs')],
    resultText: result((document) => { document.controller.name = 'Family setup'; }),
    error: null,
  })),

  mergeCase('no-base', 'No kept revision is a common ancestor. Nothing says who changed what, so every difference is noBase, shown, and theirs by default: mine\'s rename of tower is undone unless the person keeps it.', {
    noBase: true,
    mine: (document) => { machine(document, 'tower').name = 'Desk tower'; },
    theirs: (document) => { machine(document, 'pi').name = 'Pi'; },
  }, ({ result }) => ({
    differences: [d('machines[pi].fields', 'noBase'), d('machines[tower].fields', 'noBase')],
    resultText: result((document) => {
      machine(document, 'tower').name = 'Tower';
      machine(document, 'pi').name = 'Pi';
    }),
    error: null,
  })),

  mergeCase('long-lineages', 'Mine is revision 40 and theirs 39, each with a lineage of 20. The merge is revision 41, and its lineage is both heads, then the two lineages interleaved mine first, cut at 32.', {
    theirs: (document) => { machine(document, 'nas').name = 'Attic NAS'; },
    stamps: {
      mine: { revision: 40, lineage: (baseHash) => [...LONG.mine, baseHash] },
      theirs: { revision: 39, lineage: (baseHash) => [...LONG.theirs, baseHash] },
      lineage: ({ mineHash, theirsHash }) => [mineHash, theirsHash, ...LONG.stated],
    },
  }, ({ result }) => ({
    differences: [d('machines[nas].fields', 'theirs')],
    resultText: result((document) => { machine(document, 'nas').name = 'Attic NAS'; }),
    error: null,
  })),

  mergeCase('keep-mine', 'Keep mine is a merge with every entry answered mine, including the ones theirs alone changed: mine\'s content, under a new revision whose lineage holds theirs\' head, so theirs fast-forwards onto it.', {
    mine: (document) => { machine(document, 'tower').name = 'Desk tower'; },
    theirs: (document) => {
      machine(document, 'pi').name = 'Pi';
      removeMachine(document, 'nas');
    },
    choices: { 'machines[nas]': 'mine', 'machines[pi].fields': 'mine', 'machines[tower].fields': 'mine' },
  }, ({ result }) => ({
    differences: [d('machines[nas]', 'theirs'), d('machines[pi].fields', 'theirs'), d('machines[tower].fields', 'mine')],
    resultText: result(),
    error: null,
  })),

  mergeCase('choice-breaks-a-rule', 'Theirs makes pi a helper of tower and renames it; mine deletes pi, and the person keeps the deletion. The result would name a helper that does not exist, so nothing is written and the rule is named.', {
    base: (document) => { machine(document, 'tower').wake = { mac: '02:00:00:00:00:01' }; },
    mine: (document) => removeMachine(document, 'pi'),
    theirs: (document) => {
      Object.assign(machine(document, 'tower').wake, { helper: { machine: 'pi', action: 'wake-tower' }, helpers: [{ machine: 'pi', action: 'wake-tower' }] });
      machine(document, 'pi').name = 'Pi';
    },
    choices: { 'machines[pi]': 'mine' },
  }, () => ({
    differences: [d('machines[pi]', 'both'), d('machines[tower].wake', 'theirs')],
    resultText: null,
    error: 'helper-unknown',
  })),

  mergeCase('theirs-adds-site', 'Theirs adds a site and moves nas to it. The new site is inserted after its predecessor and the machine\'s own fields are taken with its new site.', {
    theirs: (document) => {
      document.sites.push({ id: 'office', name: 'Office', lanPrefixes: ['192.168.10.'] });
      machine(document, 'nas').site = 'office';
    },
  }, ({ result }) => ({
    differences: [d('machines[nas].fields', 'theirs'), d('sites[office]', 'theirs')],
    resultText: result((document) => {
      document.sites.push({ id: 'office', name: 'Office', lanPrefixes: ['192.168.10.'] });
      machine(document, 'nas').site = 'office';
    }),
    error: null,
  })),

  mergeCase('both-add-machines', 'Theirs adds laptop after tower, mine adds studio at the end. Each lands where its side put it.', {
    mine: (document) => document.machines.push({ id: 'studio', name: 'Studio', site: 'home', endpoints: [lan('192.0.2.31')], systems: [linux('Linux')] }),
    theirs: (document) => document.machines.splice(1, 0, { id: 'laptop', name: 'Laptop', site: 'home', endpoints: [lan('192.0.2.30')], systems: [linux('Linux')] }),
  }, ({ result }) => ({
    differences: [d('machines[laptop]', 'theirs'), d('machines[studio]', 'mine')],
    resultText: result((document) => document.machines.splice(1, 0, { id: 'laptop', name: 'Laptop', site: 'home', endpoints: [lan('192.0.2.30')], systems: [linux('Linux')] })),
    error: null,
  })),

  mergeCase('theirs-deletes-endpoint', 'Theirs deletes an endpoint mine left alone, mine renames pi: the deletion is taken.', {
    mine: (document) => { machine(document, 'pi').name = 'Pi'; },
    theirs: (document) => machine(document, 'tower').endpoints.pop(),
  }, ({ result }) => ({
    differences: [d('machines[pi].fields', 'mine'), d('machines[tower].endpoints[remote]', 'theirs')],
    resultText: result((document) => machine(document, 'tower').endpoints.pop()),
    error: null,
  })),

  mergeCase('both-edit-system-keep-mine', 'Both rename tower\'s system; the person keeps mine.', {
    mine: (document) => { machine(document, 'tower').systems[0].name = 'CachyOS'; },
    theirs: (document) => { machine(document, 'tower').systems[0].name = 'Arch Linux'; },
    choices: { 'machines[tower].systems[linux]': 'mine' },
  }, ({ result }) => ({
    differences: [d('machines[tower].systems[linux]', 'both')],
    resultText: result(),
    error: null,
  })),

  mergeCase('theirs-removes-unknown-key', 'Theirs removes the unknown top-level notes mine left alone: removed in the result too.', {
    base: (document) => { document.notes = 'Ask before rebooting tower'; },
    mine: (document) => { machine(document, 'pi').name = 'Pi'; },
    theirs: (document) => { delete document.notes; },
  }, ({ result }) => ({
    differences: [d('machines[pi].fields', 'mine'), d('notes', 'theirs')],
    resultText: result((document) => { delete document.notes; }),
    error: null,
  })),

  mergeCase('wake-and-fields-separately', 'Mine changes tower\'s wake ports, theirs renames tower. A machine both sides have merges part by part, so both edits are kept.', {
    mine: (document) => { machine(document, 'tower').wake.ports = [9]; },
    theirs: (document) => { machine(document, 'tower').name = 'Desk tower'; },
  }, ({ result }) => ({
    differences: [d('machines[tower].fields', 'theirs'), d('machines[tower].wake', 'mine')],
    resultText: result((document) => { machine(document, 'tower').name = 'Desk tower'; }),
    error: null,
  })),

  mergeCase('theirs-adds-first-system', 'nas has no systems key; theirs gives it one. The array mine never had is created, appended to the machine, for the system that goes in it.', {
    base: (document) => { delete machine(document, 'nas').systems; },
    theirs: (document) => { machine(document, 'nas').systems = [linux('Debian')]; },
  }, ({ result }) => ({
    differences: [d('machines[nas].systems[linux]', 'theirs')],
    resultText: result((document) => { machine(document, 'nas').systems = [linux('Debian')]; }),
    error: null,
  })),
];

// ---------------------------------------------------------------------------
// Bases.
// ---------------------------------------------------------------------------

/** A kept revision: the tower setup at revision n, so every kept text is a real document with its own hash. */
const keptText = (revision) => documentText({ ...copyOf(TOWER_DOC), controller: { ...TOWER_DOC.controller, revision } });
const A1 = keptText(1);
const A2 = keptText(2);
const A3 = keptText(3);
const M4 = keptText(4);
const hashOf = (text) => canonicalHash(text);
const kept = (...texts) => Object.fromEntries(texts.map((text) => [hashOf(text), text]));

const based = (name, purpose, input, base) => ({ name, purpose, tags: C6, input, want: { base } });

const bases = [
  based('newest-common-ancestor', 'The first hash of mine\'s lineage that is also in theirs\' and whose kept bytes hash to it.', {
    mineHash: hashOf(M4), theirsHash: standIn('theirs 3'),
    mineLineage: [hashOf(A3), hashOf(A2), hashOf(A1)], theirsLineage: [standIn('theirs 3 parent'), hashOf(A2), hashOf(A1)],
    kept: kept(A3, A2, A1),
  }, hashOf(A2)),
  based('newest-common-not-kept', 'The newest common ancestor is no longer kept here, so the next one is the base.', {
    mineHash: hashOf(M4), theirsHash: standIn('theirs 3'),
    mineLineage: [hashOf(A3), hashOf(A2), hashOf(A1)], theirsLineage: [standIn('theirs 3 parent'), hashOf(A2), hashOf(A1)],
    kept: kept(A3, A1),
  }, hashOf(A1)),
  based('kept-bytes-changed', 'The bytes kept under the newest common hash no longer hash to it, so they are not that revision and the next common one is the base.', {
    mineHash: hashOf(M4), theirsHash: standIn('theirs 3'),
    mineLineage: [hashOf(A3), hashOf(A2), hashOf(A1)], theirsLineage: [standIn('theirs 3 parent'), hashOf(A2), hashOf(A1)],
    kept: { ...kept(A3, A1), [hashOf(A2)]: A3 },
  }, hashOf(A1)),
  based('no-common-ancestor', 'Nothing in common: there is no base, and every difference is noBase.', {
    mineHash: hashOf(M4), theirsHash: standIn('theirs 3'),
    mineLineage: [hashOf(A3), hashOf(A2), hashOf(A1)], theirsLineage: [standIn('theirs 3 parent'), standIn('theirs 3 grandparent')],
    kept: kept(A3, A2, A1),
  }, null),
  based('head-is-not-a-base', 'Theirs descends from mine\'s head and mine\'s bytes are kept, but a head is never a base: the base is the newest common ancestor of the two lineages.', {
    mineHash: hashOf(M4), theirsHash: standIn('theirs 5'),
    mineLineage: [hashOf(A2), hashOf(A1)], theirsLineage: [hashOf(M4), hashOf(A2), hashOf(A1)],
    kept: kept(M4, A2, A1),
  }, hashOf(A2)),
];

// ---------------------------------------------------------------------------
// Numbers.
// ---------------------------------------------------------------------------

const compared = (name, purpose, a, b, equal) => ({ name, purpose, tags: C6, input: { a, b }, want: { equal } });

const numbers = [
  compared('integer-and-decimal', '22 and 22.0 are one number: the Mac printed the second as the first.', '22', '22.0', true),
  compared('integer-and-exponent', '22 and 2.2e1 are one number.', '22', '2.2e1', true),
  compared('negative-exponent', '22.0 and 220e-1 are one number.', '22.0', '220e-1', true),
  compared('zero-and-negative-zero', '0 and -0 are one number.', '0', '-0', true),
  compared('exponent-and-integer', '1e2 and 100 are one number.', '1e2', '100', true),
  compared('capital-exponent-and-fraction', '1E+2 and 100.00 are one number.', '1E+2', '100.00', true),
  compared('close-decimals', '0.1 and 0.10000000000000001 are two numbers, though a double cannot tell them apart.', '0.1', '0.10000000000000001', false),
  compared('large-integers', '9007199254740993 and 9007199254740992 are two numbers, though a double rounds the first to the second.', '9007199254740993', '9007199254740992', false),
  compared('large-integer-and-exponent', 'A 30-digit integer and the same value written with an exponent are one number.', '123456789012345678901234567890', '1.2345678901234567890123456789e29', true),
  compared('sign', '-1 and 1 are two numbers.', '-1', '1', false),
];

export default {
  family: 'setup-merge',
  section: '9.8',
  reference: 'tools/setup-sync.mjs',
  notes: [
    'merges: baseText is the base the device chose (bases gives the rule), or null when there is none. mineHash and theirsHash are the canonical hashes of mineText and theirsText. choices maps an entry key to mine or theirs; an entry it does not name takes mine when its kind is mine and theirs otherwise.',
    'merges: differences lists every entry where mine and theirs differ, in code-point order of the key, with its kind: mine or theirs (with a base, the side that changed it), both (with a base, both changed it, a deletion against an edit included) or noBase.',
    'merges: resultText is the merged document, or null when section 9.7 refuses it under peer intake; error is then the first of its error codes in code-point order. Compare resultText as a parsed document: object key order ignored, array order exact, number lexemes exact.',
    'Entry keys: <key> for a top-level key other than controller, machines and sites; controller.<key> for a controller key other than id, revision, updatedAt, source, device and lineage; sites[<id>]; machines[<id>] when only one side has the machine; machines[<id>].fields, .wake, .systems[<id>], .endpoints[<id>] and .endpoints[] (the endpoints without an id, in order, as one value) when both have it.',
    'bases: kept maps a hash to the text this device kept for it. The base is the first hash of mineLineage that is in theirsLineage and whose kept text hashes to it, or null.',
    'numbers: a and b are JSON number lexemes, equal when they are the same decimal value.',
  ],
  consumers: { merges: CLIENTS, bases: CLIENTS, numbers: CLIENTS },
  groups: { merges, bases, numbers },
};
