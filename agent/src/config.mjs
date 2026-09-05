// Environment resolution and the config schema for the Legion Control agent:
// which OS we are on, where our files live, what this system is called, which
// services it looks after, and the shared process runner every other module uses.
//
// Nothing in here may throw at import time. index.mjs needs to be able to load
// the whole agent on an unsupported platform and print a clean JSON error.
//
// THE HANDLING RULE, and it is the opposite of what the first version did:
//
//   No file at all is INERT. Present but wrong is FATAL.
//
// A missing config.json used to mean "the documented defaults", and those
// defaults described a specific product, with automatic updates on. So a machine
// that had never been configured, or whose config had been moved, would happily
// decide it looked after a T3 Code install and start updating it. A machine with
// no configuration now looks after nothing and has automatic updates off, which
// is the only honest reading of "nobody has told me anything".
//
// A config.json that is truncated, unreadable or contains a key the agent does
// not understand is something else again: it is a machine whose owner tried to
// say something and was not heard. That case refuses to do anything, keeps the
// last copy that loaded cleanly for comparison, and says what to fix.
//
// Every rejection carries the path that was wrong and what to write instead,
// because the person reading it is on the other end of an SSH session with no
// editor and no documentation open.

import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { CONTRACT_VERSION } from './contract.mjs';
import { acquireFileLock, stateLockPath, writeJsonAtomic } from './state.mjs';

export const AGENT_VERSION = '3.0.0';

/** The config schema this agent understands. Bumped only for a breaking change. */
export const CONFIG_VERSION = 3;

// Anything semver-shaped. Used when a service does not pin a stricter pattern:
// the point of the check is to refuse garbage that would otherwise be handed
// straight to an installer, not to have an opinion about version schemes.
export const DEFAULT_VERSION_PATTERN = '^\\d+\\.\\d+\\.\\d+(?:[-+].*)?$';

// The first version of the agent looked after exactly one service, T3 Code, and
// described it with these keys at the top level of config.json. A config that
// still carries them keeps working: see synthesizeLegacyServices below. What no
// longer happens is synthesizing that service for a machine that never mentioned
// it. New configs use "services" instead.
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
 * config.json, the state directory, the service's own state database, the npm
 * prefix, the maintenance lock — belongs to the account that owns the install,
 * so resolving any of them from SYSTEM's profile silently reads empty defaults.
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

/** The last copy of config.json that loaded without a single complaint. */
export function lastGoodConfigPath() {
  return path.join(basePath(), 'config.last-good.json');
}

/** Serialised, reconstructible and durable state all live under here. */
export function stateDir() {
  return path.join(basePath(), 'state');
}

/** One file per operation. */
export function operationsDir() {
  return path.join(basePath(), 'ops');
}

/** Where an uploaded agent bundle is buffered before it is verified. */
export function incomingDir() {
  return path.join(basePath(), 'incoming');
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

/**
 * Collects everything wrong with a document instead of stopping at the first
 * problem. Someone fixing a config over SSH should learn about all four mistakes
 * in one round trip, not one per attempt.
 */
class Report {
  constructor() {
    this.errors = [];
    this.warnings = [];
    this.migrations = [];
  }

  error(where, message, fix = null) {
    this.errors.push({ level: 'error', path: where, message, fix });
    return null;
  }

  warn(where, message, fix = null) {
    this.warnings.push({ level: 'warning', path: where, message, fix });
  }

  migrate(where, message, fix = null) {
    this.migrations.push({ level: 'migration', path: where, message, fix });
  }

  get ok() {
    return this.errors.length === 0;
  }
}

/** A string that must be a string when it is present at all. */
function reqStr(report, where, value, fallback = null) {
  if (value === undefined || value === null) return fallback;
  if (typeof value !== 'string' || value.trim().length === 0) {
    return report.error(where, `expected a non-empty string, got ${JSON.stringify(value)}`);
  }
  return value;
}

function reqBool(report, where, value, fallback) {
  if (value === undefined || value === null) return fallback;
  if (typeof value !== 'boolean') {
    return report.error(where, `expected true or false, got ${JSON.stringify(value)}`, 'write it without quotes');
  }
  return value;
}

function reqPort(report, where, value, fallback = null) {
  if (value === undefined || value === null) return fallback;
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed <= 0 || parsed >= 65536) {
    return report.error(where, `expected a port between 1 and 65535, got ${JSON.stringify(value)}`);
  }
  return parsed;
}

function reqPositive(report, where, value, fallback) {
  if (value === undefined || value === null) return fallback;
  const parsed = Number(value);
  if (!Number.isFinite(parsed) || parsed <= 0) {
    return report.error(where, `expected a number greater than zero, got ${JSON.stringify(value)}`);
  }
  return parsed;
}

function reqNonNegative(report, where, value, fallback) {
  if (value === undefined || value === null) return fallback;
  const parsed = Number(value);
  if (!Number.isFinite(parsed) || parsed < 0) {
    return report.error(where, `expected a number of zero or more, got ${JSON.stringify(value)}`);
  }
  return parsed;
}

