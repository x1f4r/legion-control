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
  if (s.technical) {
    root.dataset.technical = 'true';
  } else {
    delete root.dataset.technical;
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

function joinNamesPlain(names) {
  const list = (names || []).filter(Boolean);
  if (list.length === 0) return '';
  if (list.length === 1) return list[0];
  if (list.length === 2) return `${list[0]} and ${list[1]}`;
  return `${list.slice(0, -1).join(', ')} and ${list[list.length - 1]}`;
}

/**
 * Plain-language summary of which fleet machines received this device right after pairing
 * and which will receive it at their next sync (distinguishing `partial` from `not_reached`).
 */
export function formatPairSyncNotice(lastPaired, technical = false) {
  if (!lastPaired || typeof lastPaired !== 'object') return null;
  const node = String(lastPaired.node || '').trim();
  const synced = Array.isArray(lastPaired.synced) ? lastPaired.synced.filter(Boolean) : [];
  const rawPending = Array.isArray(lastPaired.not_synced) ? lastPaired.not_synced : [];
  const partialPeers = [];
  const unreachedPeers = [];
  for (const item of rawPending) {
    const peerNode = typeof item === 'string' ? item : item?.node;
    if (!peerNode) continue;
    const status = typeof item === 'object' ? String(item?.status || '') : '';
    const reason = typeof item === 'object' && item?.reason ? String(item.reason) : '';
    if (status === 'partial') {
      partialPeers.push({ node: peerNode, reason });
    } else {
      unreachedPeers.push({ node: peerNode, reason });
    }
  }
  if (synced.length === 0 && partialPeers.length === 0 && unreachedPeers.length === 0) return null;

  const syncedNames = joinNamesPlain(synced);
  const title =
    synced.length > 0
      ? `Paired with ${node || 'this machine'} and shared with ${syncedNames}.`
      : `Paired with ${node || 'this machine'}.`;

  if (partialPeers.length === 0 && unreachedPeers.length === 0) {
    return {
      kind: 'neutral',
      title,
      detail: 'This device can reach them directly.',
    };
  }

  const parts = [];
  if (partialPeers.length > 0) {
    const names = joinNamesPlain(partialPeers.map((p) => p.node));
    let part = `${names} received removals, but not this device yet — it will get this device at the next sync.`;
    if (technical) {
      const reasons = partialPeers
        .filter((p) => p.reason)
        .map((p) => `${p.node}: ${p.reason}`);
      if (reasons.length > 0) {
        part = `${part} (${reasons.join('; ')})`;
      }
    }
    parts.push(part);
  }
  if (unreachedPeers.length > 0) {
    const names = joinNamesPlain(unreachedPeers.map((p) => p.node));
    parts.push(`${names} did not answer and will get this device at the next sync.`);
  }

  let detail = parts.join(' ');
  if (technical) {
    const cmd =
      String(lastPaired.sync_command || '').trim() ||
      'legion fleet.sync direction=push documents=\'["devices.toml"]\'';
    detail = `${detail} Run \`${cmd}\` to sync now.`;
  }
  return {
    kind: 'warn',
    title,
    detail,
  };
}

/**
 * Describes how this device reaches a machine for Settings -> Connections.
 * Never exposes tool names when `technical` is false.
 */
export function describeConnectionRoute(entry, technical = false) {
  const node = entry?.node || 'this machine';
  const idChanged = entry?.identity_changed || null;
  const route = idChanged ? 'identity_changed' : entry?.route || 'direct';

  if (route === 'identity_changed') {
    const reporter = idChanged?.reported_by || entry?.route_via || entry?.learned_from || 'another machine';
    const isPaired = Boolean(idChanged?.is_paired || entry?.origin === 'paired');
    const hasConflicts = Array.isArray(idChanged?.conflicts) && idChanged.conflicts.length > 0;
    const hint = isPaired
      ? `${reporter} reported a different host key for ${node}. Because this device paired with ${node} directly, its saved keys were not changed; pair with ${node} again if its identity really changed.`
      : hasConflicts
        ? `Machines disagree on the new identity for ${node}. Review the reports before deciding whether to dismiss or pair directly.`
        : `${reporter} reported a change for ${node}. Review the details and accept only if ${node} was reinstalled or its keys or address changed.`;
    return {
      kind: 'identity_changed',
      label: 'Identity changed',
      hint,
      isWarning: true,
      isIdentityChanged: true,
    };
  }

  if (route === 'not_synced') {
    const introducer = entry?.learned_from || entry?.route_via || 'another machine';
    return {
      kind: 'not_synced',
      label: `not yet: ${introducer} has not passed this device on to it`,
      hint: technical
        ? `Run \`legion fleet.sync\` on ${introducer}.`
        : 'Ask the owner to sync the machines',
      isWarning: false,
      isIdentityChanged: false,
    };
  }

  if (route === 'relay') {
    const via = entry?.route_via || 'another machine';
    return {
      kind: 'relay',
      label: `through ${via}`,
      hint: null,
      isWarning: false,
      isIdentityChanged: false,
    };
  }

  return {
    kind: 'direct',
    label: 'directly',
    hint: null,
    isWarning: false,
    isIdentityChanged: false,
  };
}

/**
 * Lists relay-only machines known via `linkState.routes` or the live fleet snapshot that do not
 * have their own direct node entry in `linkState.nodes`.
 */
export function buildRelayedConnections(linkState, snapshot) {
  const directNodes = new Set((linkState?.nodes || []).map((n) => n?.node).filter(Boolean));
  const fallbackVia = linkState?.nodes?.[0]?.node || null;
  const routes = linkState?.routes && typeof linkState.routes === 'object' ? linkState.routes : {};
  const byName = new Map();

  for (const [name, info] of Object.entries(routes)) {
    if (!name || directNodes.has(name)) continue;
    byName.set(name, {
      node: name,
      route: info?.kind || 'relay',
      route_via: info?.via || fallbackVia,
      learned_from: info?.introducer || null,
      identity_changed: null,
      relayedOnly: true,
    });
  }

  for (const m of snapshot?.machines || []) {
    if (!m?.name || directNodes.has(m.name) || byName.has(m.name)) continue;
    byName.set(m.name, {
      node: m.name,
      route: 'relay',
      route_via: fallbackVia,
      learned_from: null,
      identity_changed: null,
      relayedOnly: true,
    });
  }

  return Array.from(byName.values()).sort((a, b) => (a.node < b.node ? -1 : a.node > b.node ? 1 : 0));
}

function ruleMatchesToolSpec(rule, spec) {
  if (typeof rule !== 'string' || !rule) return false;
  if (rule.startsWith('effect:')) {
    return spec?.effect === rule.slice('effect:'.length);
  }
  return globMatches(rule, String(spec?.name || ''));
}

function analyzePolicyRole(roleName, policy, tools = []) {
  const trimmed = String(roleName || '').trim();
  if (trimmed === 'owner') {
    return {
      read: true,
      write: true,
      exec: true,
      power: true,
      destroy: true,
      admin: true,
      broadWrite: true,
      broadExec: true,
      broadAdmin: true,
      canWritePolicy: true,
      hasUnknownToolRule: false,
      ownerStrength: true,
      allow: ['*'],
    };
  }
  const def = policy?.roles?.[trimmed];
  if (!def || !Array.isArray(def.allow)) return null;
  const allow = def.allow;
  const deny = Array.isArray(def.deny) ? def.deny : [];
  const toolList = Array.isArray(tools) ? tools : [];

  const eff = {
    read: false,
    write: false,
    exec: false,
    power: false,
    destroy: false,
    admin: false,
    broadWrite: false,
    broadExec: false,
    broadAdmin: false,
    canWritePolicy: false,
    hasUnknownToolRule: false,
    ownerStrength: false,
    allow,
  };

  if (allow.length === 0) return eff;

  const isDeniedEffect = (effectName) =>
    deny.some((r) => r === `effect:${effectName}` || ruleIsBroadAdmin(r));

  for (const rule of allow) {
    if (ruleIsBroadAdmin(rule)) {
      if (!isDeniedEffect('read')) eff.read = true;
      if (!isDeniedEffect('write')) {
        eff.write = true;
        eff.broadWrite = true;
      }
      if (!isDeniedEffect('exec')) {
        eff.exec = true;
        eff.broadExec = true;
      }
      if (!isDeniedEffect('power')) eff.power = true;
      if (!isDeniedEffect('destroy')) eff.destroy = true;
      if (!isDeniedEffect('admin')) {
        eff.admin = true;
        eff.broadAdmin = true;
      }
    } else if (rule === 'effect:read' && !isDeniedEffect('read')) {
      eff.read = true;
    } else if (rule === 'effect:write' && !isDeniedEffect('write')) {
      eff.write = true;
      eff.broadWrite = true;
    } else if (rule === 'effect:exec' && !isDeniedEffect('exec')) {
      eff.exec = true;
      eff.broadExec = true;
    } else if (rule === 'effect:power' && !isDeniedEffect('power')) {
      eff.power = true;
    } else if (rule === 'effect:destroy' && !isDeniedEffect('destroy')) {
      eff.destroy = true;
    } else if (rule === 'effect:admin' && !isDeniedEffect('admin')) {
      eff.admin = true;
      eff.broadAdmin = true;
    }
  }

  if (allow.some(ruleMatchesConfigWrite) && !deny.some(ruleMatchesConfigWrite)) {
    eff.canWritePolicy = true;
    eff.admin = true;
  }

  if (toolList.length > 0) {
    for (const spec of toolList) {
      const allowed = allow.some((r) => ruleMatchesToolSpec(r, spec));
      const denied = deny.some((r) => ruleMatchesToolSpec(r, spec));
      if (allowed && !denied) {
        if (spec.effect === 'read') eff.read = true;
        else if (spec.effect === 'write') eff.write = true;
        else if (spec.effect === 'exec') eff.exec = true;
        else if (spec.effect === 'power') eff.power = true;
        else if (spec.effect === 'destroy') eff.destroy = true;
        else if (spec.effect === 'admin') eff.admin = true;
        if (spec.name === 'config.write') eff.canWritePolicy = true;
      }
    }
    for (const rule of allow) {
      if (rule.startsWith('effect:') || ruleIsBroadAdmin(rule)) continue;
      if (!toolList.some((spec) => globMatches(rule, String(spec?.name || '')))) {
        eff.hasUnknownToolRule = true;
      }
    }
  } else {
    for (const rule of allow) {
      if (rule.startsWith('effect:') || ruleIsBroadAdmin(rule)) continue;
      if (ruleMatchesConfigWrite(rule)) continue;
      eff.hasUnknownToolRule = true;
    }
  }

  eff.ownerStrength = eff.broadAdmin || eff.canWritePolicy;
  return eff;
}

/**
 * Normalizes the reply from the node's `device.roles` tool (`[{name, description, strongest_effect, owner_strength}]`
 * or `{ roles: [...] }`). Preserves the node-supplied `description` and `owner_strength` verbatim.
 */
export function normalizeNodeRoles(result) {
  const list = Array.isArray(result)
    ? result
    : Array.isArray(result?.roles)
      ? result.roles
      : null;
  if (!list) return null;
  const out = [];
  const seen = new Set();
  for (const item of list) {
    const name = String(item?.name || '').trim();
    if (!name || name === 'nobody' || seen.has(name)) continue;
    seen.add(name);
    out.push({
      name,
      description: String(item?.description || '').trim(),
      strongestEffect: item?.strongest_effect || null,
      ownerStrength: Boolean(item?.owner_strength),
      fromNode: true,
    });
  }
  return out;
}

/**
 * Derives a plain-English role description strictly from the node's live `policy.toml` rules and
 * tool catalogue (matching `legion pair`'s rule analysis; never hardcoded by role name except
 * built-in `owner`).
 *
 * When `technical` is false, custom rule summaries omit raw tool names so plain mode stays free
 * of internal tool identifiers.
 */
export function describePolicyRolePlain(
  roleName,
  policy = null,
  { tools = [], technical = false } = {},
) {
  const trimmed = String(roleName || '').trim();
  if (!trimmed) return '';
  if (trimmed === 'owner') {
    return 'everything, including changing who may do what';
  }
  const eff = analyzePolicyRole(trimmed, policy, tools);
  if (!eff) return '';
  if (eff.allow.length === 0) return 'nothing (no actions allowed)';

  const summary = eff.allow.join(', ');
  const suffix = technical && summary ? `: ${summary}` : '';

  if (eff.admin) {
    if (eff.broadAdmin) {
      return 'everything, including changing who may do what (as strong as owner)';
    }
    if (eff.canWritePolicy) {
      return `everything, including changing who may do what (as strong as owner${suffix})`;
    }
    return `custom (allows admin${suffix})`;
  }
  if (eff.exec) {
    if (eff.broadExec && eff.write && !eff.hasUnknownToolRule) {
      return 'operate (read, change files, and run commands — as strong as this account)';
    }
    return `custom (allows running commands — as strong as this account${suffix})`;
  }
  if (eff.power) {
    return `custom (allows power controls${suffix})`;
  }
  if (eff.destroy) {
    return `custom (allows removing files or processes${suffix})`;
  }
  if (eff.hasUnknownToolRule) {
    return `custom${suffix ? ` (${summary})` : ''}`;
  }
  if (eff.write) {
    if (eff.broadWrite) {
      return 'change files (read and edit files, no running commands)';
    }
    return `custom (allows changing files${suffix})`;
  }
  if (eff.read) {
    return 'look only (read status and files, change nothing)';
  }
  return 'nothing (no actions allowed)';
}

const STANDARD_ROLE_ORDER = ['observer', 'editor', 'operator'];

/**
 * Builds the list of selectable roles for "Let another device in" from `policy.toml` + `owner`
 * (fallback path when `device.roles` is not offered by the node).
 */
export function extractInviteRoles(
  policy,
  roleProp,
  { allowOwner = true, tools = [], technical = false } = {},
) {
  const names = [];
  const seen = new Set();
  const push = (name) => {
    const s = String(name || '').trim();
    if (!s || s === 'nobody' || seen.has(s)) return;
    seen.add(s);
    names.push(s);
  };

  if (Array.isArray(roleProp?.enum) && roleProp.enum.length > 0) {
    for (const opt of roleProp.enum) push(opt);
  } else if (policy?.roles && typeof policy.roles === 'object') {
    const policyNames = Object.keys(policy.roles);
    for (const std of STANDARD_ROLE_ORDER) {
      if (policyNames.includes(std)) push(std);
    }
    for (const custom of [...policyNames].sort()) {
      if (custom !== 'owner') push(custom);
    }
    if (allowOwner || policyNames.includes('owner')) {
      push('owner');
    }
  }

  return names.map((name) => {
    const eff = analyzePolicyRole(name, policy, tools);
    return {
      name,
      description: describePolicyRolePlain(name, policy, { tools, technical }),
      ownerStrength: eff ? eff.ownerStrength : isRoleOwnerStrength(name, { policy }),
      fromNode: false,
    };
  });
}

/**
 * Checks a fan-out `device.list` (`on: "all"`) result to see which fleet machines received
 * `invitedDevice` and which have not yet.
 */
export function summarizeInvitedDeviceFleet(allResult, invitedDevice) {
  const results = allResult?.results && typeof allResult.results === 'object' ? allResult.results : {};
  const received = [];
  const pending = [];
  const targetId = invitedDevice?.id || null;
  const targetFp = invitedDevice?.fingerprint || null;
  const targetName = invitedDevice?.name || null;
  const targetPrincipal = invitedDevice?.principal || null;

  for (const machine of Object.keys(results).sort()) {
    const entry = results[machine];
    const devices = Array.isArray(entry?.ok?.devices) ? entry.ok.devices : null;
    if (!devices) {
      pending.push(machine);
      continue;
    }
    const found = devices.some((d) => {
      if (targetId && d?.id) return d.id === targetId;
      if (targetFp && d?.fingerprint) return d.fingerprint === targetFp;
      return Boolean(targetName && d?.name === targetName && (!targetPrincipal || d?.principal === targetPrincipal));
    });
    if (found) received.push(machine);
    else pending.push(machine);
  }

  return { received, pending };
}

/**
 * Plain-language notice for the invite screen once the other device has paired.
 */
export function formatInvitedDeviceSyncNotice(deviceName, summary, technical = false) {
  const who = String(deviceName || '').trim() || 'The new device';
  const received = Array.isArray(summary?.received) ? summary.received.filter(Boolean) : [];
  const pending = Array.isArray(summary?.pending) ? summary.pending.filter(Boolean) : [];

  const title = `${who} is paired.`;
  if (received.length > 0 && pending.length === 0) {
    return {
      kind: 'neutral',
      title,
      detail: `Received by ${joinNamesPlain(received)}.`,
    };
  }
  if (received.length > 0 && pending.length > 0) {
    let detail = `Received by ${joinNamesPlain(received)}. ${joinNamesPlain(pending)} will get it at the next sync.`;
    if (technical) {
      detail = `${detail} Run \`legion fleet.sync direction=push documents='["devices.toml"]'\` to sync now.`;
    }
    return {
      kind: 'warn',
      title,
      detail,
    };
  }
  if (pending.length > 0) {
    return {
      kind: 'warn',
      title,
      detail: `${joinNamesPlain(pending)} will get it at the next sync.`,
    };
  }
  return {
    kind: 'neutral',
    title,
    detail: 'Ready to connect.',
  };
}
