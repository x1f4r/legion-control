// Document acceptance, v3.md section 9.7, as cases.
//
// Almost every case is the small tower setup of documents.mjs with exactly one
// thing changed, so the one thing changed is the only thing the verdict can be
// about. There is a case for every error code, for every schema constraint a
// client editor or decoder is likely to get wrong, and for every warning on its
// own, because a warning is precisely what a client used to turn into a refusal:
// Android refused a machine with no systems yet and an endpoint with no id, so a
// setup the Mac wrote stopped replicating at the phone (C3). The guard cases are
// the hosts, users, argv items and repositories that try to be syntax somewhere
// further down the line (T5, T6); the agent refuses them too and is held to that
// through the agent-enforced tag.

import { SETUP_ID, TOWER_DOC, copyOf, documentText } from '../documents.mjs';

const EVERYONE = ['agent', 'mac', 'desktop', 'android'];
const CLIENTS = ['mac', 'desktop', 'android'];

const LAUNCHER = '/home/me/.legion-control/bin/launcher.mjs';
const PI = Object.freeze({
  id: 'pi',
  name: 'Beacon',
  alwaysOn: true,
  endpoints: [{ id: 'lan', kind: 'lan', host: '192.0.2.40', user: 'me', port: 22 }],
  systems: [{ id: 'linux', name: 'Linux', platform: 'linux', shell: 'posix', agent: ['/usr/bin/node', LAUNCHER] }],
});

/** The tower setup, changed by `change`, as the text a client reads. */
const tower = (change = () => {}) => {
  const document = copyOf(TOWER_DOC);
  change(document);
  return documentText(document);
};
const towerAndPi = (change = () => {}) => tower((document) => {
  document.machines.push(copyOf(PI));
  change(document);
});
const first = (document) => document.machines[0];
const helper = (machine, action = 'wake-tower') => ({ machine, action });

/**
 * A case. The agent verifies peer cases: its own checks never refuse a valid
 * one and always refuse one tagged agent-enforced. The other intakes are
 * client behaviour.
 */
const acceptance = (name, intake, text, want, purpose, tags = []) => ({
  name, purpose, tags,
  consumers: intake === 'peer' ? EVERYONE : CLIENTS,
  input: { intake, text },
  want,
});
const peer = (name, text, want, purpose, tags) => acceptance(name, 'peer', text, want, purpose, tags);
const refused = (...errors) => ({ valid: false, errors, warnings: [], keep: false });
const accepted = (...warnings) => ({ valid: true, errors: [], warnings, keep: true });
const ENFORCED = 'agent-enforced';

const hash = (n) => n.toString(16).padStart(64, '0');

/** The tower text with a second copy of a system's agent member written in front of the first. */
const repeatedAgent = () => {
  const text = tower();
  const find = '"agent": [';
  if (text.split(find).length !== 2) throw new Error('the tower document no longer has exactly one agent member');
  return text.replace(find, '"agent": ["/usr/bin/node", "/home/me/other.mjs"],\n          "agent": [');
};