function reqIso(report, where, value, fallback = null) {
  if (value === undefined || value === null) return fallback;
  const text = reqStr(report, where, value);
  if (text === null) return fallback;
  if (Number.isNaN(Date.parse(text))) {
    return report.error(where, `expected an ISO timestamp, got ${JSON.stringify(value)}`, 'for example "2026-09-30T08:00:00Z"');
  }
  return text;
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

/** argv() that complains rather than quietly producing null. */
function reqArgv(report, where, value, { required = false } = {}) {
  if (value === undefined || value === null) {
    if (required) report.error(where, 'this command is required but was not given');
    return null;
  }
  if (typeof value === 'string') {
    return report.error(
      where,
      'a command must be an array of arguments, not a string',
      `write ["${value.split(/\s+/).join('", "')}"] — the agent never runs a shell, so it cannot split this for you`,
    );
  }
  if (!Array.isArray(value)) {
    return report.error(where, `expected an array of strings, got ${JSON.stringify(value)}`);
  }
  const parts = argv(value);
  if (!parts) return report.error(where, 'the command array has no usable arguments in it');
  return parts;
}

function stringList(report, where, value, fallback = []) {
  if (value === undefined || value === null) return fallback;
  if (!Array.isArray(value)) return report.error(where, `expected an array of strings, got ${JSON.stringify(value)}`) ?? [];
  const bad = value.find((item) => typeof item !== 'string');
  if (bad !== undefined) return report.error(where, `every entry must be a string; found ${JSON.stringify(bad)}`) ?? [];
  return value.filter((item) => item.trim().length > 0);
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

function reqPattern(report, where, value, fallback) {
  if (value === undefined || value === null) return fallback;
  if (typeof value !== 'string' || value.length === 0) {
    return report.error(where, `expected a regular expression as a string, got ${JSON.stringify(value)}`);
  }
  try {
    new RegExp(value);
  } catch (err) {
    return report.error(where, `not a valid regular expression: ${err.message}`);
  }
  return value;
}

/** Reject keys the agent does not understand, so a typo is never silently ignored. */
function rejectUnknownKeys(report, where, object, known) {
  if (!isPlainObject(object)) return;
  for (const key of Object.keys(object)) {
    if (known.includes(key)) continue;
    const near = known.find((candidate) => candidate.toLowerCase() === key.toLowerCase());
    report.error(
      `${where}.${key}`,
      `unknown setting "${key}"`,
      near ? `did you mean "${near}"?` : `this block accepts: ${known.join(', ')}`,
    );
  }
}

// ---------------------------------------------------------------------------
// Probes
// ---------------------------------------------------------------------------

const PROCESS_KEYS = ['type', 'unit', 'task', 'match', 'start', 'stop', 'running'];

function normalizeProcess(report, where, raw, { kind }) {
  // kind "app" implies its process: running means a process inside the bundle,
  // stop means asking the app to quit, start means opening it again.
  if (kind === 'app' && raw === undefined) return { type: 'app' };
  if (raw === undefined || raw === null) return { type: 'none' };
  if (!isPlainObject(raw)) {
    return report.error(where, `expected an object describing the process, got ${JSON.stringify(raw)}`) ?? { type: 'none' };
  }
  rejectUnknownKeys(report, where, raw, PROCESS_KEYS);

  switch (raw.type) {
    case 'systemd-user':
    case 'systemd-system': {
      const unit = reqStr(report, `${where}.unit`, raw.unit);
      if (!unit) {
        report.error(`${where}.unit`, `a ${raw.type} process needs the unit name`, 'for example "t3-code.service"');
        return { type: 'none' };
      }
      return { type: raw.type, unit };
    }
    case 'scheduled-task': {
      const task = reqStr(report, `${where}.task`, raw.task);
      if (!task) {
        report.error(`${where}.task`, 'a scheduled-task process needs the task name');
        return { type: 'none' };
      }
      // match is left null on purpose: its default is the npm package root, and
      // working that out costs a prefix probe that config loading must not do.
      return { type: 'scheduled-task', task, match: reqStr(report, `${where}.match`, raw.match) };
    }
    case 'app':
      return { type: 'app' };
    case 'command': {
      const start = reqArgv(report, `${where}.start`, raw.start);
      const stop = reqArgv(report, `${where}.stop`, raw.stop);
      const running = reqArgv(report, `${where}.running`, raw.running);
      if (!start && !stop && !running) {
        report.error(
          where,
          'a command process needs at least one of start, stop or running',
          'without any of them the agent cannot tell whether the service is up, let alone act on it',
        );
      }
      return { type: 'command', start, stop, running };
    }
    case 'none':
      return { type: 'none' };
    default:
      return (
        report.error(
          `${where}.type`,
          `unknown process type ${JSON.stringify(raw.type)}`,
          'one of: systemd-user, systemd-system, scheduled-task, app, command, none',
        ) ?? { type: 'none' }
      );
  }
}

const HEALTH_KEYS = ['type', 'host', 'port', 'path', 'command', 'timeoutSeconds'];

function normalizeHealth(report, where, raw) {
  if (raw === undefined || raw === null) return { type: 'none', timeoutSeconds: 5 };
  if (!isPlainObject(raw)) {
    return (
      report.error(where, `expected an object describing the health check, got ${JSON.stringify(raw)}`) ?? {
        type: 'none',
        timeoutSeconds: 5,
      }
    );
  }
  rejectUnknownKeys(report, where, raw, HEALTH_KEYS);
  const timeoutSeconds = reqPositive(report, `${where}.timeoutSeconds`, raw.timeoutSeconds, 5);

  switch (raw.type) {
    case 'http': {
      const value = {
        type: 'http',
        host: reqStr(report, `${where}.host`, raw.host, '127.0.0.1'),
        port: reqPort(report, `${where}.port`, raw.port),
        path: reqStr(report, `${where}.path`, raw.path, '/'),
        timeoutSeconds,
      };
      if (value.port === null) {
        report.error(`${where}.port`, 'an http health check needs a port', 'for example 3773');
      }
      return value;
    }
    case 'command': {
      const command = reqArgv(report, `${where}.command`, raw.command, { required: true });
      return command ? { type: 'command', command, timeoutSeconds } : { type: 'none', timeoutSeconds };
    }
    case 'none':
      return { type: 'none', timeoutSeconds };
    default:
      return (
        report.error(`${where}.type`, `unknown health type ${JSON.stringify(raw.type)}`, 'one of: http, command, none') ?? {
          type: 'none',
          timeoutSeconds,
        }
      );
  }
}

const BUSY_KEYS = ['type', 'home', 'staleHours', 'command', 'host', 'port', 'path', 'busyWhen', 'allowMissing', 'timeoutSeconds'];

/**
 * The busy source for a service.
 *
 * "unmonitored" is not the same as "none", and conflating them is what let the
 * first version report a service as idle because nobody had said how to tell. A
 * service with no busy block probes as UNMONITORED, which is reported honestly
 * and warned about, rather than being quietly treated as a service with no work
 * worth protecting. Writing {"type": "none"} is how you say that on purpose, and
 * it is a decision the config has to state out loud.
 */
function normalizeBusy(report, where, raw, { serviceId }) {
  if (raw === undefined || raw === null) {
    report.migrate(
      where,
      `${serviceId} does not say how to tell whether it is busy, so disruptive actions will be refused for it`,
      'add "busy": {"type": "none"} to declare it never busy, or a real probe (t3-sqlite, command, http)',
    );
    return { type: 'unmonitored', timeoutSeconds: 8 };
  }
  if (!isPlainObject(raw)) {
    return (
      report.error(where, `expected an object describing the busy probe, got ${JSON.stringify(raw)}`) ?? {
        type: 'unmonitored',
        timeoutSeconds: 8,
      }
    );
  }
  rejectUnknownKeys(report, where, raw, BUSY_KEYS);
  const timeoutSeconds = reqPositive(report, `${where}.timeoutSeconds`, raw.timeoutSeconds, 8);

  switch (raw.type) {
    case 't3-sqlite': {
      const home = reqStr(report, `${where}.home`, raw.home);
      return {
        type: 't3-sqlite',
        home: home ? expandHome(home) : null,
        staleHours: reqPositive(report, `${where}.staleHours`, raw.staleHours, LEGACY_DEFAULTS.staleTurnHours),
        // A database that is not there is normally a machine where the product
        // has never run — but it is also exactly what a wrong path looks like,
        // and the two are indistinguishable from here. Absent is treated as
        // unknown (and therefore blocking) unless the config says otherwise.
        allowMissing: reqBool(report, `${where}.allowMissing`, raw.allowMissing, false),
        timeoutSeconds,
      };
    }
    case 'command': {
      const command = reqArgv(report, `${where}.command`, raw.command, { required: true });
      if (!command) return { type: 'unmonitored', timeoutSeconds };
      return {
        type: 'command',
        command,
        staleHours: reqPositive(report, `${where}.staleHours`, raw.staleHours, null),
        timeoutSeconds,
      };
    }
    case 'http': {
      const value = {
        type: 'http',
        host: reqStr(report, `${where}.host`, raw.host, '127.0.0.1'),
        port: reqPort(report, `${where}.port`, raw.port),
        path: reqStr(report, `${where}.path`, raw.path, '/'),
        busyWhen: reqStr(report, `${where}.busyWhen`, raw.busyWhen, 'busy'),
        timeoutSeconds,
      };
      if (value.port === null) report.error(`${where}.port`, 'an http busy probe needs a port');
      return value;
    }
    case 'none':
      return { type: 'none', timeoutSeconds };
    default:
      return (
        report.error(
          `${where}.type`,
          `unknown busy type ${JSON.stringify(raw.type)}`,
          'one of: t3-sqlite, command, http, none. "none" means this service has no work worth protecting',
        ) ?? { type: 'unmonitored', timeoutSeconds }
      );
  }
}

/**
 * A drain command: ask the service to stop accepting new work and finish what it
 * has, before it is stopped.
 *
 * Checking is not preventing. The busy gate notices work; only the service
 * itself can refuse to start more, and where it can, we ask it to.
 */
function normalizeDrain(report, where, raw) {
  if (raw === undefined || raw === null) return null;
  if (!isPlainObject(raw)) return report.error(where, `expected an object, got ${JSON.stringify(raw)}`);
  rejectUnknownKeys(report, where, raw, ['command', 'timeoutSeconds']);
  const command = reqArgv(report, `${where}.command`, raw.command, { required: true });
  if (!command) return null;
  return { command, timeoutSeconds: reqPositive(report, `${where}.timeoutSeconds`, raw.timeoutSeconds, 30) };
}

function normalizeRelay(report, where, raw) {
  if (raw === undefined || raw === null) return null;
  if (!isPlainObject(raw)) return report.error(where, `expected an object, got ${JSON.stringify(raw)}`);
  rejectUnknownKeys(report, where, raw, ['type']);
  if (raw.type === 'none') return null;
  if (raw.type !== 'cloudflared') {
    return report.error(`${where}.type`, `unknown relay type ${JSON.stringify(raw.type)}`, 'one of: cloudflared, none');
  }
  return { type: 'cloudflared' };
}

function normalizeLatest(report, where, raw) {
  if (raw === undefined || raw === null) return null;
  if (!isPlainObject(raw)) return report.error(where, `expected an object, got ${JSON.stringify(raw)}`);
  rejectUnknownKeys(report, where, raw, ['type', 'repo', 'prerelease']);
  if (raw.type !== 'github-releases') {
    return report.error(`${where}.type`, `unknown release feed ${JSON.stringify(raw.type)}`, 'only "github-releases" is supported');
  }
  const repo = reqStr(report, `${where}.repo`, raw.repo);
  if (!repo || !repo.includes('/')) {
    return report.error(`${where}.repo`, 'expected "owner/name"', 'for example "pingdotgg/t3code"');
  }
  return { type: 'github-releases', repo, prerelease: reqBool(report, `${where}.prerelease`, raw.prerelease, true) };
}

/**
 * An external endpoint the service is meant to be reachable at.
 *
 * Never probed by an ordinary status poll: turning every poll into an Internet
 * round trip would make the thing people press twenty times a day slower and
 * flakier for no gain. It exists for `doctor --deep`, where the question
 * actually being asked is "can anything outside this machine get to it".
 */
function normalizeEndpoint(report, where, raw) {
  if (raw === undefined || raw === null) return null;
  if (!isPlainObject(raw)) return report.error(where, `expected an object, got ${JSON.stringify(raw)}`);
  rejectUnknownKeys(report, where, raw, ['url', 'timeoutSeconds', 'expectStatus']);
  const url = reqStr(report, `${where}.url`, raw.url);
  if (!url) return report.error(`${where}.url`, 'an endpoint needs a url', 'for example "https://t3.example.com/"');
  let parsed;
  try {
    parsed = new URL(url);
  } catch {
    return report.error(`${where}.url`, `not a usable URL: ${JSON.stringify(url)}`);
  }
  if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
    return report.error(`${where}.url`, `only http and https endpoints are supported, got ${parsed.protocol}`);
  }
  return {
    url,
    timeoutSeconds: reqPositive(report, `${where}.timeoutSeconds`, raw.timeoutSeconds, 8),
    expectStatus: reqPositive(report, `${where}.expectStatus`, raw.expectStatus, 200),
  };
}

// ---------------------------------------------------------------------------
// The update policy block
// ---------------------------------------------------------------------------

export const WEEKDAYS = ['sun', 'mon', 'tue', 'wed', 'thu', 'fri', 'sat'];
const UPDATES_KEYS = ['automatic', 'pauseUntil', 'maintenanceWindows', 'order', 'after'];

function normalizeWindow(report, where, raw) {
  if (!isPlainObject(raw)) return report.error(where, `expected an object, got ${JSON.stringify(raw)}`);
  rejectUnknownKeys(report, where, raw, ['days', 'from', 'to']);

  const days = stringList(report, `${where}.days`, raw.days, [...WEEKDAYS]).map((day) => day.slice(0, 3).toLowerCase());
  const unknownDay = days.find((day) => !WEEKDAYS.includes(day));
  if (unknownDay) report.error(`${where}.days`, `unknown day ${JSON.stringify(unknownDay)}`, `one of: ${WEEKDAYS.join(', ')}`);

  const time = (key, fallback) => {
    if (raw[key] === undefined || raw[key] === null) return fallback;
    const value = raw[key];
    if (typeof value !== 'string' || !/^([01]\d|2[0-3]):[0-5]\d$/.test(value)) {
      report.error(`${where}.${key}`, `expected a 24-hour local time like "02:30", got ${JSON.stringify(value)}`);
      return fallback;
    }
    return value;
  };

  return { days: days.length > 0 ? days : [...WEEKDAYS], from: time('from', '00:00'), to: time('to', '23:59') };
}

/**
 * `updates`, at the top level and per service.
 *
 * Per service, every value may be null, which means "inherit". That is what lets
 * one noisy service be taken off the schedule without taking the machine off it,
 * and it is why the per-service default is null rather than the system value
 * copied in: a config that copies the value freezes it, and changing the machine
 * switch afterwards would then do nothing.
 *
 * `order` and `after` are additive, optional and outside the frozen contract:
 * they only decide the order in which a cycle walks services it was going to
 * walk anyway. A client that does not know about them loses nothing.
 */
function normalizeUpdates(report, where, raw, { perService }) {
  const value = perService
    ? { automatic: null, pauseUntil: null, maintenanceWindows: null, order: 0, after: [] }
    : { automatic: true, pauseUntil: null, maintenanceWindows: [] };

  if (raw === undefined || raw === null) return value;
  if (!isPlainObject(raw)) {
    report.error(where, `expected an object describing the update policy, got ${JSON.stringify(raw)}`);
    return value;
  }
  rejectUnknownKeys(report, where, raw, perService ? UPDATES_KEYS : ['automatic', 'pauseUntil', 'maintenanceWindows']);

  value.automatic = reqBool(report, `${where}.automatic`, raw.automatic, value.automatic);
  value.pauseUntil = reqIso(report, `${where}.pauseUntil`, raw.pauseUntil, null);

  if (raw.maintenanceWindows !== undefined && raw.maintenanceWindows !== null) {
    if (!Array.isArray(raw.maintenanceWindows)) {
      report.error(
        `${where}.maintenanceWindows`,
        `expected an array of windows, got ${JSON.stringify(raw.maintenanceWindows)}`,
        'an empty array means "any time"; omit the key on a service to inherit',
      );
    } else {
      value.maintenanceWindows = raw.maintenanceWindows
        .map((entry, index) => normalizeWindow(report, `${where}.maintenanceWindows[${index}]`, entry))
        .filter(Boolean);
    }
  }

  if (perService) {
    value.order = reqNonNegative(report, `${where}.order`, raw.order, 0);
    value.after = stringList(report, `${where}.after`, raw.after, []);
  }
  return value;
}

// ---------------------------------------------------------------------------
// Services
// ---------------------------------------------------------------------------

function defaultLockFile(base) {
  return path.join(base, 'update.lock');
}

const COMMON_SERVICE_KEYS = [
  'id',
  'name',
  'kind',
  'process',
  'health',
  'busy',
  'drain',
  'relay',
  'endpoint',
  'lockFile',
  'updates',
  'versionPattern',
  'channel',
];
const KIND_KEYS = {
  npm: ['package', 'npmPrefix', 'allowScripts'],
  app: ['path', 'bundleId', 'updaterCacheDir', 'stagedFilePattern', 'latest'],
  command: ['installedVersion', 'latestVersion', 'update', 'verify', 'rollback', 'requireVersionMatch'],
};

/** Ids travel over ssh as bare tokens, so they are held to the token grammar. */
const ID_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._-]*$/;

