// Environment resolution and the config schema for the Legion Control agent:
// which OS we are on, where our files live, what this system is called, which
// services it looks after, and the shared process runner every other module uses.
//
// Nothing in here may throw at import time. index.mjs needs to be able to load
// the whole agent on an unsupported platform and print a clean JSON error.
//
// The config file is optional and every key in it is optional. A value that does
// not make sense falls back to its default rather than propagating: a garbled
// port must not turn into a health check against port NaN, and an unreadable
// file must not turn into "no services and therefore nothing to look after".

import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

export const AGENT_VERSION = '2.1.0';

// Anything semver-shaped. Used when a service does not pin a stricter pattern:
// the point of the check is to refuse garbage that would otherwise be handed
// straight to an installer, not to have an opinion about version schemes.
export const DEFAULT_VERSION_PATTERN = '^\\d+\\.\\d+\\.\\d+(?:[-+].*)?$';

// The first version of the agent looked after exactly one service, T3 Code, and
// described it with these keys at the top level of config.json. They still work:
// see synthesizeLegacyServices below. New configs use "services" instead.
export const LEGACY_DEFAULTS = {
  port: 3773,
  channel: 'nightly',
  staleTurnHours: 6,
  // Packages in the T3 dependency tree that are allowed to run install scripts.
  // npm 12 blocks them by default and node-pty has to compile pty.node from
  // source on Linux, so without this the server cannot start after an update.
  // Only consulted on npm 12 and newer; see allowScriptsArgs in providers/npm.mjs.
  allowScripts: ['node-pty', 'msgpackr-extract'],
  t3AppPath: '/Applications/T3 Code (Nightly).app',
  t3AppBundleId: 'com.t3tools.t3code',
  updaterCacheDir: '~/Library/Caches/t3code-updater',
  stagedFilePattern: '^T3-Code-(.+)-(?:arm64|x64|universal)\\.(?:zip|dmg)$',
  nightlyPattern: '^\\d+\\.\\d+\\.\\d+-nightly\\.\\d+\\.\\d+$',
};

const IS_WINDOWS = process.platform === 'win32';

/** "linux" | "windows" | "mac" | null (null means this host is outside the contract). */
export function detectPlatform() {
  if (process.platform === 'linux') return 'linux';
  if (IS_WINDOWS) return 'windows';
  if (process.platform === 'darwin') return 'mac';
  return null;
}

/** What we report in JSON: the supported name, or the raw platform for diagnostics. */
export function platformName() {
  return detectPlatform() ?? process.platform;
}

export function isSupportedPlatform() {
  return detectPlatform() !== null;
}

const PLATFORM_SYSTEM_NAMES = { linux: 'Linux', windows: 'Windows', mac: 'macOS' };

/**
 * A Windows scheduled task can be registered to run as SYSTEM, and for SYSTEM
 * os.homedir() and %APPDATA%/%LOCALAPPDATA% all point at
 * C:\Windows\system32\config\systemprofile. Everything the agent cares about —
 * config.json, state.json, the service's own state database, the npm prefix, the
 * maintenance lock — belongs to the account that owns the install, so resolving
 * any of them from SYSTEM's profile silently reads empty defaults: no config
 * (auto update always on), no state database (never busy), the wrong npm prefix,
 * the wrong lock file.
 */
function runningInSystemProfile() {
  return IS_WINDOWS && /[\\/]config[\\/]systemprofile[\\/]?$/i.test(os.homedir());
}

/**
 * The home directory of the account that owns the install. Normally os.homedir();
 * under a SYSTEM task it is derived from where this file actually sits, since
 * the agent is installed at <home>/.legion-control/agent/src/.
 */
export function userHome() {
  if (!runningInSystemProfile()) return os.homedir();
  return path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
}

/**
 * Expand a leading ~ against the owning user's home. Only the leading ~ is
 * touched: a ~ anywhere else in a path is a legitimate character.
 */
export function expandHome(value) {
  if (typeof value !== 'string' || value.length === 0) return value;
  if (value === '~') return userHome();
  if (value.startsWith('~/') || value.startsWith('~\\')) return path.join(userHome(), value.slice(2));
  return value;
}

/** %APPDATA% / %LOCALAPPDATA%, ignoring the env under SYSTEM for the reason above. */
export function windowsAppData(kind) {
  if (!runningInSystemProfile()) {
    const fromEnv = kind === 'Local' ? process.env.LOCALAPPDATA : process.env.APPDATA;
    if (fromEnv) return fromEnv;
  }
  return path.join(userHome(), 'AppData', kind);
}