const errorCases = [
  peer('duplicate-member', repeatedAgent(), refused('duplicate-member'),
    'A system that writes agent twice. Parsers disagree about which copy counts, so the document means different things on different devices; this is checked first, on the text, and no other rule is reported.', [ENFORCED]),
  peer('duplicate-machine', tower((document) => document.machines.push({ ...copyOf(first(document)), name: 'Second tower' })), refused('duplicate-machine'),
    'Two machines share the id tower, so a route, a record and a binding naming it are ambiguous.', [ENFORCED]),
  peer('duplicate-site', tower((document) => { document.sites = [{ id: 'home', name: 'Home' }, { id: 'home', name: 'Also home' }]; }), refused('duplicate-site'),
    'Two sites share the id home.', [ENFORCED]),
  peer('duplicate-system', tower((document) => first(document).systems.push({ id: 'linux', name: 'Linux again' })), refused('duplicate-system'),
    'Two systems of one machine share the id linux.', [ENFORCED]),
  peer('duplicate-endpoint', tower((document) => first(document).endpoints.push({ id: 'lan', kind: 'lan', host: '192.0.2.20', user: 'me' })), refused('duplicate-endpoint'),
    'Two endpoints of one machine share the id lan, so a remembered route is ambiguous.', [ENFORCED]),
  peer('unknown-site', tower((document) => {
    document.sites = [{ id: 'attic', name: 'Attic house', lanPrefixes: ['192.168.50.'] }];
    first(document).site = 'flat';
  }), refused('unknown-site'),
  'A machine names a site that sites[] does not define (fixture controller-document.invalid-unknown-site.json).', [ENFORCED]),
  peer('helper-self', tower((document) => {
    first(document).wake = { mac: '02:00:00:00:00:01', helper: helper('tower'), helpers: [helper('tower')] };
  }), refused('helper-self'),
  'A machine is its own wake helper (fixture controller-document.invalid-self-helper.json).', [ENFORCED]),
  peer('helper-unknown', tower((document) => {
    first(document).wake = { mac: '02:00:00:00:00:01', helper: helper('old-box'), helpers: [helper('old-box')] };
  }), refused('helper-unknown'),
  'A wake helper names a machine the setup does not have.', [ENFORCED]),
  peer('helper-alias', towerAndPi((document) => {
    document.machines.push({ ...copyOf(PI), id: 'nas', name: 'Attic NAS' });
    first(document).wake = { mac: '02:00:00:00:00:01', helper: helper('nas'), helpers: [helper('pi'), helper('nas')] };
  }), refused('helper-alias'),
  'wake.helper names nas and wake.helpers[0] names pi, so a 1.2 client and a 1.4 client try different machines (fixture controller-document.invalid-helper-mismatch.json).', [ENFORCED]),
  peer('helper-cycle', towerAndPi((document) => {
    first(document).wake = { mac: '02:00:00:00:00:01', helper: helper('pi'), helpers: [helper('pi')] };
    document.machines[1].wake = { mac: '02:00:00:00:00:02', helper: helper('tower', 'wake-pi'), helpers: [helper('tower', 'wake-pi')] };
  }), refused('helper-cycle'),
  'Two machines wake each other, so each waits for the other to be awake first (fixture controller-document.invalid-helper-cycle.json).', [ENFORCED]),
  peer('helper-cycle-through-alias', towerAndPi((document) => {
    first(document).wake = { mac: '02:00:00:00:00:01', helper: helper('pi') };
    document.machines[1].wake = { mac: '02:00:00:00:00:02', helper: helper('tower', 'wake-pi') };
  }), refused('helper-cycle'),
  'The same ring written only with the singular helper key, as a 1.2 editor writes it. The alias is a helper too.', [ENFORCED]),
  peer('helper-self-beside-empty-helpers', tower((document) => {
    first(document).wake = { mac: '02:00:00:00:00:01', helper: helper('tower'), helpers: [] };
  }), refused('helper-self'),
  'The alias beside an empty helpers list still names a helper, because a 1.2 client runs it. Reading helpers ?? [helper] skips it and stores a document every client refuses.', [ENFORCED]),
];

const machine = (n) => ({
  id: `m${String(n).padStart(2, '0')}`,
  endpoints: [{ id: 'lan', host: `192.0.2.${n + 1}`, user: 'me' }],
  systems: [{ id: 'linux', agent: ['/usr/bin/node', LAUNCHER] }],
});

