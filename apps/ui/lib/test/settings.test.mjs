import assert from 'node:assert/strict';
import test from 'node:test';
import {
  ACCENT_SWATCHES,
  DEFAULT_SETTINGS,
  SETTINGS_KEY,
  applySettingsToRoot,
  buildDeviceInviteArgs,
  buildDeviceRemoveArgs,
  buildRelayedConnections,
  describeAppearance,
  describeConnectionRoute,
  describePolicyRolePlain,
  describeRefreshing,
  detectDeviceNoun,
  extractInviteRoles,
  findIncomingDevice,
  formatDeviceRemovedNotice,
  formatInvitedDeviceSyncNotice,
  formatPairSyncNotice,
  isInviteOwnerStrength,
  isPairingId,
  isRoleOwnerStrength,
  normalizeNodeRoles,
  normalizeSettings,
  parsePolicyForInvite,
  readSettings,
  summarizeInvitedDeviceFleet,
  writeSettings,
} from '../settings.js';

function memoryStorage(initial = {}) {
  const map = new Map(Object.entries(initial));
  return {
    getItem: (k) => (map.has(k) ? map.get(k) : null),
    setItem: (k, v) => map.set(k, String(v)),
    removeItem: (k) => map.delete(k),
  };
}

test('normalizeSettings falls back to defaults for invalid values', () => {
  const out = normalizeSettings({
    theme: 'sepia',
    accent: 'neon',
    textSize: 'giant',
    density: 'packed',
    showMetrics: 'yes',
    refreshSeconds: 99,
  });
  assert.deepEqual(out, { ...DEFAULT_SETTINGS });
  assert.equal(ACCENT_SWATCHES.length, 6);
});

test('readSettings migrates legacy legion.theme and writeSettings persists', () => {
  const store = memoryStorage({ 'legion.theme': 'light' });
  const migrated = readSettings(store);
  assert.equal(migrated.theme, 'light');
  assert.equal(migrated.accent, 'green');

  const updated = writeSettings(store, {
    ...migrated,
    accent: 'blue',
    textSize: 'large',
    density: 'compact',
    technical: true,
    showOffline: false,
    refreshSeconds: 30,
    checkOffline: false,
  });
  assert.ok(store.getItem(SETTINGS_KEY));
  assert.deepEqual(readSettings(store), updated);
});

test('applySettingsToRoot toggles data attributes cleanly without inline styles', () => {
  const root = { dataset: {} };
  applySettingsToRoot(root, {
    theme: 'dark',
    accent: 'violet',
    textSize: 'large',
    density: 'compact',
  });
  assert.equal(root.dataset.theme, 'dark');
  assert.equal(root.dataset.accent, 'violet');
  assert.equal(root.dataset.text, 'large');
  assert.equal(root.dataset.density, 'compact');

  applySettingsToRoot(root, DEFAULT_SETTINGS);
  assert.deepEqual(root.dataset, {});
});

test('detectDeviceNoun and summary descriptions read cleanly', () => {
  assert.equal(detectDeviceNoun('http', 'Mozilla/5.0', 390), 'browser');
  assert.equal(detectDeviceNoun('app', 'Mozilla/5.0 (Linux; Android 15; Pixel 10 Mobile)', 390), 'phone');
  assert.equal(detectDeviceNoun('app', 'Mozilla/5.0 (Linux; Android 15; Pixel Tablet)', 1100), 'tablet');
  assert.equal(detectDeviceNoun('app', 'Mozilla/5.0 (Macintosh; Intel Mac OS X)', 1200), 'computer');

  assert.equal(
    describeAppearance({ theme: 'dark', accent: 'green', textSize: 'medium' }, 'phone'),
    'Dark · green · medium text',
  );
  assert.equal(
    describeAppearance({ theme: 'system', accent: 'blue', textSize: 'small' }, 'phone'),
    'Like the phone · blue · small text',
  );
  assert.equal(describeRefreshing({ refreshSeconds: 0 }), 'Only when you refresh');
  assert.equal(describeRefreshing({ refreshSeconds: 15 }), 'Every 15 seconds while open');
  assert.equal(describeRefreshing({ refreshSeconds: 60 }), 'Every minute while open');
});