/**
 * Install base. Identical shape on every OS:
 *   <home>/.legion-control  |  C:\Users\<you>\.legion-control
 * LEGIONCTL_HOME relocates the whole layout, which is what the tests use and
 * what lets a second install live beside the real one.
 */
export function basePath() {
  if (process.env.LEGIONCTL_HOME) return process.env.LEGIONCTL_HOME;
  return path.join(userHome(), '.legion-control');
}

export function configPath() {
  return path.join(basePath(), 'config.json');
}

export function statePath() {
  return path.join(basePath(), 'state.json');
}

export function logPath() {
  return path.join(basePath(), 'legionctl.log');
}

/** Where a global npm install lands, given a prefix. */
export function globalModulesDir(prefix) {
  return IS_WINDOWS ? path.join(prefix, 'node_modules') : path.join(prefix, 'lib', 'node_modules');
}

/** The npm prefix a global install normally uses; a service's npmPrefix overrides it. */
export function defaultNpmPrefix() {
  if (IS_WINDOWS) return path.join(windowsAppData('Roaming'), 'npm');
  return path.join(userHome(), '.npm-global');
}

// ---------------------------------------------------------------------------
// Normalization helpers
// ---------------------------------------------------------------------------

function isPlainObject(value) {
  return Boolean(value) && typeof value === 'object' && !Array.isArray(value);
}

function str(value, fallback = null) {
  return typeof value === 'string' && value.trim().length > 0 ? value : fallback;
}

function bool(value, fallback) {
  return typeof value === 'boolean' ? value : fallback;
}

function port(value, fallback = null) {
  const parsed = Number(value);
  return Number.isInteger(parsed) && parsed > 0 && parsed < 65536 ? parsed : fallback;
}

function positive(value, fallback) {
  const parsed = Number(value);
  return Number.isFinite(parsed) && parsed > 0 ? parsed : fallback;
}

function nonNegative(value, fallback) {
  const parsed = Number(value);
  return Number.isFinite(parsed) && parsed >= 0 ? parsed : fallback;
}

/**
 * An argv array, never a shell string. A command given as a bare string is
 * rejected rather than split on spaces: guessing where the arguments are in
 * `sh -c "a b | c"` is how quoting bugs get shipped.
 */
export function argv(value) {
  if (!Array.isArray(value)) return null;
  const parts = value.filter((item) => typeof item === 'string' && item.length > 0).map((item) => expandHome(item));
  return parts.length > 0 ? parts : null;
}

function stringList(value, fallback = []) {
  if (!Array.isArray(value)) return fallback;
  const parts = value.filter((item) => typeof item === 'string' && item.trim().length > 0);
  return parts;
}

/** A configured regular expression, or the fallback when it does not compile. */
export function compilePattern(source, fallback) {
  for (const candidate of [source, fallback]) {
    if (typeof candidate !== 'string' || candidate.length === 0) continue;
    try {
      return new RegExp(candidate);
    } catch {
      /* an unusable pattern falls through to the fallback */
    }
  }
  return null;
}

// ---------------------------------------------------------------------------
// Probes
// ---------------------------------------------------------------------------

function normalizeProcess(raw, { kind }) {
  // kind "app" implies its process: running means a process inside the bundle,
  // stop means asking the app to quit, start means opening it again.
  if (kind === 'app') return { type: 'app' };
  if (!isPlainObject(raw)) return { type: 'none' };

  switch (raw.type) {
    case 'systemd-user':
    case 'systemd-system': {
      const unit = str(raw.unit);
      if (!unit) return { type: 'none' };
      return { type: raw.type, unit };
    }
    case 'scheduled-task': {
      const task = str(raw.task);
      if (!task) return { type: 'none' };
      // match is left null on purpose: its default is the npm package root, and
      // working that out costs a prefix probe that config loading must not do.
      return { type: 'scheduled-task', task, match: str(raw.match) };
    }
    case 'app':
      return { type: 'app' };
    case 'command':
      return {
        type: 'command',
        start: argv(raw.start),
        stop: argv(raw.stop),
        running: argv(raw.running),
      };
    default:
      return { type: 'none' };
  }
}

