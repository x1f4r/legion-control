import assert from 'node:assert/strict';
import test from 'node:test';
import {
  ACCENT_SWATCHES,
  DEFAULT_SETTINGS,
  SETTINGS_KEY,
  applySettingsToRoot,
  buildDeviceInviteArgs,
  buildDeviceRemoveArgs,
  describeAppearance,
  describeRefreshing,
  detectDeviceNoun,
  findIncomingDevice,
  formatDeviceRemovedNotice,
  isInviteOwnerStrength,
  isPairingId,
  isRoleOwnerStrength,
  normalizeSettings,
  parsePolicyForInvite,
  readSettings,
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