test('buildDeviceInviteArgs and buildDeviceRemoveArgs match fleet device tool schemas', () => {
  const fleetInviteSpec = {
    name: 'device.invite',
    input_schema: {
      type: 'object',
      properties: {
        principal: { type: 'string' },
        role: { type: 'string' },
        name: { type: 'string' },
        allow_owner: { type: 'boolean' },
        valid_minutes: { type: 'integer' },
      },
    },
  };
  const legacyInviteSpec = {
    name: 'device.invite',
    input_schema: {
      type: 'object',
      required: ['principal'],
      properties: {
        principal: { type: 'string' },
        valid_minutes: { type: 'integer' },
      },
    },
  };

  // With role + name schema and a role chosen -> sends name + role (never principal + role).
  assert.deepEqual(
    buildDeviceInviteArgs(fleetInviteSpec, {
      label: 'Pixel 10',
      role: 'observer',
      validMinutes: 15,
    }),
    { name: 'Pixel 10', role: 'observer', valid_minutes: 15 },
  );

  // With owner role + explicit allowOwner -> sends allow_owner: true.
  assert.deepEqual(
    buildDeviceInviteArgs(fleetInviteSpec, {
      label: 'tablet',
      role: 'owner',
      validMinutes: 60,
      allowOwner: true,
    }),
    { name: 'tablet', role: 'owner', valid_minutes: 60, allow_owner: true },
  );

  // Blank role on fleet schema -> reuses existing principal.
  assert.deepEqual(
    buildDeviceInviteArgs(fleetInviteSpec, {
      label: 'phone',
      role: '',
      validMinutes: 15,
    }),
    { principal: 'phone', valid_minutes: 15 },
  );

  // Legacy schema without role/name -> sends principal only.
  assert.deepEqual(
    buildDeviceInviteArgs(legacyInviteSpec, {
      label: 'phone',
      role: 'observer',
      validMinutes: 15,
      allowOwner: true,
    }),
    { principal: 'phone', valid_minutes: 15 },
  );

  // device.remove prefers unique pairing id when available, falling back to name.
  assert.deepEqual(buildDeviceRemoveArgs({ id: '0123456789abcdef0123456789abcdef', name: 'pixel' }), {
    id: '0123456789abcdef0123456789abcdef',
  });
  assert.deepEqual(buildDeviceRemoveArgs({ name: 'Pixel 10' }), { name: 'Pixel 10' });
});