const schemaCases = [
  peer('schema-version-2', tower((document) => { document.version = 2; }), refused('schema'),
    'version stays 1: a bump would make every 1.2 client refuse documents for keys it can ignore.', [ENFORCED]),
  peer('schema-version-missing', tower((document) => { delete document.version; }), refused('schema'),
    'A document without version is refused; section 12 already refuses it before any rule is read.', [ENFORCED]),
  peer('schema-51-machines', tower((document) => { document.machines = Array.from({ length: 51 }, (_, n) => machine(n)); }), refused('schema'),
    'At most 50 machines.', [ENFORCED]),
  peer('schema-21-endpoints', tower((document) => {
    first(document).endpoints = Array.from({ length: 21 }, (_, n) => ({ id: `e${n}`, host: `192.0.2.${n + 1}`, user: 'me' }));
  }), refused('schema'),
  'At most 20 endpoints on a machine.', [ENFORCED]),
  peer('schema-11-systems', tower((document) => {
    first(document).systems = Array.from({ length: 11 }, (_, n) => ({ id: `s${n}`, agent: ['/usr/bin/node', LAUNCHER] }));
  }), refused('schema'),
  'At most 10 systems on a machine.', [ENFORCED]),
  peer('schema-21-sites', tower((document) => {
    document.sites = Array.from({ length: 21 }, (_, n) => ({ id: `site${n}`, name: `Site ${n}` }));
  }), refused('schema'),
  'At most 20 sites in one setup.', [ENFORCED]),
  peer('schema-6-helpers', towerAndPi((document) => {
    first(document).wake = { mac: '02:00:00:00:00:01', helpers: Array.from({ length: 6 }, (_, n) => helper('pi', `wake-tower-${n}`)) };
  }), refused('schema'),
  'At most 5 wake helpers.', [ENFORCED]),
  peer('schema-empty-lan-prefixes', tower((document) => { document.sites = [{ id: 'home', lanPrefixes: [] }]; }), refused('schema'),
    'lanPrefixes, when present, has at least one entry; editors omit it instead of writing an empty list.'),
  peer('schema-port-0', tower((document) => { first(document).endpoints[0].port = 0; }), refused('schema'),
    'Port 0 is not a port.', [ENFORCED]),
  peer('schema-port-65536', tower((document) => { first(document).endpoints[0].port = 65536; }), refused('schema'),
    'One over the highest port.', [ENFORCED]),
  peer('schema-port-string', tower((document) => { first(document).endpoints[0].port = '22'; }), refused('schema'),
    'A port is a JSON integer, never a string.', [ENFORCED]),
  peer('schema-bad-mac', tower((document) => { first(document).wake = { mac: '02:00:00:00:00' }; }), refused('schema'),
    'A MAC address has six groups.', [ENFORCED]),
  peer('schema-source-android', tower((document) => { document.controller.source = 'android'; }), refused('schema'),
    'source is the kind of client (mac, desktop, phone, cli, legacy), not a platform name; the device name goes in device.'),
  peer('schema-setup-id-65', tower((document) => { document.controller.id = 'a'.repeat(65); }), refused('schema'),
    'A setup id is at most 64 characters.', [ENFORCED]),
  peer('schema-setup-id-at', tower((document) => { document.controller.id = 'a@b'; }), refused('schema'),
    'An @ is in the generic token grammar and not in a setup id, which the agent refuses with bad-argument (C10).', [ENFORCED, 'finding:c10']),
  peer('schema-helper-action-colon', towerAndPi((document) => {
    first(document).wake = { mac: '02:00:00:00:00:01', helper: helper('pi', 'a:b'), helpers: [helper('pi', 'a:b')] };
  }), refused('schema'),
  'A wake action is a configured id, which has no colon.', [ENFORCED]),
  peer('schema-lineage-33', tower((document) => { document.controller.lineage = Array.from({ length: 33 }, (_, n) => hash(n + 1)); }), refused('schema'),
    'A lineage holds at most 32 hashes.'),
  peer('schema-lineage-duplicate', tower((document) => { document.controller.lineage = [hash(1), hash(1)]; }), refused('schema'),
    'A document cannot descend from one ancestor twice.'),
  peer('schema-lineage-uppercase', tower((document) => { document.controller.lineage = [hash(0xabcdef).toUpperCase()]; }), refused('schema'),
    'Lineage hashes are lowercase hex, the form every client computes.'),
  peer('schema-bad-repo', tower((document) => { document.appUpdates = { githubRepo: 'owner' }; }), refused('schema'),
    'A repository is owner/name.', [ENFORCED, 'finding:t6']),
  peer('schema-controller-without-revision', tower((document) => { delete document.controller.revision; }), refused('schema'),
    'A controller block has an id and a revision.'),
  peer('schema-machine-without-id', tower((document) => { delete first(document).id; }), refused('schema'),
    'Every machine has an id.', [ENFORCED]),
];