function normalizeService(report, raw, index, { platform, base }) {
  const where = `services[${index}]`;
  if (!isPlainObject(raw)) {
    return report.error(where, `expected an object describing a service, got ${JSON.stringify(raw)}`);
  }
  const id = reqStr(report, `${where}.id`, raw.id, `service-${index + 1}`);
  if (id !== null && !ID_PATTERN.test(id)) {
    report.error(
      `${where}.id`,
      `the id ${JSON.stringify(id)} has characters that cannot be passed safely over SSH`,
      'use letters, digits, dot, dash and underscore only',
    );
  }
  const kind = ['npm', 'app', 'command'].includes(raw.kind) ? raw.kind : null;
  if (!kind) {
    return report.error(`${where}.kind`, `unknown service kind ${JSON.stringify(raw.kind)}`, 'one of: npm, app, command');
  }
  rejectUnknownKeys(report, where, raw, [...COMMON_SERVICE_KEYS, ...KIND_KEYS[kind]]);

  const service = {
    id,
    name: reqStr(report, `${where}.name`, raw.name, id),
    kind,
    process: normalizeProcess(report, `${where}.process`, raw.process, { kind }),
    health: normalizeHealth(report, `${where}.health`, raw.health),
    busy: normalizeBusy(report, `${where}.busy`, raw.busy, { serviceId: id }),
    drain: normalizeDrain(report, `${where}.drain`, raw.drain),
    relay: normalizeRelay(report, `${where}.relay`, raw.relay),
    endpoint: normalizeEndpoint(report, `${where}.endpoint`, raw.endpoint),
    lockFile: expandHome(reqStr(report, `${where}.lockFile`, raw.lockFile, defaultLockFile(base))),
    updates: normalizeUpdates(report, `${where}.updates`, raw.updates, { perService: true }),
    channel: null,
  };

  if (kind === 'npm') {
    service.package = reqStr(report, `${where}.package`, raw.package, id);
    service.channel = reqStr(report, `${where}.channel`, raw.channel, 'latest');
    service.versionPattern = reqPattern(report, `${where}.versionPattern`, raw.versionPattern, DEFAULT_VERSION_PATTERN);
    const prefix = reqStr(report, `${where}.npmPrefix`, raw.npmPrefix);
    service.npmPrefix = prefix ? expandHome(prefix) : null;
    service.allowScripts = stringList(report, `${where}.allowScripts`, raw.allowScripts, []);
  } else if (kind === 'app') {
    const appPath = reqStr(report, `${where}.path`, raw.path);
    if (!appPath) report.error(`${where}.path`, 'an app service needs the bundle path', 'for example "/Applications/Thing.app"');
    service.path = expandHome(appPath ?? '');
    service.bundleId = reqStr(report, `${where}.bundleId`, raw.bundleId);
    const cache = reqStr(report, `${where}.updaterCacheDir`, raw.updaterCacheDir);
    service.updaterCacheDir = expandHome(cache ?? '');
    service.stagedFilePattern = reqPattern(
      report,
      `${where}.stagedFilePattern`,
      raw.stagedFilePattern,
      LEGACY_DEFAULTS.stagedFilePattern,
    );
    service.latest = normalizeLatest(report, `${where}.latest`, raw.latest);
    service.channel = reqStr(report, `${where}.channel`, raw.channel, service.latest?.prerelease === false ? 'stable' : 'nightly');
    service.versionPattern = reqPattern(report, `${where}.versionPattern`, raw.versionPattern, DEFAULT_VERSION_PATTERN);
  } else {
    service.installedVersion = reqArgv(report, `${where}.installedVersion`, raw.installedVersion);
    service.latestVersion = reqArgv(report, `${where}.latestVersion`, raw.latestVersion);
    service.update = reqArgv(report, `${where}.update`, raw.update);
    // The postcondition. An update command that exits zero has proved nothing:
    // the first version accepted whatever version it read afterwards, so a no-op
    // updater reported a completed update it had never performed.
    service.verify = reqArgv(report, `${where}.verify`, raw.verify);
    service.rollback = reqArgv(report, `${where}.rollback`, raw.rollback);
    service.requireVersionMatch = reqBool(
      report,
      `${where}.requireVersionMatch`,
      raw.requireVersionMatch,
      // Comparing versions is only meaningful when both can be read. Where they
      // can, equality is required by default and the update fails without it.
      Boolean(raw.installedVersion && raw.latestVersion),
    );
    service.versionPattern = reqPattern(report, `${where}.versionPattern`, raw.versionPattern, null);
    if (service.update && !service.verify && !service.requireVersionMatch) {
      report.warn(
        `${where}.verify`,
        `${id} has an update command whose result cannot be checked`,
        'add "verify" (a command that fails when the update did not take) or an installedVersion/latestVersion pair, otherwise an update can only ever be reported as unverified',
      );
    }
    service.channel = reqStr(report, `${where}.channel`, raw.channel, null);
  }

  // Unsupported platforms still parse the config; they simply cannot act on it.
  if (kind === 'app' && platform !== 'mac' && service.process.type === 'app') {
    service.process = { type: 'none' };
  }

  return service;
}