function normalizeHealth(raw) {
  if (!isPlainObject(raw)) return { type: 'none' };
  switch (raw.type) {
    case 'http':
      return {
        type: 'http',
        host: str(raw.host, '127.0.0.1'),
        port: port(raw.port),
        path: str(raw.path, '/'),
      };
    case 'command': {
      const command = argv(raw.command);
      return command ? { type: 'command', command } : { type: 'none' };
    }
    default:
      return { type: 'none' };
  }
}

function normalizeBusy(raw) {
  if (!isPlainObject(raw)) return { type: 'none' };
  switch (raw.type) {
    case 't3-sqlite':
      return {
        type: 't3-sqlite',
        home: str(raw.home) ? expandHome(raw.home) : null,
        staleHours: positive(raw.staleHours, LEGACY_DEFAULTS.staleTurnHours),
      };
    case 'command': {
      const command = argv(raw.command);
      if (!command) return { type: 'none' };
      return { type: 'command', command, staleHours: positive(raw.staleHours, null) };
    }
    case 'http':
      return {
        type: 'http',
        host: str(raw.host, '127.0.0.1'),
        port: port(raw.port),
        path: str(raw.path, '/'),
        busyWhen: str(raw.busyWhen, 'busy'),
      };
    default:
      return { type: 'none' };
  }
}

function normalizeRelay(raw) {
  if (!isPlainObject(raw)) return null;
  if (raw.type !== 'cloudflared') return null;
  return { type: 'cloudflared' };
}

function normalizeLatest(raw) {
  if (!isPlainObject(raw)) return null;
  if (raw.type !== 'github-releases') return null;
  const repo = str(raw.repo);
  if (!repo || !repo.includes('/')) return null;
  return { type: 'github-releases', repo, prerelease: bool(raw.prerelease, true) };
}

// ---------------------------------------------------------------------------
// Services
// ---------------------------------------------------------------------------

function defaultLockFile(base) {
  return path.join(base, 'update.lock');
}

function normalizeService(raw, index, { platform, base }) {
  if (!isPlainObject(raw)) return null;
  const id = str(raw.id, `service-${index + 1}`);
  const kind = ['npm', 'app', 'command'].includes(raw.kind) ? raw.kind : null;
  if (!kind) return null;

  const service = {
    id,
    name: str(raw.name, id),
    kind,
    process: normalizeProcess(raw.process, { kind }),
    health: normalizeHealth(raw.health),
    busy: normalizeBusy(raw.busy),
    relay: normalizeRelay(raw.relay),
    lockFile: expandHome(str(raw.lockFile, defaultLockFile(base))),
    channel: null,
  };

  if (kind === 'npm') {
    service.package = str(raw.package, id);
    service.channel = str(raw.channel, 'latest');
    service.versionPattern = str(raw.versionPattern, DEFAULT_VERSION_PATTERN);
    service.npmPrefix = str(raw.npmPrefix) ? expandHome(raw.npmPrefix) : null;
    service.allowScripts = stringList(raw.allowScripts);
  } else if (kind === 'app') {
    service.path = expandHome(str(raw.path, ''));
    service.bundleId = str(raw.bundleId);
    service.updaterCacheDir = expandHome(str(raw.updaterCacheDir, ''));
    service.stagedFilePattern = str(raw.stagedFilePattern, LEGACY_DEFAULTS.stagedFilePattern);
    service.latest = normalizeLatest(raw.latest);
    service.channel = str(raw.channel, service.latest?.prerelease === false ? 'stable' : 'nightly');
    service.versionPattern = str(raw.versionPattern, DEFAULT_VERSION_PATTERN);
  } else {
    service.installedVersion = argv(raw.installedVersion);
    service.latestVersion = argv(raw.latestVersion);
    service.update = argv(raw.update);
    service.versionPattern = str(raw.versionPattern, null);
  }

  // Unsupported platforms still parse the config; they simply cannot act on it.
  if (kind === 'app' && platform !== 'mac' && service.process.type === 'app') {
    service.process = { type: 'none' };
  }

  return service;
}

/**
 * The single T3 Code service the first version of the agent had hardwired,
 * rebuilt from the legacy top-level keys. A machine that was set up before
 * "services" existed keeps working, byte for byte, with no config change.
 */