const warningCases = [
  peer('warn-no-machines', tower((document) => { document.machines = []; }), accepted('no-machines'),
    'A setup with no machines yet is a setup: it is how one starts.', ['finding:c3']),
  peer('warn-machine-no-systems', tower((document) => { delete first(document).systems; }), accepted('machine-no-systems'),
    'A machine with no systems yet is accepted and not driven; Android used to refuse the whole setup for it (C3).', ['finding:c3']),
  peer('warn-machine-no-endpoints', tower((document) => { delete first(document).endpoints; }), accepted('machine-no-endpoints'),
    'A machine nobody can reach yet is accepted.', ['finding:c3']),
  peer('warn-system-no-agent', tower((document) => { delete first(document).systems[0].agent; }), accepted('system-no-agent'),
    'A system without an agent command is not driven, and that is a warning.'),
  peer('warn-endpoint-no-id', tower((document) => { delete first(document).endpoints[0].id; }), accepted('endpoint-no-id'),
    'An endpoint without an id cannot be remembered as a route; it is still used.', ['finding:c3']),
  peer('warn-endpoint-no-user', tower((document) => { delete first(document).endpoints[0].user; }), accepted('endpoint-no-user'),
    'An endpoint without a user logs in as whatever the device\'s own ssh configuration says.', ['finding:c3']),
  peer('warn-endpoint-unknown-system', tower((document) => { first(document).endpoints[0].system = 'windows'; }), accepted('endpoint-unknown-system'),
    'An endpoint hint naming a system the machine does not have is ignored, not refused.', ['finding:c3']),
  peer('host-all-digits-undialable', tower((document) => { first(document).endpoints[0].host = '1234'; }), accepted('endpoint-undialable'),
    'A host of digits passes the document guard but is an integer address to a resolver; the entry is skipped, never dialled.', ['finding:t5']),
  peer('host-trailing-dot-undialable', tower((document) => { first(document).endpoints[0].host = 'tower.example.'; }), accepted('endpoint-undialable'),
    'A trailing dot passes the guard and fails the dial grammar.', ['finding:t5']),
  peer('legacy-ssh-host-undialable', tower((document) => {
    delete first(document).endpoints;
    first(document).ssh = { host: '1.2.3', user: 'me' };
  }), accepted('endpoint-undialable'),
  'The deprecated machine.ssh host is held to the dial grammar too; with it the machine has a host, so it is not missing endpoints.', ['finding:t5']),
  peer('warn-wake-no-mac', towerAndPi((document) => { first(document).wake = { helper: helper('pi'), helpers: [helper('pi')] }; }), accepted('wake-no-mac'),
    'A wake block with helpers and no MAC: helpers still work, direct wake does not.'),
  peer('warn-wake-no-mac-at-site', tower((document) => {
    document.sites = [{ id: 'home', name: 'Home', lanPrefixes: ['192.168.1.'] }];
    first(document).site = 'home';
    first(document).wake = { ports: [9] };
  }), accepted('wake-no-mac'),
  'A wake block at a site with no broadcast anywhere, and no MAC address: nothing is woken directly, so no broadcast is defaulted either.'),
  peer('warn-wake-unplaced', tower((document) => { first(document).wake = { mac: '02:00:00:00:00:01' }; }), accepted('wake-unplaced'),
    'A MAC address with no site, no lanPrefix and no helper: no client can tell when it is on the machine\'s network.'),
  peer('warn-wake-lan-prefix-invalid', tower((document) => { first(document).wake = { mac: '02:00:00:00:00:01', lanPrefix: '192.168.1.5' }; }), accepted('prefix-invalid', 'wake-unplaced'),
    'A wake.lanPrefix that is a full address never matches, so it places nothing: the block is as unplaced as one without lanPrefix.'),
  peer('warn-wake-broadcast-defaulted', tower((document) => { first(document).wake = { mac: '02:00:00:00:00:01', lanPrefix: '192.168.1.' }; }), accepted('wake-broadcast-defaulted'),
    'A machine that can be woken directly with no broadcast address: clients derive one, which Android used to refuse outright.', ['finding:c3']),
  peer('warn-helper-alias-unused', towerAndPi((document) => {
    first(document).wake = { mac: '02:00:00:00:00:01', lanPrefix: '192.168.1.', broadcast: ['192.168.1.255'], helper: helper('pi'), helpers: [] };
  }), accepted('helper-alias-unused'),
  'wake.helper beside an empty wake.helpers: clients disagree about whether it counts.'),
  peer('warn-helper-other-site', towerAndPi((document) => {
    document.sites = [
      { id: 'home', name: 'Home', lanPrefixes: ['192.168.1.'], broadcast: ['192.168.1.255'] },
      { id: 'office', name: 'Office', lanPrefixes: ['192.168.10.0/24'] },
    ];
    first(document).site = 'home';
    document.machines[1].site = 'office';
    first(document).wake = { mac: '02:00:00:00:00:01', helper: helper('pi'), helpers: [helper('pi')] };
  }), accepted('helper-other-site'),
  'A helper at another site works only if its action reaches the target\'s network some other way.'),
  peer('warn-helper-not-always-on', towerAndPi((document) => {
    delete document.machines[1].alwaysOn;
    first(document).wake = { mac: '02:00:00:00:00:01', helper: helper('pi'), helpers: [helper('pi')] };
  }), accepted('helper-not-always-on'),
  'A helper not marked alwaysOn may itself need waking first.'),
  peer('warn-shared-prefix', tower((document) => {
    document.sites = [{ id: 'home', lanPrefixes: ['192.168.0.0/16'] }, { id: 'office', lanPrefixes: ['192.168.10.'] }];
  }), accepted('shared-prefix'),
  'Two sites whose prefixes overlap: matching one does not say which site a device is on. The prefixes are compared parsed, not as text.'),
  peer('warn-prefix-invalid', tower((document) => { document.sites = [{ id: 'home', lanPrefixes: ['192.168.1.5'] }]; }), accepted('prefix-invalid'),
    'A full address is not a prefix: it never matches, and the setup is not refused for it.'),
  peer('warn-prefix-without-dot', tower((document) => { document.sites = [{ id: 'home', lanPrefixes: ['192.168.1'] }]; }), accepted('prefix-without-dot'),
    'Without the trailing dot, a client that predates section 10.1 also matches 192.168.10.x.'),
  peer('warn-broadcast-invalid', tower((document) => {
    document.sites = [{ id: 'home', lanPrefixes: ['192.168.1.'], broadcast: ['studio.local'] }];
  }), accepted('broadcast-invalid'),
  'A broadcast entry that is a name is skipped and never resolved.'),
];