/**
 * The single T3 Code service the first version of the agent had hardwired,
 * rebuilt from the legacy top-level keys.
 *
 * This only ever runs for a config file that actually carries one of those keys.
 * A machine with no config at all, or with a config that simply has no services,
 * looks after nothing — inventing a service for it is how an unconfigured
 * machine ended up quietly updating something.
 */
function synthesizeLegacyServices(report, raw, { platform, base }) {
  const channel = reqStr(report, 'channel', raw.channel, LEGACY_DEFAULTS.channel);
  const health = {
    type: 'http',
    host: '127.0.0.1',
    port: reqPort(report, 'port', raw.port, LEGACY_DEFAULTS.port),
    path: '/',
    timeoutSeconds: 5,
  };
  const busy = {
    type: 't3-sqlite',
    home: null,
    staleHours: reqPositive(report, 'staleTurnHours', raw.staleTurnHours, LEGACY_DEFAULTS.staleTurnHours),
    // The legacy shape is the one install that genuinely might not have run T3
    // yet, and it has no way to say so. Keep its previous meaning rather than
    // blocking every disruptive action on a machine that was working yesterday.
    allowMissing: true,
    timeoutSeconds: 8,
  };
  const updates = { automatic: null, pauseUntil: null, maintenanceWindows: null, order: 0, after: [] };

  if (platform === 'mac') {
    // The Mac ran the Electron app rather than the npm package, so none of the
    // npm settings applied to it and none of these applied to the other systems.
    const appPath = reqStr(report, 't3AppPath', raw.t3AppPath, LEGACY_DEFAULTS.t3AppPath);
    return [
      {
        id: 't3',
        name: 'T3 Code',
        kind: 'app',
        path: expandHome(appPath),
        bundleId: reqStr(report, 't3AppBundleId', raw.t3AppBundleId, LEGACY_DEFAULTS.t3AppBundleId),
        updaterCacheDir: expandHome(reqStr(report, 'updaterCacheDir', raw.updaterCacheDir, LEGACY_DEFAULTS.updaterCacheDir)),
        stagedFilePattern: LEGACY_DEFAULTS.stagedFilePattern,
        latest: { type: 'github-releases', repo: 'pingdotgg/t3code', prerelease: channel !== 'stable' },
        channel,
        versionPattern: DEFAULT_VERSION_PATTERN,
        process: { type: 'app' },
        health,
        busy,
        drain: null,
        // There is no relay on the Mac and there is not meant to be: the Mac is
        // the machine that reaches out, not the one that is reached.
        relay: null,
        endpoint: null,
        lockFile: defaultLockFile(base),
        updates,
      },
    ];
  }

  const npmPrefix = reqStr(report, 'npmPrefix', raw.npmPrefix);
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
    npmPrefix: npmPrefix ? expandHome(npmPrefix) : null,
    allowScripts: Array.isArray(raw.allowScripts)
      ? stringList(report, 'allowScripts', raw.allowScripts, [])
      : [...LEGACY_DEFAULTS.allowScripts],
    process:
      platform === 'windows'
        ? { type: 'scheduled-task', task: 'T3 Code Connect', match: null }
        : { type: 'systemd-user', unit: 't3-code.service' },
    health,
    busy,
    drain: null,
    relay: { type: 'cloudflared' },
    endpoint: null,
    // On Windows the maintenance lock is the file the existing watchdog task
    // already honours, so it has to be exactly that path and not one of ours.
    lockFile:
      platform === 'windows'
        ? path.join(windowsAppData('Local'), 'T3Code', 'update.lock')
        : defaultLockFile(base),
    updates,
  };
  return [service];
}

// ---------------------------------------------------------------------------
// Boot, sleep, actions
// ---------------------------------------------------------------------------

const BOOT_METHODS = ['efi-bootnext', 'clear-bootsequence', 'bootsequence', 'grub-reboot', 'command'];

function normalizeBootTarget(report, where, id, raw) {
  if (!isPlainObject(raw)) return report.error(where, `expected an object describing a boot target, got ${JSON.stringify(raw)}`);
  rejectUnknownKeys(report, where, raw, ['name', 'method', 'match', 'entry', 'arm', 'verify']);
  if (!ID_PATTERN.test(id)) {
    report.error(`${where}`, `the boot target id ${JSON.stringify(id)} cannot be passed safely over SSH`, 'use letters, digits, dot, dash and underscore only');
  }
  if (!BOOT_METHODS.includes(raw.method)) {
    return report.error(`${where}.method`, `unknown boot method ${JSON.stringify(raw.method)}`, `one of: ${BOOT_METHODS.join(', ')}`);
  }
  const target = { id, name: reqStr(report, `${where}.name`, raw.name), method: raw.method };
  if (raw.method === 'efi-bootnext') {
    target.match = reqPattern(report, `${where}.match`, raw.match, null);
    if (!target.match) {
      return report.error(`${where}.match`, 'an efi-bootnext target needs a match pattern', 'for example "^Windows Boot Manager\\\\b"');
    }
  }
  if (raw.method === 'bootsequence' || raw.method === 'grub-reboot') {
    target.entry = reqStr(report, `${where}.entry`, raw.entry);
    if (!target.entry) return report.error(`${where}.entry`, `a ${raw.method} target needs an entry`);
  }
  if (raw.method === 'command') {
    target.arm = reqArgv(report, `${where}.arm`, raw.arm, { required: true });
    target.verify = reqArgv(report, `${where}.verify`, raw.verify);
    if (!target.arm) return null;
    if (!target.verify) {
      report.warn(
        `${where}.verify`,
        `the boot target ${id} cannot be read back after it is armed`,
        'add "verify", a command that fails unless the target really is armed; without it a failed arm is followed by a reboot into the system you were already on',
      );
    }
  }
  return target;
}