test('isInviteOwnerStrength detects owner-strength roles and identities beyond the literal name owner', () => {
  const policyToml = `
default = "owner"

[principals.admin]
role = "owner"

[principals."device:boss"]
role = "superadmin"

[principals.phone]
role = "operator"

[principals.helper]
role = "config_writer"

[principals.readonly_cfg]
role = "config_reader"

[roles.superadmin]
allow = [
  "effect:read",
  "effect:admin", # as strong as owner
]

[roles.config_writer]
allow = ["config.write", "effect:read"]

[roles.config_reader]
allow = ["config.*"]
deny = ["config.write"]

[roles.operator]
allow = ["effect:read", "effect:write", "effect:exec", "effect:power"]
`;
  const policy = parsePolicyForInvite(policyToml);

  // Explicit role selection (usesRoleName = true)
  assert.equal(isRoleOwnerStrength('owner', { policy }), true);
  assert.equal(isRoleOwnerStrength('superadmin', { policy }), true);
  assert.equal(isRoleOwnerStrength('config_writer', { policy }), true);
  assert.equal(isRoleOwnerStrength('config_reader', { policy }), false);
  assert.equal(isRoleOwnerStrength('operator', { policy }), false);
  assert.equal(isRoleOwnerStrength('nobody', { policy }), false);

  assert.equal(
    isInviteOwnerStrength({ label: 'tablet', role: 'superadmin', usesRoleName: true, policy }),
    true,
  );
  assert.equal(
    isInviteOwnerStrength({ label: 'tablet', role: 'operator', usesRoleName: true, policy }),
    false,
  );

  // Existing identity selection (usesRoleName = false): checks the identity's role on the node,
  // not only the literal principal name "owner".
  assert.equal(
    isInviteOwnerStrength({ label: 'owner', role: '', usesRoleName: false, policy }),
    true,
  );
  assert.equal(
    isInviteOwnerStrength({ label: 'admin', role: '', usesRoleName: false, policy }),
    true,
  );
  assert.equal(
    isInviteOwnerStrength({ label: 'device:boss', role: '', usesRoleName: false, policy }),
    true,
  );
  assert.equal(
    isInviteOwnerStrength({ label: 'helper', role: '', usesRoleName: false, policy }),
    true,
  );
  assert.equal(
    isInviteOwnerStrength({ label: 'readonly_cfg', role: '', usesRoleName: false, policy }),
    false,
  );
  assert.equal(
    isInviteOwnerStrength({ label: 'phone', role: '', usesRoleName: false, policy }),
    false,
  );

  // Fallbacks when policy.toml could not be read: whoami and incomingDevices.
  assert.equal(
    isInviteOwnerStrength({
      label: 'boss-token',
      role: '',
      usesRoleName: false,
      policy: null,
      whoami: { principal: 'boss-token', role: 'custom_admin', tools: ['meta.whoami', 'config.write'] },
    }),
    true,
  );
  assert.equal(
    isInviteOwnerStrength({
      label: 'tablet',
      role: '',
      usesRoleName: false,
      policy: null,
      incomingDevices: [{ principal: 'tablet', role: 'owner' }],
    }),
    true,
  );
  assert.equal(
    isInviteOwnerStrength({
      label: 'phone',
      role: '',
      usesRoleName: false,
      policy: null,
      whoami: { principal: 'phone', role: 'operator', tools: ['meta.whoami', 'device.invite'] },
    }),
    false,
  );
});

test('findIncomingDevice matches pairing-id routes strictly by id and never by a name equal to an id', () => {
  const realId = '0123456789abcdef0123456789abcdef';
  const otherId = 'fedcba9876543210fedcba9876543210';

  assert.equal(isPairingId(realId), true);
  assert.equal(isPairingId('0123456789ABCDEF0123456789ABCDEF'), false);
  assert.equal(isPairingId('Pixel 10'), false);

  const spoofedByNameWithOtherId = { id: otherId, name: realId, principal: 'phone' };
  const spoofedByNameWithoutId = { name: realId, principal: 'phone' };
  const realDevice = { id: realId, name: 'Pixel 10', principal: 'device:pixel' };
  const legacyDevice = { name: 'Legacy Phone', principal: 'phone' };

  // When route item is a pairing id, never match a device whose name happens to equal that id.
  assert.equal(findIncomingDevice([spoofedByNameWithOtherId, spoofedByNameWithoutId], realId), null);
  assert.equal(
    findIncomingDevice([spoofedByNameWithOtherId, realDevice], realId),
    realDevice,
  );

  // Non-id route items still resolve legacy devices by name.
  assert.equal(findIncomingDevice([realDevice, legacyDevice], 'Legacy Phone'), legacyDevice);
});

test('formatDeviceRemovedNotice never lists fleet machines for a local-only device', () => {
  assert.equal(
    formatDeviceRemovedNotice(
      { name: 'local-phone', local_only: true },
      { removed: true, still_trusted_by: ['pi', 'studio'] },
    ),
    null,
  );
  assert.equal(
    formatDeviceRemovedNotice(
      { name: 'Pixel 10', local_only: false },
      { removed: true, still_trusted_by: [] },
    ),
    null,
  );
  const notice = formatDeviceRemovedNotice(
    { name: 'Pixel 10', local_only: false },
    { removed: true, still_trusted_by: ['pi'] },
  );
  assert.ok(notice && notice.includes('pi') && notice.includes('Pixel 10'));
});