function synthesizeLegacyServices(raw, { platform, base }) {
  const channel = str(raw.channel, LEGACY_DEFAULTS.channel);
  const health = { type: 'http', host: '127.0.0.1', port: port(raw.port, LEGACY_DEFAULTS.port), path: '/' };
  const busy = { type: 't3-sqlite', home: null, staleHours: positive(raw.staleTurnHours, LEGACY_DEFAULTS.staleTurnHours) };

  if (platform === 'mac') {
    // The Mac ran the Electron app rather than the npm package, so none of the
    // npm settings applied to it and none of these applied to the other systems.
    return [
      {
        id: 't3',
        name: 'T3 Code',
        kind: 'app',
        path: expandHome(str(raw.t3AppPath, LEGACY_DEFAULTS.t3AppPath)),
        bundleId: str(raw.t3AppBundleId, LEGACY_DEFAULTS.t3AppBundleId),
        updaterCacheDir: expandHome(str(raw.updaterCacheDir, LEGACY_DEFAULTS.updaterCacheDir)),
        stagedFilePattern: LEGACY_DEFAULTS.stagedFilePattern,
        latest: { type: 'github-releases', repo: 'pingdotgg/t3code', prerelease: channel !== 'stable' },
        channel,
        versionPattern: DEFAULT_VERSION_PATTERN,
        process: { type: 'app' },
        health,
        busy,
        // There is no relay on the Mac and there is not meant to be: the Mac is
        // the machine that reaches out, not the one that is reached.
        relay: null,
        lockFile: defaultLockFile(base),
      },
    ];
  }

  const service = {
    id: 't3',
    name: 'T3 Code',
    kind: 'npm',
    package: 't3',
    channel,
    // The first version only ever installed nightlies and refused anything that
    // was not shaped like one, so a legacy config on the nightly channel keeps
    // that stricter guard. Any other channel gets the ordinary semver shape.
    versionPattern: channel === 'nightly' ? LEGACY_DEFAULTS.nightlyPattern : DEFAULT_VERSION_PATTERN,
    npmPrefix: str(raw.npmPrefix) ? expandHome(raw.npmPrefix) : null,
    allowScripts: Array.isArray(raw.allowScripts) ? stringList(raw.allowScripts) : [...LEGACY_DEFAULTS.allowScripts],
    process:
      platform === 'windows'
        ? { type: 'scheduled-task', task: 'T3 Code Connect', match: null }
        : { type: 'systemd-user', unit: 't3-code.service' },
    health,
    busy,
    relay: { type: 'cloudflared' },
    // On Windows the maintenance lock is the file the existing watchdog task
    // already honours, so it has to be exactly that path and not one of ours.
    lockFile:
      platform === 'windows'
        ? path.join(windowsAppData('Local'), 'T3Code', 'update.lock')
        : defaultLockFile(base),
  };
  return [service];
}

// ---------------------------------------------------------------------------
// Boot, sleep, actions
// ---------------------------------------------------------------------------

const BOOT_METHODS = ['efi-bootnext', 'clear-bootsequence', 'bootsequence', 'grub-reboot', 'command'];

function normalizeBootTarget(id, raw) {
  if (!isPlainObject(raw)) return null;
  if (!BOOT_METHODS.includes(raw.method)) return null;
  const target = { id, name: str(raw.name), method: raw.method };
  if (raw.method === 'efi-bootnext') {
    target.match = str(raw.match);
    if (!target.match) return null;
  }
  if (raw.method === 'bootsequence' || raw.method === 'grub-reboot') {
    target.entry = str(raw.entry);
    if (!target.entry) return null;
  }
  if (raw.method === 'command') {
    target.arm = argv(raw.arm);
    target.verify = argv(raw.verify);
    if (!target.arm) return null;
  }
  return target;
}

/**
 * With nothing configured, a Linux system offers Windows through BootNext and a
 * Windows system offers Linux by clearing the firmware boot sequence. That is
 * what the first version of the agent did, and it keeps a controller that names
 * the two systems "linux" and "windows" working untouched.
 */
function legacyBootTargets(platform) {
  if (platform === 'linux') {
    return { windows: { id: 'windows', name: null, method: 'efi-bootnext', match: '^Windows Boot Manager\\b' } };
  }
  if (platform === 'windows') {
    return { linux: { id: 'linux', name: null, method: 'clear-bootsequence' } };
  }
  return {};
}

