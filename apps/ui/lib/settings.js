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

/**
 * Builds `device.invite` arguments matching the node's live tool schema:
 * - When `device.invite` accepts both `name` and `role` and a role is given, sends `{ name, role }`
 *   (never `principal` + `role` together, which the node refuses).
 * - Otherwise sends `{ principal }` (plus `role` only if the schema accepts `role` without `name`).
 * - Includes `allow_owner: true` only when the schema accepts it and `allowOwner` is explicitly set.
 */
export function buildDeviceInviteArgs(inviteSpec, { label, role, validMinutes, allowOwner = false }) {
  const props = inviteSpec?.input_schema?.properties || {};
  const hasName = Boolean(props.name);
  const hasRole = Boolean(props.role);
  const hasAllowOwner = Boolean(props.allow_owner);
  const who = String(label || '').trim();
  const trimmedRole = String(role || '').trim();
  const args = { valid_minutes: validMinutes };

  if (hasName && hasRole && trimmedRole) {
    args.name = who;
    args.role = trimmedRole;
  } else {
    args.principal = who;
    if (hasRole && trimmedRole) args.role = trimmedRole;
  }
  if (hasAllowOwner && allowOwner) {
    args.allow_owner = true;
  }
  return args;
}

/**
 * Builds `device.remove` arguments, preferring the unique pairing `id` when present so two paired
 * devices sharing a display name can still be removed unambiguously.
 */
export function buildDeviceRemoveArgs(device) {
  if (device?.id) return { id: device.id };
  return { name: device?.name };
}

const PAIRING_ID_RE = /^[0-9a-f]{32}$/;

/**
 * Whether `value` has the exact shape of a Legion pairing id (32 lowercase hex digits).
 */
export function isPairingId(value) {
  return typeof value === 'string' && PAIRING_ID_RE.test(value);
}

/**
 * Resolves an `#/settings/connections/incoming/<item>` route segment against `device.list`.
 * When `routeItem` is a pairing id, matches strictly by `device.id` and never by a device name
 * that happens to equal that id.
 */
export function findIncomingDevice(devices, routeItem) {
  const list = Array.isArray(devices) ? devices : [];
  const target = String(routeItem || '');
  if (!target) return null;
  if (isPairingId(target)) {
    return list.find((d) => d?.id === target) || null;
  }
  return (
    list.find((d) => d?.id === target) ||
    list.find((d) => !d?.id && d?.name === target) ||
    list.find((d) => d?.name === target) ||
    null
  );
}

/**
 * Plain-language notice after `device.remove` succeeds, or `null` when nothing extra needs
 * attention. Never lists fleet machines as still trusting a `local_only` device.
 */
export function formatDeviceRemovedNotice(device, result) {
  if (device?.local_only) return null;
  const stillTrusted = Array.isArray(result?.still_trusted_by)
    ? result.still_trusted_by.filter(Boolean)
    : [];
  if (stillTrusted.length === 0) return null;
  const name = device?.name || result?.name || 'This device';
  const verb =
    stillTrusted.length === 1
      ? 'was unreachable and still trusts'
      : 'were unreachable and still trust';
  return `${name} was removed here, but ${stillTrusted.join(', ')} ${verb} it until the next sync.`;
}

function stripTomlComment(line) {
  let quote = null;
  let escaped = false;
  for (let i = 0; i < line.length; i += 1) {
    const ch = line[i];
    if (quote === '"') {
      if (escaped) {
        escaped = false;
      } else if (ch === '\\') {
        escaped = true;
      } else if (ch === '"') {
        quote = null;
      }
      continue;
    }
    if (quote === "'") {
      if (ch === "'") quote = null;
      continue;
    }
    if (ch === '"' || ch === "'") {
      quote = ch;
    } else if (ch === '#') {
      return line.slice(0, i);
    }
  }
  return line;
}

function unquoteTomlString(raw) {
  const s = String(raw || '').trim();
  if (s.length >= 2 && s.startsWith('"') && s.endsWith('"')) {
    try {
      return JSON.parse(s);
    } catch {
      return s.slice(1, -1);
    }
  }
  if (s.length >= 2 && s.startsWith("'") && s.endsWith("'")) {
    return s.slice(1, -1);
  }
  return s;
}

