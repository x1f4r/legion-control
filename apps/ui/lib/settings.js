// Persistent UI preferences: appearance, density, machine card options, technical mode, refreshing.

export const SETTINGS_KEY = 'legion.settings.v1';
const LEGACY_THEME_KEY = 'legion.theme';

export const ACCENT_SWATCHES = [
  { id: 'green', label: 'Green' },
  { id: 'blue', label: 'Blue' },
  { id: 'violet', label: 'Violet' },
  { id: 'amber', label: 'Amber' },
  { id: 'rose', label: 'Rose' },
  { id: 'neutral', label: 'Neutral' },
];

const VALID_THEMES = new Set(['system', 'light', 'dark']);
const VALID_ACCENTS = new Set(ACCENT_SWATCHES.map((s) => s.id));
const VALID_TEXT_SIZES = new Set(['small', 'medium', 'large']);
const VALID_DENSITIES = new Set(['roomy', 'compact']);
const VALID_REFRESH = new Set([0, 15, 30, 60]);

export const DEFAULT_SETTINGS = Object.freeze({
  theme: 'system',
  accent: 'green',
  textSize: 'medium',
  density: 'roomy',
  showMetrics: true,
  showOffline: true,
  technical: false,
  refreshSeconds: 15,
  checkOffline: true,
});

export function normalizeSettings(raw) {
  const src = raw && typeof raw === 'object' ? raw : {};
  const refreshNum = Number(src.refreshSeconds);
  return {
    theme: VALID_THEMES.has(src.theme) ? src.theme : DEFAULT_SETTINGS.theme,
    accent: VALID_ACCENTS.has(src.accent) ? src.accent : DEFAULT_SETTINGS.accent,
    textSize: VALID_TEXT_SIZES.has(src.textSize) ? src.textSize : DEFAULT_SETTINGS.textSize,
    density: VALID_DENSITIES.has(src.density) ? src.density : DEFAULT_SETTINGS.density,
    showMetrics:
      typeof src.showMetrics === 'boolean' ? src.showMetrics : DEFAULT_SETTINGS.showMetrics,
    showOffline:
      typeof src.showOffline === 'boolean' ? src.showOffline : DEFAULT_SETTINGS.showOffline,
    technical: typeof src.technical === 'boolean' ? src.technical : DEFAULT_SETTINGS.technical,
    refreshSeconds: VALID_REFRESH.has(refreshNum) ? refreshNum : DEFAULT_SETTINGS.refreshSeconds,
    checkOffline:
      typeof src.checkOffline === 'boolean' ? src.checkOffline : DEFAULT_SETTINGS.checkOffline,
  };
}

export function readSettings(storage) {
  if (!storage || typeof storage.getItem !== 'function') {
    return { ...DEFAULT_SETTINGS };
  }
  try {
    const raw = storage.getItem(SETTINGS_KEY);
    if (raw) {
      return normalizeSettings(JSON.parse(raw));
    }
    // Migrate legacy theme preference if no v1 settings exist yet.
    const legacyTheme = storage.getItem(LEGACY_THEME_KEY);
    if (VALID_THEMES.has(legacyTheme)) {
      return normalizeSettings({ theme: legacyTheme });
    }
  } catch {
    // Ignore corrupt or unavailable storage.
  }
  return { ...DEFAULT_SETTINGS };
}

export function writeSettings(storage, settings) {
  const normalized = normalizeSettings(settings);
  if (!storage || typeof storage.setItem !== 'function') return normalized;
  try {
    storage.setItem(SETTINGS_KEY, JSON.stringify(normalized));
  } catch {
    // Ignore quota or disabled storage errors.
  }
  return normalized;
}

/**
 * Applies appearance settings via data-* attributes on the document root (CSP-safe, no inline CSS).
 */
export function applySettingsToRoot(root, settings) {
  if (!root || !root.dataset) return;
  const s = normalizeSettings(settings);
  if (s.theme === 'light' || s.theme === 'dark') {
    root.dataset.theme = s.theme;
  } else {
    delete root.dataset.theme;
  }
  if (s.accent && s.accent !== 'green') {
    root.dataset.accent = s.accent;
  } else {
    delete root.dataset.accent;
  }
  if (s.textSize && s.textSize !== 'medium') {
    root.dataset.text = s.textSize;
  } else {
    delete root.dataset.text;
  }
  if (s.density && s.density !== 'roomy') {
    root.dataset.density = s.density;
  } else {
    delete root.dataset.density;
  }
}

export function detectDeviceNoun(linkKind, userAgent = '', width = 0) {
  if (linkKind === 'http') return 'browser';
  const ua = String(userAgent || '');
  if (/iPad|Tablet/i.test(ua)) return 'tablet';
  if (/Android/i.test(ua)) {
    return /Mobile/i.test(ua) || (width > 0 && width < 720) ? 'phone' : 'tablet';
  }
  if (/iPhone|iPod|Mobile/i.test(ua)) return 'phone';
  if (width > 0 && width < 600) return 'phone';
  if (width >= 600 && width < 1024 && /Touch/i.test(ua)) return 'tablet';
  return 'computer';
}

export function describeAppearance(settings, deviceNoun = 'device') {
  const s = normalizeSettings(settings);
  const themeLabel =
    s.theme === 'dark' ? 'Dark' : s.theme === 'light' ? 'Light' : `Like the ${deviceNoun}`;
  const accentLabel = s.accent;
  const sizeLabel = `${s.textSize} text`;
  return `${themeLabel} · ${accentLabel} · ${sizeLabel}`;
}

export function describeRefreshing(settings) {
  const s = normalizeSettings(settings);
  if (s.refreshSeconds === 0) return 'Only when you refresh';
  if (s.refreshSeconds === 60) return 'Every minute while open';
  return `Every ${s.refreshSeconds} seconds while open`;
}