test('formatPairSyncNotice describes synced and pending fleet machines without leaking tool names in plain mode', () => {
  assert.equal(formatPairSyncNotice({ node: 'tower', synced: [], not_synced: [] }, false), null);

  const allSynced = formatPairSyncNotice(
    { node: 'tower', synced: ['pi', 'studio'], not_synced: [] },
    false,
  );
  assert.equal(allSynced.kind, 'neutral');
  assert.match(allSynced.title, /Paired with tower and shared with pi and studio/);
  assert.ok(!allSynced.detail.includes('fleet.sync'));

  const partialPlain = formatPairSyncNotice(
    {
      node: 'tower',
      synced: ['pi'],
      not_synced: [{ node: 'studio', reason: 'connection refused' }],
      sync_command: 'legion fleet.sync direction=push documents=\'["devices.toml"]\'',
    },
    false,
  );
  assert.equal(partialPlain.kind, 'warn');
  assert.match(partialPlain.title, /shared with pi/);
  assert.match(partialPlain.detail, /studio did not answer and will get this device at the next sync/);
  assert.ok(!partialPlain.detail.includes('fleet.sync'));
  assert.ok(!partialPlain.detail.includes('devices.toml'));

  const partialTech = formatPairSyncNotice(
    {
      node: 'tower',
      synced: ['pi'],
      not_synced: [{ node: 'studio', status: 'not_reached' }],
      sync_command: 'legion fleet.sync direction=push documents=\'["devices.toml"]\'',
    },
    true,
  );
  assert.match(partialTech.detail, /legion fleet\.sync/);

  // Partial peer (removals delivered, live device changes refused) is distinguished from not_reached.
  const partialStatusPlain = formatPairSyncNotice(
    {
      node: 'tower',
      synced: ['pi'],
      not_synced: [
        { node: 'studio', status: 'partial', reason: 'live device changes refused' },
        { node: 'nas', status: 'not_reached' },
      ],
    },
    false,
  );
  assert.equal(partialStatusPlain.kind, 'warn');
  assert.match(partialStatusPlain.detail, /studio received removals, but not this device yet/);
  assert.match(partialStatusPlain.detail, /nas did not answer/);
  assert.ok(!partialStatusPlain.detail.includes('fleet.sync'));
});

test('describeConnectionRoute and buildRelayedConnections render direct, relay, unsynced, and identity-changed routes cleanly', () => {
  assert.equal(describeConnectionRoute({ node: 'tower', route: 'direct' }, false).label, 'directly');
  assert.equal(
    describeConnectionRoute({ node: 'nas', route: 'relay', route_via: 'tower' }, false).label,
    'through tower',
  );

  const unsyncedPlain = describeConnectionRoute(
    { node: 'studio', route: 'not_synced', learned_from: 'tower', route_via: 'tower' },
    false,
  );
  assert.equal(unsyncedPlain.label, 'not yet: tower has not passed this device on to it');
  assert.equal(unsyncedPlain.hint, 'Ask the owner to sync the machines');
  assert.ok(!unsyncedPlain.hint.includes('fleet.sync'));

  const unsyncedTech = describeConnectionRoute(
    { node: 'studio', route: 'not_synced', learned_from: 'tower', route_via: 'tower' },
    true,
  );
  assert.match(unsyncedTech.hint, /legion fleet\.sync/);

  const changed = describeConnectionRoute(
    {
      node: 'mini',
      origin: 'learned',
      route: 'identity_changed',
      identity_changed: { reported_by: 'tower', old_fingerprints: ['a'], new_fingerprints: ['b'] },
    },
    false,
  );
  assert.equal(changed.isIdentityChanged, true);
  assert.equal(changed.label, 'Identity changed');
  assert.match(changed.hint, /tower reported a change for mini/);

  const changedPaired = describeConnectionRoute(
    {
      node: 'tower',
      origin: 'paired',
      route: 'identity_changed',
      identity_changed: {
        reported_by: 'pi',
        is_paired: true,
        old_fingerprints: ['a'],
        new_fingerprints: ['b'],
      },
    },
    false,
  );
  assert.match(changedPaired.hint, /pair with tower again/);

  const relayed = buildRelayedConnections(
    {
      nodes: [{ node: 'tower' }, { node: 'pi' }],
      routes: {
        tower: { kind: 'direct' },
        pi: { kind: 'direct' },
        nas: { kind: 'relay', via: 'tower' },
      },
    },
    { machines: [{ name: 'tower' }, { name: 'pi' }, { name: 'nas' }, { name: 'laptop' }] },
  );
  assert.deepEqual(
    relayed.map((r) => [r.node, r.route, r.route_via]),
    [
      ['laptop', 'relay', 'tower'],
      ['nas', 'relay', 'tower'],
    ],
  );
});

