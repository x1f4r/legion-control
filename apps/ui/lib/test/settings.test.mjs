import assert from 'node:assert/strict';
import test from 'node:test';
import {
  ACCENT_SWATCHES,
  DEFAULT_SETTINGS,
  SETTINGS_KEY,
  applySettingsToRoot,
  describeAppearance,
  describeRefreshing,
  detectDeviceNoun,
  normalizeSettings,
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