const intakeCases = [
  peer('system-empty-agent', tower((document) => { first(document).systems[0].agent = []; }), accepted('system-no-agent'),
    'An empty agent is the same as none: the system is not driven, and the document is valid.'),
  peer('partial-machines', towerAndPi((document) => {
    const [tower, pi] = document.machines;
    tower.endpoints[0].system = 'windows';
    pi.endpoints = [{ host: '192.0.2.40' }];
    delete pi.systems;
    document.machines.push({
      id: 'nas',
      name: 'Attic NAS',
      systems: [{ id: 'linux', name: 'Debian' }],
      wake: { mac: '02:00:00:00:00:02', lanPrefix: '192.168.1.' },
    });
  }), accepted('endpoint-no-id', 'endpoint-no-user', 'endpoint-unknown-system', 'machine-no-endpoints', 'machine-no-systems', 'system-no-agent', 'wake-broadcast-defaulted'),
  'A setup half way through being written, with every gap Android used to refuse: no systems, no endpoints, an endpoint without id or user, a hint to a system that does not exist, a wake block without broadcast. All warnings, no errors (C3).', ['finding:c3']),
  peer('site-named-without-sites', tower((document) => { first(document).site = 'home'; }), accepted(),
    'A site name is only checked against a sites list that has entries; a setup without sites places nobody.'),
  peer('endpoints-without-ids-are-not-duplicates', tower((document) => {
    for (const endpoint of first(document).endpoints) delete endpoint.id;
    first(document).endpoints.push({ kind: 'remote', host: 'tower.example.net', user: 'me' });
  }), accepted('endpoint-no-id'),
  'Two endpoints without an id are never duplicates of each other; the desktop counted them as such.'),
  peer('canonical-bytes-first', `\ufeff${tower().replace(/\n/g, '\r\n')}\r\n\r\n`, accepted(),
    'A byte order mark, CRLF line endings and trailing blank lines: acceptance reads the canonical bytes, so none of it matters.'),
  acceptance('local-without-controller', 'local', tower((document) => { delete document.controller; }),
    { valid: true, errors: [], assignsIdentity: true, keep: true },
    'A pasted document with no controller block gets a fresh setup id, revision 1 and an empty lineage before it is checked.'),
  acceptance('local-controller-without-id', 'local', tower((document) => { document.controller = { name: 'Home setup', source: 'phone' }; }),
    { valid: true, errors: [], assignsIdentity: true, keep: true },
    'A controller block without an id is given one too, keeping its other keys.'),
  acceptance('local-controller-id-null', 'local', tower((document) => { document.controller = { id: null, name: 'Home setup', source: 'phone' }; }),
    { valid: true, errors: [], assignsIdentity: true, keep: true },
    'An id of null is no id: an editor that clears the field writes null, and the document is given an identity rather than refused for it.'),
  acceptance('local-identity-then-checked', 'local', tower((document) => {
    delete document.controller;
    document.machines.push(copyOf(first(document)));
  }), { valid: false, errors: ['duplicate-machine'], assignsIdentity: true, keep: false },
  'Local intake assigns the identity and then checks everything else: a pasted document with a duplicate machine is still refused.'),
  acceptance('local-with-identity', 'local', tower(), { valid: true, errors: [], assignsIdentity: false, keep: true },
    'A local document that already has an id keeps it.'),
  peer('peer-without-controller', tower((document) => { delete document.controller; }), { valid: true, assignsIdentity: false },
    'The text of local-without-controller read from a peer. A machine may hold a document without a controller block (a 2.x copy); under peer intake it is valid and nothing is assigned.'),
  peer('peer-controller-without-id', tower((document) => { document.controller = { name: 'Home setup', source: 'phone' }; }), { ...refused('schema'), assignsIdentity: false },
    'The text of local-controller-without-id read from a peer: a controller block without an id breaks the schema, because only local intake assigns one.'),
  peer('host-comma-star', tower((document) => { first(document).endpoints[0].host = 'victim.example,*'; }), refused('schema'),
    'A comma and a star make a known_hosts pattern that pins one key for every host (fixture controller-document.invalid-host.json).', [ENFORCED, 'finding:t5']),
  peer('host-leading-dash', tower((document) => { first(document).endpoints[0].host = '-oProxyCommand=x'; }), refused('schema'),
    'A host that starts with a hyphen is an ssh option (fixture controller-document.invalid-host-option.json).', [ENFORCED, 'finding:t5']),
  peer('user-option', tower((document) => { first(document).endpoints[0].user = '-oProxyCommand=x'; }), refused('schema'),
    'A user that starts with a hyphen is an ssh option too.', [ENFORCED, 'finding:t5']),
  peer('user-substitution', tower((document) => { first(document).endpoints[0].user = 'me$(id)'; }), refused('schema'),
    'A command substitution in a user (fixture controller-document.invalid-user.json).', [ENFORCED, 'finding:t5']),
  peer('agent-argv-empty-item', tower((document) => { first(document).systems[0].agent = ['node', '']; }), refused('schema'),
    'An empty argv item vanishes or becomes an empty argument depending on the shell (fixture controller-document.invalid-agent-argv.json).', [ENFORCED, 'finding:t5']),
  peer('agent-argv-bell-character', tower((document) => { first(document).systems[0].agent = ['node', `${LAUNCHER}\u0007`]; }), refused('schema'),
    'A control character in an argv item (fixture controller-document.invalid-agent-control.json).', [ENFORCED, 'finding:t5']),
  peer('agent-argv-del-character', tower((document) => { first(document).systems[0].agent = ['node', `${LAUNCHER}\u007f`]; }), refused('schema'),
    'U+007F is a control character as well.', [ENFORCED, 'finding:t5']),
  peer('agent-argv-4097-bytes', tower((document) => { first(document).systems[0].agent = ['node', `${'é'.repeat(2048)}a`]; }), refused('schema'),
    '2049 characters but 4097 UTF-8 bytes: the argv-item limit is counted in bytes, which a schema cannot, so it is checked in code.', [ENFORCED, 'finding:t5']),
  peer('repo-dotdot', tower((document) => { document.appUpdates = { githubRepo: 'owner/..' }; }), refused('schema'),
    'A repository named .. turns every client\'s update URL into a path out of the owner (fixture controller-document.invalid-repo.json).', [ENFORCED, 'finding:t6']),
  acceptance('applied-legacy-host-comma', 'applied', tower((document) => {
    first(document).ssh = { host: 'victim.example,*', user: 'me' };
  }), { valid: false, errors: ['schema'], warnings: [], keep: true },
  'The document this device already applies, written before the guards existed, with a pattern in the deprecated ssh host. It is kept and shown with its error; that machine is disabled, and publishing is held until it is fixed.'),
  acceptance('applied-version-2', 'applied', tower((document) => { document.version = 2; }),
    { valid: false, errors: ['schema'], keep: true },
    'An applied document that breaks the schema is still kept, because section 12 accepts its bytes.'),
  acceptance('applied-duplicate-member', 'applied', repeatedAgent(),
    { valid: false, errors: ['duplicate-member'], keep: true },
    'An applied document with a repeated member name is kept and shown with its error.'),
  acceptance('applied-not-json', 'applied', 'this was a setup file once\n',
    { valid: false, errors: ['schema'], keep: false, assignsIdentity: false },
    'Text that section 12 refuses is not a document at all: even applied intake cannot keep it.'),
  acceptance('applied-valid', 'applied', tower(), { valid: true, errors: [], keep: true },
    'A valid applied document, as a client reads it at start.'),
];

export default {
  family: 'setup-acceptance',
  section: '9.7',
  reference: 'tools/document-rules.mjs',
  notes: [
    'input.text is the document as a client reads it; it is checked as UTF-8 bytes. errors and warnings are sorted, unique codes from section 9.7. Warnings are only reported for a document without errors.',
    'Order: section 12 first, where a refused text is the error schema under every intake and keep is false; then a repeated member name on the canonical text, reported alone; then, for local intake, the identity; then the schema and the rules.',
    'keep is valid for peer and local intake. For applied intake it is true unless section 12 refuses the text.',
    'assignsIdentity is true for local intake when the document has no controller block or a block whose id is absent or null; the document is then checked with a fresh setup id, revision 1 and an empty lineage.',
    `Cases tagged agent-enforced are refused by agent 3.1.0 on config set as well. The base document is the tower setup with setup id ${SETUP_ID}.`,
  ],
  consumers: { documents: CLIENTS },
  groups: { documents: [...errorCases, ...schemaCases, ...warningCases, ...intakeCases] },
};