function normalizeBoot(raw, { platform }) {
  const boot = { targets: {}, reboot: null };
  if (!isPlainObject(raw)) return { ...boot, targets: legacyBootTargets(platform) };

  boot.reboot = argv(raw.reboot);
  if (!isPlainObject(raw.targets)) return { ...boot, targets: legacyBootTargets(platform) };

  for (const [id, value] of Object.entries(raw.targets)) {
    const target = normalizeBootTarget(id, value);
    if (target) boot.targets[id] = target;
  }
  // An empty or entirely unusable targets block is the same as none at all, so
  // the platform default still applies rather than leaving the system with no
  // way back to the other side.
  if (Object.keys(boot.targets).length === 0) boot.targets = legacyBootTargets(platform);
  return boot;
}

/**
 * The suspend tools to look for on Windows, best first. The built in rundll32
 * and .NET suspend calls are silently vetoed on some machines — they exit 0,
 * report success, and the machine keeps running — which is why this is a list of
 * external tools resolved at run time rather than one assumed path. Assuming one
 * and being wrong is the worst outcome a sleep button has: the command exits,
 * the app says the machine is going to sleep, and it is still sitting there.
 */
export const DEFAULT_WINDOWS_SLEEP_TOOLS = [
  'psshutdown64.exe',
  'psshutdown.exe',
  'C:\\Tools\\PSTools\\psshutdown64.exe',
  'C:\\Tools\\PSTools\\psshutdown.exe',
  'C:\\PSTools\\psshutdown64.exe',
  'C:\\PSTools\\psshutdown.exe',
];

function normalizeSleep(raw) {
  const sleepConfig = {
    before: [],
    settle: 2,
    command: null,
    tools: [...DEFAULT_WINDOWS_SLEEP_TOOLS],
  };
  if (!isPlainObject(raw)) return sleepConfig;

  if (Array.isArray(raw.before)) {
    sleepConfig.before = raw.before.map((entry) => argv(entry)).filter(Boolean);
  }
  sleepConfig.settle = nonNegative(raw.settle, sleepConfig.settle);
  sleepConfig.command = argv(raw.command);
  const tools = stringList(raw.tools).map((tool) => expandHome(tool));
  if (tools.length > 0) sleepConfig.tools = tools;
  return sleepConfig;
}

function normalizeActions(raw) {
  if (!Array.isArray(raw)) return [];
  const actions = [];
  const seen = new Set();
  for (const entry of raw) {
    if (!isPlainObject(entry)) continue;
    const id = str(entry.id);
    const command = argv(entry.command);
    if (!id || !command || seen.has(id)) continue;
    seen.add(id);
    actions.push({
      id,
      name: str(entry.name, id),
      command,
      confirm: str(entry.confirm),
      busyGated: bool(entry.busyGated, false),
      timeoutSeconds: positive(entry.timeoutSeconds, 60),
    });
  }
  return actions;
}

// ---------------------------------------------------------------------------
// The whole document
// ---------------------------------------------------------------------------

/**
 * Turn whatever was in config.json into the shape the rest of the agent works
 * with. Pure: everything that varies between machines comes in through options,
 * which is what lets the tests exercise all three platforms from one of them.
 */
export function normalizeConfig(raw, options = {}) {
  const source = isPlainObject(raw) ? raw : {};
  const platform = options.platform ?? detectPlatform();
  const base = options.base ?? basePath();

  const system = isPlainObject(source.system) ? source.system : {};
  const config = {
    system: {
      id: str(system.id, platform ?? process.platform),
      name: str(system.name, PLATFORM_SYSTEM_NAMES[platform] ?? (platform ?? process.platform)),
    },
    autoUpdate: bool(source.autoUpdate, true),
    services: [],
    boot: normalizeBoot(source.boot, { platform }),
    sleep: normalizeSleep(source.sleep),
    actions: normalizeActions(source.actions),
    // True when the services were rebuilt from the legacy top-level keys rather
    // than read from a "services" array. Only used for reporting.
    legacyServices: false,
  };

  if (Array.isArray(source.services)) {
    const seen = new Set();
    source.services.forEach((entry, index) => {
      const service = normalizeService(entry, index, { platform, base });
      if (!service || seen.has(service.id)) return;
      seen.add(service.id);
      config.services.push(service);
    });
  } else {
    config.services = synthesizeLegacyServices(source, { platform, base });
    config.legacyServices = true;
  }

  return config;
}

function ensureBaseDir() {
  try {
    fs.mkdirSync(basePath(), { recursive: true });
    return true;
  } catch {
    return false;
  }
}

function readJsonFile(file) {
  try {
    const raw = fs.readFileSync(file, 'utf8');
    const parsed = JSON.parse(raw);
    if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) return parsed;
    return null;
  } catch {
    return null;
  }
}