/**
 * With nothing configured at all, a Linux system offers Windows through BootNext
 * and a Windows system offers Linux by clearing the firmware boot sequence. That
 * is what the first version of the agent did, and it keeps a controller that
 * names the two systems "linux" and "windows" working untouched.
 *
 * An explicitly EMPTY targets object is a different statement — "this machine
 * has nowhere else to boot" — and is honoured as written.
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

function normalizeBoot(report, raw, { platform, legacy }) {
  const boot = { targets: {}, reboot: null, rebootHelper: null };
  const fallback = legacy ? legacyBootTargets(platform) : {};

  if (raw === undefined || raw === null) return { ...boot, targets: fallback };
  if (!isPlainObject(raw)) {
    report.error('boot', `expected an object, got ${JSON.stringify(raw)}`);
    return { ...boot, targets: fallback };
  }
  rejectUnknownKeys(report, 'boot', raw, ['targets', 'reboot', 'rebootHelper']);

  boot.reboot = reqArgv(report, 'boot.reboot', raw.reboot);
  // A constrained helper is the recommended way to reboot with elevation: one
  // program that does exactly one thing, rather than a root shell.
  boot.rebootHelper = reqArgv(report, 'boot.rebootHelper', raw.rebootHelper);

  if (raw.targets === undefined || raw.targets === null) return { ...boot, targets: fallback };
  if (!isPlainObject(raw.targets)) {
    report.error('boot.targets', `expected an object keyed by target id, got ${JSON.stringify(raw.targets)}`);
    return { ...boot, targets: fallback };
  }

  for (const [id, value] of Object.entries(raw.targets)) {
    const target = normalizeBootTarget(report, `boot.targets.${id}`, id, value);
    if (target) boot.targets[id] = target;
  }
  return boot;
}

/**
 * The suspend tools to look for on Windows, best first. The built in rundll32
 * and .NET suspend calls are silently vetoed on some machines — they exit 0,
 * report success, and the machine keeps running — which is why this is a list of
 * external tools resolved at run time rather than one assumed path.
 */
export const DEFAULT_WINDOWS_SLEEP_TOOLS = [
  'psshutdown64.exe',
  'psshutdown.exe',
  'C:\\Tools\\PSTools\\psshutdown64.exe',
  'C:\\Tools\\PSTools\\psshutdown.exe',
  'C:\\PSTools\\psshutdown64.exe',
  'C:\\PSTools\\psshutdown.exe',
];

function normalizeSleep(report, raw) {
  const sleepConfig = { before: [], settle: 2, command: null, tools: [...DEFAULT_WINDOWS_SLEEP_TOOLS] };
  if (raw === undefined || raw === null) return sleepConfig;
  if (!isPlainObject(raw)) {
    report.error('sleep', `expected an object, got ${JSON.stringify(raw)}`);
    return sleepConfig;
  }
  rejectUnknownKeys(report, 'sleep', raw, ['before', 'settle', 'command', 'tools']);

  if (raw.before !== undefined && raw.before !== null) {
    if (!Array.isArray(raw.before)) {
      report.error('sleep.before', `expected an array of commands, got ${JSON.stringify(raw.before)}`);
    } else {
      sleepConfig.before = raw.before
        .map((entry, index) => reqArgv(report, `sleep.before[${index}]`, entry, { required: true }))
        .filter(Boolean);
    }
  }
  sleepConfig.settle = reqNonNegative(report, 'sleep.settle', raw.settle, sleepConfig.settle);
  sleepConfig.command = reqArgv(report, 'sleep.command', raw.command);
  const tools = stringList(report, 'sleep.tools', raw.tools, []).map((tool) => expandHome(tool));
  if (tools.length > 0) sleepConfig.tools = tools;
  return sleepConfig;
}

/**
 * A wake-on-LAN action: the agent sends the magic packet itself.
 *
 * This is what lets a machine that is already awake on a LAN wake another one on
 * the same LAN, without anybody installing a third-party wakeonlan binary and
 * without the controller having to be on that network at all. It is an action
 * rather than a new verb because `run <id>` already exists, is already allowed
 * for a restricted key, and already takes the operation lock.
 */
function normalizeWol(report, where, raw) {
  if (!isPlainObject(raw)) return report.error(where, `expected an object, got ${JSON.stringify(raw)}`);
  rejectUnknownKeys(report, where, raw, ['mac', 'broadcast', 'ports', 'repeats']);

  const mac = reqStr(report, `${where}.mac`, raw.mac);
  if (!mac || !/^[0-9A-Fa-f]{2}(?:[:-][0-9A-Fa-f]{2}){5}$/.test(mac)) {
    return report.error(`${where}.mac`, `expected six hex pairs, got ${JSON.stringify(raw.mac)}`, 'for example "AA:BB:CC:DD:EE:FF"');
  }

  const broadcast = stringList(report, `${where}.broadcast`, raw.broadcast, []);
  if (broadcast.length === 0) {
    return report.error(
      `${where}.broadcast`,
      'a wol action needs at least one broadcast address',
      'the LAN broadcast of the network the sleeping machine is on, for example "192.168.1.255"',
    );
  }

  let ports = [9, 7];
  if (raw.ports !== undefined && raw.ports !== null) {
    if (!Array.isArray(raw.ports)) {
      report.error(`${where}.ports`, `expected an array of port numbers, got ${JSON.stringify(raw.ports)}`);
    } else {
      ports = raw.ports.map((port, index) => reqPort(report, `${where}.ports[${index}]`, port)).filter((port) => port !== null);
      if (ports.length === 0) report.error(`${where}.ports`, 'no usable port numbers');
    }
  }

  const repeats = reqPositive(report, `${where}.repeats`, raw.repeats, 3);
  if (repeats !== null && repeats > 10) {
    report.error(`${where}.repeats`, `at most 10 packets per address, got ${repeats}`, 'three is plenty; more is just noise on the wire');
  }

  return { mac: mac.replace(/-/g, ':').toUpperCase(), broadcast, ports, repeats: Math.min(repeats ?? 3, 10) };
}