function parseTomlKeySegments(header) {
  const segments = [];
  let i = 0;
  const n = header.length;
  while (i < n) {
    while (i < n && (header[i] === ' ' || header[i] === '\t')) i += 1;
    if (i >= n) break;
    if (header[i] === '"' || header[i] === "'") {
      const q = header[i];
      const start = i;
      i += 1;
      let escaped = false;
      while (i < n) {
        if (q === '"' && escaped) {
          escaped = false;
        } else if (q === '"' && header[i] === '\\') {
          escaped = true;
        } else if (header[i] === q) {
          i += 1;
          break;
        }
        i += 1;
      }
      segments.push(unquoteTomlString(header.slice(start, i)));
    } else {
      const start = i;
      while (i < n && header[i] !== '.' && header[i] !== ' ' && header[i] !== '\t') i += 1;
      segments.push(header.slice(start, i));
    }
    while (i < n && (header[i] === ' ' || header[i] === '\t')) i += 1;
    if (header[i] === '.') i += 1;
  }
  return segments;
}

function parseTomlStringArray(raw) {
  const out = [];
  const s = String(raw || '');
  let i = 0;
  const n = s.length;
  while (i < n) {
    const ch = s[i];
    if (ch === '"' || ch === "'") {
      const start = i;
      i += 1;
      let escaped = false;
      while (i < n) {
        if (ch === '"' && escaped) {
          escaped = false;
        } else if (ch === '"' && s[i] === '\\') {
          escaped = true;
        } else if (s[i] === ch) {
          i += 1;
          break;
        }
        i += 1;
      }
      out.push(unquoteTomlString(s.slice(start, i)));
    } else {
      i += 1;
    }
  }
  return out;
}

function arrayBracketDelta(text) {
  let depth = 0;
  let quote = null;
  let escaped = false;
  for (let i = 0; i < text.length; i += 1) {
    const ch = text[i];
    if (quote === '"') {
      if (escaped) escaped = false;
      else if (ch === '\\') escaped = true;
      else if (ch === '"') quote = null;
      continue;
    }
    if (quote === "'") {
      if (ch === "'") quote = null;
      continue;
    }
    if (ch === '"' || ch === "'") quote = ch;
    else if (ch === '[') depth += 1;
    else if (ch === ']') depth -= 1;
  }
  return depth;
}

/**
 * Extracts `[principals.*]` role assignments and `[roles.*]` allow/deny rule lists from
 * `policy.toml` text so the invite screen can detect owner-strength identities and roles.
 */