test('extractInviteRoles, normalizeNodeRoles, and live policy analysis never hardcode descriptions by role name', () => {
  // Primary path: device.roles preserves node-supplied descriptions verbatim.
  const fromNode = normalizeNodeRoles([
    {
      name: 'observer',
      description: 'node custom description for observer',
      strongest_effect: 'read',
      owner_strength: false,
    },
    {
      name: 'owner',
      description: 'everything, including changing who may do what',
      strongest_effect: 'admin',
      owner_strength: true,
    },
  ]);
  assert.equal(fromNode.length, 2);
  assert.equal(fromNode[0].description, 'node custom description for observer');
  assert.equal(fromNode[0].ownerStrength, false);
  assert.equal(fromNode[1].ownerStrength, true);

  // Fallback path: descriptions are derived strictly from live policy rules, NOT role names.
  // Even a role named "observer" that grants effect:admin is described as owner-strength!
  const swappedPolicy = parsePolicyForInvite(`
[roles.observer]
allow = ["effect:admin"]

[roles.operator]
allow = ["effect:read"]

[roles.helper]
allow = ["effect:read", "effect:exec"]
`);
  const swappedDescObserver = describePolicyRolePlain('observer', swappedPolicy);
  assert.match(swappedDescObserver, /as strong as owner/);
  assert.equal(
    describePolicyRolePlain('operator', swappedPolicy),
    'look only (read status and files, change nothing)',
  );

  const policy = parsePolicyForInvite(`
[roles.observer]
allow = ["effect:read"]

[roles.editor]
allow = ["effect:read", "effect:write"]

[roles.operator]
allow = ["effect:read", "effect:write", "effect:exec", "effect:power"]

[roles.helper]
allow = ["effect:read", "effect:exec"]
`);
  const roles = extractInviteRoles(policy, { type: 'string' }, { allowOwner: true });
  assert.deepEqual(
    roles.map((r) => r.name),
    ['observer', 'editor', 'operator', 'helper', 'owner'],
  );
  assert.equal(
    describePolicyRolePlain('observer', policy),
    'look only (read status and files, change nothing)',
  );
  assert.match(describePolicyRolePlain('helper', policy), /allows running commands/);
  assert.equal(roles.find((r) => r.name === 'owner').ownerStrength, true);
  assert.equal(roles.find((r) => r.name === 'observer').ownerStrength, false);

  const summary = summarizeInvitedDeviceFleet(
    {
      results: {
        tower: { ok: { devices: [{ id: 'abc', name: 'tablet' }] } },
        pi: { ok: { devices: [{ id: 'abc', name: 'tablet' }] } },
        studio: { ok: { devices: [] } },
      },
    },
    { id: 'abc', name: 'tablet' },
  );
  assert.deepEqual(summary, { received: ['pi', 'tower'], pending: ['studio'] });

  const plainNotice = formatInvitedDeviceSyncNotice('tablet', summary, false);
  assert.equal(plainNotice.title, 'tablet is paired.');
  assert.match(plainNotice.detail, /Received by pi and tower/);
  assert.match(plainNotice.detail, /studio will get it at the next sync/);
  assert.ok(!plainNotice.detail.includes('fleet.sync'));
});