/** Write via a temp file + rename so a killed process cannot leave a truncated file. */
function writeJsonFile(file, value) {
  if (!ensureBaseDir()) return false;
  const tmp = `${file}.tmp`;
  try {
    fs.writeFileSync(tmp, `${JSON.stringify(value, null, 2)}\n`, 'utf8');
    fs.renameSync(tmp, file);
    return true;
  } catch {
    try {
      fs.rmSync(tmp, { force: true });
    } catch {
      /* nothing useful to do */
    }
    return false;
  }
}

/** config.json, normalized. A missing or corrupt file degrades to the defaults. */
export function loadConfig() {
  return normalizeConfig(readJsonFile(configPath()) || {});
}

/**
 * Merge a patch into config.json and return the normalized result.
 *
 * Only the keys that were already in the file plus the patch are written back.
 * Stamping the full set of defaults would freeze today's defaults into every
 * config the first time anyone toggled auto update.
 */
export function saveConfig(patch) {
  const stored = readJsonFile(configPath()) || {};
  const merged = { ...stored, ...patch };
  const written = writeJsonFile(configPath(), merged);
  return { ok: written, config: normalizeConfig(merged) };
}

export function loadState() {
  return readJsonFile(statePath()) || {};
}

/** Merge a patch into state.json. Returns the merged state even if the write failed. */
export function saveState(patch) {
  const merged = { ...loadState(), ...patch };
  writeJsonFile(statePath(), merged);
  return merged;
}

/**
 * Per-service corner of state.json. The first version of the agent kept these at
 * the top level because there was only ever one service; the first service still
 * falls back to them so an install that is upgraded in place does not appear to
 * forget its last update.
 */
export function serviceState(state, service, isFirst = false) {
  const scoped = isPlainObject(state.services) ? state.services[service.id] : null;
  if (isPlainObject(scoped)) return scoped;
  if (isFirst) {
    return {
      lastUpdate: state.lastUpdate ?? null,
      pendingRestart: state.pendingRestart ?? false,
      pendingVersion: state.pendingVersion ?? null,
      applyFailures: state.macApplyFailures ?? null,
    };
  }
  return {};
}

/** Merge a patch into one service's corner of state.json. */
export function saveServiceState(service, patch) {
  const state = loadState();
  const services = isPlainObject(state.services) ? { ...state.services } : {};
  services[service.id] = { ...(isPlainObject(services[service.id]) ? services[service.id] : {}), ...patch };
  return saveState({ services });
}

/**
 * Shared process runner. Always an argv array, never a shell string, so a
 * Windows npm prefix or "C:\Program Files\nodejs" cannot break quoting.
 */
export function runCommand(file, args = [], options = {}) {
  const result = spawnSync(file, args, {
    encoding: 'utf8',
    timeout: options.timeoutMs ?? 30000,
    windowsHide: true,
    maxBuffer: options.maxBuffer ?? 16 * 1024 * 1024,
    cwd: options.cwd,
    env: options.env ?? process.env,
    shell: options.shell === true,
  });

  const stdout = typeof result.stdout === 'string' ? result.stdout : '';
  const stderr = typeof result.stderr === 'string' ? result.stderr : '';
  const timedOut = result.error?.code === 'ETIMEDOUT' || result.signal === 'SIGTERM';

  // A PowerShell script passed as one argument is hundreds of characters long
  // and would otherwise end up verbatim in every error message.
  const full = [file, ...args].join(' ');
  const command = full.length > 160 ? `${full.slice(0, 160)}...` : full;

  return {
    ok: result.status === 0,
    code: result.status,
    signal: result.signal ?? null,
    stdout,
    stderr,
    timedOut,
    error: result.error ? String(result.error.message || result.error) : null,
    command,
  };
}

/** Run a configured argv array through the shared runner. */
export function runArgv(command, options = {}) {
  return runCommand(command[0], command.slice(1), options);
}

/** One-line failure description for a runCommand result. */
export function describeFailure(result) {
  if (result.timedOut) return `${result.command} timed out`;
  const output = (result.stderr || result.stdout || '').trim().split('\n').slice(0, 3).join(' ');
  const reason = output || result.error || '';
  const code = result.code === null || result.code === undefined ? 'no exit code' : `exit ${result.code}`;
  return reason ? `${result.command} failed (${code}): ${reason}` : `${result.command} failed (${code})`;
}

export function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