export function parsePolicyForInvite(tomlText) {
  const principals = {};
  const roles = {};
  if (typeof tomlText !== 'string' || !tomlText.trim()) {
    return { principals, roles };
  }

  let section = null; // { kind: 'principal' | 'role' | 'principals' | 'roles', name?: string }
  let pendingArray = null; // { roleName: string, field: 'allow' | 'deny', buf: string, depth: number }

  for (const rawLine of tomlText.split(/\r?\n/)) {
    const line = stripTomlComment(rawLine).trim();
    if (!line) continue;

    if (pendingArray) {
      pendingArray.buf += ` ${line}`;
      pendingArray.depth += arrayBracketDelta(line);
      if (pendingArray.depth <= 0) {
        roles[pendingArray.roleName][pendingArray.field] = parseTomlStringArray(pendingArray.buf);
        pendingArray = null;
      }
      continue;
    }

    if (line.startsWith('[') && line.endsWith(']') && !line.startsWith('[[')) {
      const segs = parseTomlKeySegments(line.slice(1, -1));
      if (segs[0] === 'principals' && segs.length === 2 && segs[1]) {
        section = { kind: 'principal', name: segs[1] };
      } else if (segs[0] === 'roles' && segs.length === 2 && segs[1]) {
        section = { kind: 'role', name: segs[1] };
        if (!roles[segs[1]]) roles[segs[1]] = { allow: [], deny: [] };
      } else if (segs[0] === 'principals' && segs.length === 1) {
        section = { kind: 'principals' };
      } else if (segs[0] === 'roles' && segs.length === 1) {
        section = { kind: 'roles' };
      } else {
        section = null;
      }
      continue;
    }

    const eqIdx = line.indexOf('=');
    if (eqIdx <= 0 || !section) continue;
    const key = unquoteTomlString(line.slice(0, eqIdx));
    const val = line.slice(eqIdx + 1).trim();

    if (section.kind === 'principal' && key === 'role') {
      principals[section.name] = unquoteTomlString(val);
    } else if (section.kind === 'principals' && val.startsWith('{') && val.endsWith('}')) {
      const m = val.match(/\brole\s*=\s*("[^"]*"|'[^']*')/);
      if (m) principals[key] = unquoteTomlString(m[1]);
    } else if (section.kind === 'role' && (key === 'allow' || key === 'deny')) {
      const depth = arrayBracketDelta(val);
      if (depth <= 0) {
        roles[section.name][key] = parseTomlStringArray(val);
      } else {
        pendingArray = { roleName: section.name, field: key, buf: val, depth };
      }
    }
  }

  return { principals, roles };
}

function globMatches(pattern, text) {
  let p = 0;
  let t = 0;
  let star = -1;
  let match = 0;
  while (t < text.length) {
    if (p < pattern.length && (pattern[p] === '?' || pattern[p] === text[t])) {
      p += 1;
      t += 1;
    } else if (p < pattern.length && pattern[p] === '*') {
      star = p;
      match = t;
      p += 1;
    } else if (star !== -1) {
      p = star + 1;
      match += 1;
      t = match;
    } else {
      return false;
    }
  }
  while (p < pattern.length && pattern[p] === '*') p += 1;
  return p === pattern.length;
}

function isWhoamiOwnerStrength(whoami) {
  if (!whoami) return false;
  if (whoami.role === 'owner') return true;
  if (Array.isArray(whoami.tools) && whoami.tools.includes('config.write')) return true;
  return false;
}

function ruleMatchesConfigWrite(rule) {
  if (rule === 'effect:admin') return true;
  if (typeof rule !== 'string' || !rule || rule.startsWith('effect:')) return false;
  return globMatches(rule, 'config.write');
}

function ruleIsBroadAdmin(rule) {
  if (rule === 'effect:admin') return true;
  return typeof rule === 'string' && rule.length > 0 && /^\*+$/.test(rule);
}

/**
 * Whether `roleName` has owner-strength permissions on this node: the built-in `owner` role, or
 * a policy role allowing `effect:admin` or `config.write` (which can rewrite `policy.toml`).
 */
export function isRoleOwnerStrength(roleName, { policy = null, whoami = null } = {}) {
  const trimmed = String(roleName || '').trim();
  if (!trimmed || trimmed === 'nobody') return false;
  if (trimmed === 'owner') return true;
  const def = policy?.roles?.[trimmed];
  if (def) {
    const allow = Array.isArray(def.allow) ? def.allow : [];
    const deny = Array.isArray(def.deny) ? def.deny : [];
    const allowsBroadAdmin = allow.some(ruleIsBroadAdmin) && !deny.some(ruleIsBroadAdmin);
    const allowsConfigWrite =
      allow.some(ruleMatchesConfigWrite) && !deny.some(ruleMatchesConfigWrite);
    return allowsBroadAdmin || allowsConfigWrite;
  }
  if (whoami?.role && trimmed === whoami.role) {
    return isWhoamiOwnerStrength(whoami);
  }
  return false;
}

/**
 * Whether the current invite form targets an owner-strength role or identity on this node.
 */
export function isInviteOwnerStrength({
  label = '',
  role = '',
  usesRoleName = false,
  policy = null,
  whoami = null,
  incomingDevices = [],
} = {}) {
  if (usesRoleName) {
    return isRoleOwnerStrength(role, { policy, whoami });
  }
  const who = String(label || '').trim();
  if (!who) return false;
  if (who === 'owner') return true;

  const policyRole = policy?.principals?.[who] || null;
  if (policyRole) {
    return isRoleOwnerStrength(policyRole, { policy, whoami });
  }
  if (whoami?.principal && who === whoami.principal) {
    return isWhoamiOwnerStrength(whoami);
  }
  const matchedDevice = (incomingDevices || []).find((d) => d?.principal === who && d?.role);
  if (matchedDevice) {
    return isRoleOwnerStrength(matchedDevice.role, { policy, whoami });
  }
  return false;
}