function normalizeActions(report, raw) {
  if (raw === undefined || raw === null) return [];
  if (!Array.isArray(raw)) {
    report.error('actions', `expected an array of actions, got ${JSON.stringify(raw)}`);
    return [];
  }
  const actions = [];
  const seen = new Set();
  raw.forEach((entry, index) => {
    const where = `actions[${index}]`;
    if (!isPlainObject(entry)) {
      report.error(where, `expected an object describing an action, got ${JSON.stringify(entry)}`);
      return;
    }
    rejectUnknownKeys(report, where, entry, ['id', 'name', 'command', 'wol', 'confirm', 'busyGated', 'timeoutSeconds']);
    const id = reqStr(report, `${where}.id`, entry.id);
    if (!id) {
      report.error(`${where}.id`, 'an action needs an id');
      return;
    }
    if (!ID_PATTERN.test(id)) {
      report.error(
        `${where}.id`,
        `the id ${JSON.stringify(id)} has characters that cannot be passed safely over SSH`,
        'use letters, digits, dot, dash and underscore only',
      );
    }
    if (seen.has(id)) {
      report.error(`${where}.id`, `the action id ${JSON.stringify(id)} is used more than once`);
      return;
    }
    // Exactly one of the two: an action either runs a command or sends a packet,
    // and an action carrying both is a config whose author had two ideas and
    // committed to neither.
    const hasCommand = entry.command !== undefined && entry.command !== null;
    const hasWol = entry.wol !== undefined && entry.wol !== null;
    if (hasCommand && hasWol) {
      report.error(where, 'an action has both "command" and "wol"', 'keep exactly one of them');
      return;
    }
    if (!hasCommand && !hasWol) {
      report.error(where, 'an action needs either "command" (an argv array) or "wol" (a wake-on-LAN target)');
      return;
    }

    const command = hasCommand ? reqArgv(report, `${where}.command`, entry.command, { required: true }) : null;
    const wol = hasWol ? normalizeWol(report, `${where}.wol`, entry.wol) : null;
    if (hasCommand && !command) return;
    if (hasWol && !wol) return;
    seen.add(id);
    actions.push({
      id,
      name: reqStr(report, `${where}.name`, entry.name, id),
      kind: hasWol ? 'wol' : 'command',
      command,
      wol,
      confirm: reqStr(report, `${where}.confirm`, entry.confirm),
      // A wake packet takes milliseconds and disturbs nothing on this machine,
      // so it is not busy-gated unless the config asks for it.
      busyGated: reqBool(report, `${where}.busyGated`, entry.busyGated, false),
      timeoutSeconds: reqPositive(report, `${where}.timeoutSeconds`, entry.timeoutSeconds, 60),
    });
  });
  return actions;
}

// ---------------------------------------------------------------------------
// The whole document
// ---------------------------------------------------------------------------

function normalizeTelemetry(report, raw) {
  if (raw === undefined || raw === null) return { probes: [] };
  if (!isPlainObject(raw)) {
    report.error('telemetry', 'expected an object containing explicitly configured probes');
    return { probes: [] };
  }
  rejectUnknownKeys(report, 'telemetry', raw, ['probes']);
  if (!Array.isArray(raw.probes) || raw.probes.length > 8) {
    report.error('telemetry.probes', 'expected an array of at most eight probes');
    return { probes: [] };
  }
  const seen = new Set();
  const probes = [];
  for (const [index, entry] of raw.probes.entries()) {
    const where = `telemetry.probes[${index}]`;
    if (!isPlainObject(entry)) {
      report.error(where, 'expected a probe object');
      continue;
    }
    rejectUnknownKeys(report, where, entry, ['id', 'name', 'unit', 'command', 'timeoutSeconds']);
    const id = reqStr(report, `${where}.id`, entry.id);
    if (!id || !ID_PATTERN.test(id) || id.length > 64 || seen.has(id)) {
      report.error(`${where}.id`, 'expected a unique id of at most 64 letters, digits, dots, dashes or underscores');
      continue;
    }
    seen.add(id);
    const name = reqStr(report, `${where}.name`, entry.name, id);
    const unit = entry.unit ?? '';
    if (!name || name.length > 120 || /[\u0000-\u001f\u007f]/.test(name)) report.error(`${where}.name`, 'expected a name of at most 120 characters without control characters');
    if (typeof unit !== 'string' || unit.length > 24 || /[\u0000-\u001f\u007f]/.test(unit)) report.error(`${where}.unit`, 'expected a unit of at most 24 characters without control characters');
    const command = reqArgv(report, `${where}.command`, entry.command, { required: true });
    if (Array.isArray(entry.command) && entry.command.some((part) => typeof part !== 'string' || part.length === 0 || part.includes('\0'))) {
      report.error(`${where}.command`, 'every argument must be a non-empty string without NUL characters');
    }
    const timeoutSeconds = entry.timeoutSeconds ?? 1;
    if (typeof timeoutSeconds !== 'number' || !Number.isFinite(timeoutSeconds) || timeoutSeconds <= 0 || timeoutSeconds > 2) {
      report.error(`${where}.timeoutSeconds`, 'expected a number greater than zero and at most two seconds');
    }
    if (command) probes.push({ id, name, unit, command, timeoutSeconds });
  }
  return { probes };
}

const TOP_LEVEL_KEYS = ['configVersion', 'system', 'autoUpdate', 'updates', 'services', 'boot', 'sleep', 'actions', 'telemetry'];
const LEGACY_TOP_LEVEL_KEYS = [
  'port',
  'channel',
  'staleTurnHours',
  'allowScripts',
  'npmPrefix',
  't3AppPath',
  't3AppBundleId',
  'updaterCacheDir',
];

/**
 * Turn whatever was in config.json into the shape the rest of the agent works
 * with. Pure: everything that varies between machines comes in through options,
 * which is what lets the tests exercise all three platforms from one of them.
 *
 * Returns { ok, config, errors, warnings, migrations }. `config` is always
 * present so a caller can report what it managed to understand, but a caller
 * that is about to CHANGE something must refuse unless ok is true.
 */
export function normalizeConfig(raw, options = {}) {
  const report = new Report();
  const platform = options.platform ?? detectPlatform();
  const base = options.base ?? basePath();
  // "There is no file" is a different statement from "the file says nothing",
  // and only the first one produces an inert machine.
  const present = options.present !== false;

  if (raw !== undefined && raw !== null && !isPlainObject(raw)) {
    report.error('', `config.json must contain a JSON object, got ${JSON.stringify(raw)}`);
    return { ok: false, config: null, errors: report.errors, warnings: report.warnings, migrations: report.migrations };
  }
  const source = isPlainObject(raw) ? raw : {};

  const usesLegacyKeys = present && LEGACY_TOP_LEVEL_KEYS.some((key) => source[key] !== undefined);
  rejectUnknownKeys(report, 'config', source, usesLegacyKeys ? [...TOP_LEVEL_KEYS, ...LEGACY_TOP_LEVEL_KEYS] : TOP_LEVEL_KEYS);

  const declaredVersion = source.configVersion;
  if (declaredVersion !== undefined) {
    if (!Number.isInteger(declaredVersion) || declaredVersion < 1) {
      report.error('configVersion', `expected a whole number, got ${JSON.stringify(declaredVersion)}`);
    } else if (declaredVersion > CONFIG_VERSION) {
      report.error(
        'configVersion',
        `this config declares version ${declaredVersion} but the agent understands up to ${CONFIG_VERSION}`,
        'update the agent on this machine before using this config, rather than letting an older agent guess at settings it does not know',
      );
    }
  }

  const system = source.system;
  if (system !== undefined && system !== null && !isPlainObject(system)) {
    report.error('system', `expected an object, got ${JSON.stringify(system)}`);
  }
  const systemBlock = isPlainObject(system) ? system : {};
  rejectUnknownKeys(report, 'system', systemBlock, ['id', 'name']);

  // `updates` is the block; `autoUpdate` is the 2.x spelling of updates.automatic
  // and stays readable and writable. When both are present they have to agree,
  // because silently preferring one of them is how a machine ends up updating
  // itself against a setting somebody thought they had turned off.
  const updates = normalizeUpdates(report, 'updates', source.updates, { perService: false });
  const legacyAutoUpdate = reqBool(report, 'autoUpdate', source.autoUpdate, null);
  if (legacyAutoUpdate !== null) {
    if (source.updates === undefined || source.updates === null) {
      updates.automatic = legacyAutoUpdate;
    } else if (isPlainObject(source.updates) && source.updates.automatic !== undefined && source.updates.automatic !== legacyAutoUpdate) {
      report.error(
        'autoUpdate',
        `autoUpdate is ${legacyAutoUpdate} but updates.automatic is ${source.updates.automatic}, and they are the same setting`,
        'keep updates.automatic and delete autoUpdate',
      );
    } else if (isPlainObject(source.updates) && source.updates.automatic === undefined) {
      updates.automatic = legacyAutoUpdate;
    }
  }

  // An unconfigured machine is inert: nothing to look after, and no schedule.
  if (!present) updates.automatic = false;

  const config = {
    configVersion: Number.isInteger(declaredVersion) ? declaredVersion : null,
    contract: CONTRACT_VERSION,
    agentVersion: AGENT_VERSION,
    system: {
      id: reqStr(report, 'system.id', systemBlock.id, platform ?? process.platform),
      name: reqStr(report, 'system.name', systemBlock.name, PLATFORM_SYSTEM_NAMES[platform] ?? (platform ?? process.platform)),
    },
    updates,
    // Kept in the parsed config so 2.x reply shapes can carry it unchanged.
    autoUpdate: updates.automatic === true,
    services: [],
    // Only a config that still carries the pre-"services" top-level keys gets
    // the old platform boot targets invented for it. A modern config that simply
    // has no boot block has no boot targets, because guessing that a machine can
    // reboot into another operating system is not a safe default.
    boot: normalizeBoot(report, source.boot, { platform, legacy: usesLegacyKeys }),
    sleep: normalizeSleep(report, source.sleep),
    actions: normalizeActions(report, source.actions),
    telemetry: normalizeTelemetry(report, source.telemetry),
    legacyServices: false,
    inert: !present,
  };

  if (source.services !== undefined && source.services !== null) {
    if (!Array.isArray(source.services)) {
      report.error('services', `expected an array of services, got ${JSON.stringify(source.services)}`);
    } else {
      const seen = new Set();
      source.services.forEach((entry, index) => {
        const service = normalizeService(report, entry, index, { platform, base });
        if (!service) return;
        if (seen.has(service.id)) {
          report.error(`services[${index}].id`, `the service id ${JSON.stringify(service.id)} is used more than once`);
          return;
        }
        seen.add(service.id);
        config.services.push(service);
      });
      for (const [index, service] of config.services.entries()) {
        for (const dependency of service.updates.after) {
          if (!seen.has(dependency)) {
            report.error(
              `services[${index}].updates.after`,
              `${service.id} is ordered after ${JSON.stringify(dependency)}, which is not a service on this machine`,
            );
          }
        }
      }
    }
  } else if (usesLegacyKeys) {
    config.services = synthesizeLegacyServices(report, source, { platform, base });
    config.legacyServices = true;
    report.migrate(
      'services',
      'this config predates the "services" list, so the T3 Code service was rebuilt from the old top-level keys',
      'move the settings into a "services" array; the old keys keep working until you do',
    );
  } else if (present) {
    report.warn(
      'services',
      'this machine looks after no services',
      'the agent can still boot, sleep and run actions; add a "services" array to have it look after something',
    );
  }

  return { ok: report.ok, config, errors: report.errors, warnings: report.warnings, migrations: report.migrations };
}

// ---------------------------------------------------------------------------
// Reading and writing
// ---------------------------------------------------------------------------

function ensureBaseDir() {
  try {
    fs.mkdirSync(basePath(), { recursive: true });
    return true;
  } catch {
    return false;
  }
}

/**
 * Read a JSON document, telling the cases apart.
 *
 * Collapsing "there is no file", "the file cannot be read" and "the file is not
 * JSON" into one empty object is the specific bug this replaces: a truncated
 * config used to load as the full set of enabled defaults.
 */
export function readJsonDocument(file) {
  let raw;
  try {
    raw = fs.readFileSync(file, 'utf8');
  } catch (err) {
    if (err.code === 'ENOENT') return { state: 'missing', value: null, error: null };
    return { state: 'unreadable', value: null, error: `${file} could not be read: ${err.message}` };
  }
  if (raw.trim().length === 0) return { state: 'empty', value: null, error: `${file} is empty`, raw };
  try {
    const parsed = JSON.parse(raw);
    return { state: 'ok', value: parsed, error: null, raw };
  } catch (err) {
    return { state: 'malformed', value: null, error: `${file} does not parse as JSON: ${err.message}`, raw };
  }
}

/** Keep a copy of the last config that loaded cleanly, for doctor to compare against. */
function rememberLastGood(raw) {
  if (typeof raw !== 'string') return;
  try {
    const file = lastGoodConfigPath();
    if (fs.existsSync(file) && fs.readFileSync(file, 'utf8') === raw) return;
    ensureBaseDir();
    const tmp = `${file}.tmp-${process.pid}`;
    fs.writeFileSync(tmp, raw, 'utf8');
    fs.renameSync(tmp, file);
  } catch {
    /* the last-good copy is a convenience, never a requirement */
  }
}

/** When the last-good copy was written, or null when there is none. */
export function lastGoodInfo() {
  try {
    const stat = fs.statSync(lastGoodConfigPath());
    return { path: lastGoodConfigPath(), at: new Date(stat.mtimeMs).toISOString() };
  } catch {
    return null;
  }
}

/**
 * config.json, normalized, with everything that is wrong with it.
 *
 * Returns { ok, config, source, problems, errors, warnings, migrations }.
 *
 *   source "defaults"        no config.json; the machine is inert.
 *   source "file"            config.json was read.
 *   source "last-known-good" the file is broken and an earlier good copy exists;
 *                            it is REPORTED, not applied. ok stays false and
 *                            every mutation is refused, because quietly running
 *                            yesterday's policy is not what the owner asked for
 *                            either.
 */
export function loadConfig(options = {}) {
  const file = options.path ?? configPath();
  // `status`, `doctor`, `op`, `history` and `logs` promise to change nothing on
  // the machine. Keeping the last-good copy up to date is maintenance, and
  // maintenance belongs to the commands that were asked to change something.
  const readOnly = options.readOnly === true;
  const document = readJsonDocument(file);

  if (document.state === 'missing') {
    const result = normalizeConfig({}, { ...options, present: false });
    return {
      ...result,
      source: 'defaults',
      path: file,
      error: null,
      problems: result.errors,
      warnings: [
        ...result.warnings,
        {
          level: 'warning',
          path: 'config.json',
          message: `there is no ${file}, so this machine looks after nothing and automatic updates are off`,
          fix: 'write a config.json to tell the agent what to look after; "legionctl doctor" lists what it would check',
        },
      ],
    };
  }

  if (document.state !== 'ok') {
    const lastGood = lastGoodInfo();
    const problem = {
      level: 'error',
      path: 'config.json',
      message: document.error,
      fix: lastGood
        ? `the last copy that loaded cleanly is at ${lastGood.path} (${lastGood.at}); compare it with "legionctl doctor" and copy it back if it is still right`
        : 'fix or remove the file; the agent will not guess at settings, and it will not fall back to defaults that might enable automatic updates you had turned off',
    };
    return {
      ok: false,
      config: null,
      source: lastGood ? 'last-known-good' : 'defaults',
      path: file,
      lastGood,
      errors: [problem],
      problems: [problem],
      warnings: [],
      migrations: [],
      error: document.error,
    };
  }

  const result = normalizeConfig(document.value, { ...options, present: true });
  if (result.ok && !readOnly) rememberLastGood(document.raw);
  return {
    ...result,
    source: 'file',
    path: file,
    problems: result.errors,
    error: result.ok ? null : `${file} has ${result.errors.length} problem${result.errors.length === 1 ? '' : 's'}`,
  };
}

/**
 * Merge a patch into config.json and return the normalized result.
 *
 * Refuses outright when the stored document cannot be read. Writing a patch on
 * top of a file we could not parse would rewrite it as the patch plus nothing,
 * silently discarding whatever the owner had configured.
 *
 * Only the keys that were already in the file plus the patch are written back.
 * Stamping the full set of defaults would freeze today's defaults into every
 * config the first time anyone toggled a switch.
 */
export function saveConfig(patch, { waitMs = 10000 } = {}) {
  // A unique temporary filename only makes the final rename atomic. The read,
  // patch and validation must share the same lock, or two acknowledged changes
  // can each replace the other's snapshot. Callers that hold the operation lock
  // take this state mutex inside it; this function never acquires an outer lock.
  let lock;
  try {
    lock = acquireFileLock(stateLockPath(), { waitMs, purpose: 'config' });
    return saveConfigLocked(patch);
  } catch (error) {
    return { ok: false, config: null, error: `${error?.message ?? String(error)}; no configuration change was saved` };
  } finally {
    lock?.release();
  }
}

function saveConfigLocked(patch) {
  const file = configPath();
  const document = readJsonDocument(file);
  if (document.state !== 'ok' && document.state !== 'missing') {
    return { ok: false, config: null, error: `${document.error}; refusing to overwrite it with a partial document` };
  }
  const stored = document.state === 'ok' ? document.value : {};
  if (!isPlainObject(stored)) {
    return { ok: false, config: null, error: `${file} is not a JSON object; refusing to overwrite it` };
  }
  const merged = typeof patch === 'function' ? patch(stored) : { ...stored, ...patch };
  const normalized = normalizeConfig(merged, { present: true });
  if (!normalized.ok) {
    return {
      ok: false,
      config: normalized.config,
      errors: normalized.errors,
      error: 'the change would leave config.json in a state this agent cannot use, so nothing was written',
    };
  }
  const written = writeJsonAtomic(file, merged);
  if (!written.ok) return { ok: false, config: normalized.config, error: written.error };
  rememberLastGood(`${JSON.stringify(merged, null, 2)}\n`);
  return { ok: true, config: normalized.config, error: null };
}

// ---------------------------------------------------------------------------
// Running things
// ---------------------------------------------------------------------------

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

/**
 * The same runner, without blocking the event loop.
 *
 * spawnSync is the right tool almost everywhere in the agent: the work is
 * sequential and the code reads better for it. It is the wrong tool for status,
 * where several independent probes each have their own timeout and running them
 * one after another made a healthy machine look unreachable — two probes with a
 * sixteen second budget added up to a thirty-two second reply, past both clients'
 * command budgets. Anything that has to fit inside a shared deadline uses this.
 */
const activeCommandGroups = new Set();
let commandExitCleanupInstalled = false;

export function runCommandAsync(file, args = [], options = {}) {
  return new Promise((resolve) => {
    const timeoutMs = options.timeoutMs ?? 30000;
    const maxBuffer = options.maxBuffer ?? 16 * 1024 * 1024;
    const full = [file, ...args].join(' ');
    const command = full.length > 160 ? `${full.slice(0, 160)}...` : full;

    let child;
    try {
      child = spawn(file, args, {
        windowsHide: true,
        // A timeout must terminate descendants too, including a child whose
        // parent exited while it still holds one of our output pipes open.
        detached: process.platform !== 'win32',
        cwd: options.cwd,
        env: options.env ?? process.env,
        shell: options.shell === true,
      });
    } catch (err) {
      resolve({
        ok: false,
        code: null,
        signal: null,
        stdout: '',
        stderr: '',
        timedOut: false,
        error: String(err.message || err),
        command,
      });
      return;
    }

    let stdout = '';
    let stderr = '';
    let timedOut = false;
    let settled = false;
    // Internal disposable workers can release parent-owned scratch only after
    // Windows confirms the process has released its native file handles.
    if (typeof options.onExit === 'function') child.once('exit', options.onExit);
    const closePipes = () => {
      child.stdin?.destroy();
      child.stdout?.destroy();
      child.stderr?.destroy();
    };
    const killPosixGroup = () => {
      if (!child.pid) return false;
      try { process.kill(-child.pid, 'SIGKILL'); return true; } catch (error) {
        if (error.code === 'ESRCH') return true;
        try { child.kill('SIGKILL'); } catch { /* cancellation could not be confirmed */ }
        return false;
      }
    };
    if (process.platform !== 'win32' && child.pid) {
      activeCommandGroups.add(child.pid);
      if (!commandExitCleanupInstalled) {
        commandExitCleanupInstalled = true;
        process.once('exit', () => {
          for (const pid of activeCommandGroups) {
            try { process.kill(-pid, 'SIGKILL'); } catch { /* the group may already be gone */ }
          }
        });
      }
    }
    const finish = (value) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      activeCommandGroups.delete(child.pid);
      resolve(value);
    };

    const timer = setTimeout(() => {
      timedOut = true;
      let cancellationRequested = false;
      const timeoutResult = (terminationConfirmed = false) => ({
        ok: false, code: null, signal: null, stdout, stderr, timedOut: true, terminationConfirmed,
        error: terminationConfirmed
          ? 'command deadline expired; the process exited and the command outcome is unknown'
          : cancellationRequested
            ? 'command deadline expired; process-tree termination was requested and the command outcome is unknown'
            : 'command deadline expired; process-tree termination could not be confirmed and the command outcome is unknown',
        command,
      });
      if (process.platform !== 'win32') {
        cancellationRequested = killPosixGroup();
      } else if (child.pid && options.terminateTree === false) {
        // Disposable workers that cannot spawn children need no taskkill tree
        // traversal. Terminate their owned handle directly, then await the exit
        // notification: Windows keeps SQLite files locked until that happens.
        // Only trusted internal workers may opt out of process-tree cleanup.
        closePipes();
        let terminationWait;
        const exited = () => {
          clearTimeout(terminationWait);
          child.unref();
          finish(timeoutResult(true));
        };
        child.once('exit', exited);
        try { cancellationRequested = child.kill('SIGKILL'); } catch { /* return an unconfirmed timeout */ }
        if (child.exitCode !== null || child.signalCode !== null) exited();
        else terminationWait = setTimeout(() => {
          child.removeListener('exit', exited);
          child.unref();
          finish(timeoutResult(false));
        }, 500);
        return;
      } else if (child.pid) {
        // Windows has no POSIX process group signal. Ask its tree-aware tool to
        // terminate the command and descendants, without waiting on its pipes.
        // The result below remains unknown even if taskkill later succeeds.
        try {
          const killer = spawn('taskkill.exe', ['/PID', String(child.pid), '/T', '/F'], { windowsHide: true, stdio: 'ignore' });
          // Do not abort traversal after an arbitrary 250 ms: on a loaded
          // machine that can leave the command alive. The cleanup helper has
          // no inherited pipes and may finish independently of this deadline.
          killer.once('error', () => {});
          killer.unref();
          cancellationRequested = true;
        } catch { /* report unconfirmed cancellation below */ }
      }
      // Resolving only from `close` is unsafe: grandchildren can keep these
      // pipes open indefinitely, even after their immediate parent is killed.
      closePipes();
      child.unref();
      finish(timeoutResult());
    }, timeoutMs);

    const collect = (stream, append) => {
      if (!stream) return;
      stream.setEncoding('utf8');
      stream.on('data', (chunk) => append(chunk));
      stream.on('error', () => {
        /* a closed pipe is not an outcome the caller can act on */
      });
    };
    collect(child.stdout, (chunk) => {
      if (stdout.length < maxBuffer) stdout += chunk.slice(0, maxBuffer - stdout.length);
    });
    collect(child.stderr, (chunk) => {
      if (stderr.length < maxBuffer) stderr += chunk.slice(0, maxBuffer - stderr.length);
    });

    child.on('error', (err) => {
      closePipes();
      finish({ ok: false, code: null, signal: null, stdout, stderr, timedOut, error: String(err.message || err), command });
    });
    child.on('close', (code, signal) => {
      if (timedOut) return;
      finish({
        ok: code === 0 && !timedOut,
        code: timedOut ? null : code,
        signal: signal ?? null,
        stdout,
        stderr,
        timedOut,
        error: null,
        command,
      });
    });
  });
}

export function runArgvAsync(command, options = {}) {
  return runCommandAsync(command[0], command.slice(1), options);
}

/**
 * Run tasks with a ceiling on how many are in flight at once.
 *
 * Unbounded parallelism across a dozen services would start a dozen PowerShell
 * processes on a machine that is already busy, which is its own kind of
 * disruption. Four at a time keeps a wide machine inside its status budget
 * without becoming the reason it is slow.
 */
export async function mapWithLimit(items, limit, worker) {
  const results = new Array(items.length);
  let next = 0;
  const runners = new Array(Math.max(1, Math.min(limit, items.length))).fill(null).map(async () => {
    for (;;) {
      const index = next;
      next += 1;
      if (index >= items.length) return;
      results[index] = await worker(items[index], index);
    }
  });
  await Promise.all(runners);
  return results;
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

/**
 * A deadline that several probes can share. Everything that runs under a status
 * budget asks this how much time is left rather than assuming it has its own
 * full allowance, which is what stopped two sixteen-second probes from adding up
 * to a thirty-two-second status reply.
 */
export function deadline(totalMs) {
  const at = Date.now() + totalMs;
  return {
    at,
    remaining() {
      return Math.max(0, at - Date.now());
    },
    expired() {
      return Date.now() >= at;
    },
    /** At most `want` ms, and never more than is left. */
    slice(want) {
      return Math.max(0, Math.min(want, at - Date.now()));
    },
  };
}
