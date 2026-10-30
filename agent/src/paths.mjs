// Where this agent and its files are: which OS it is running on, whose home
// it belongs to, where its base directory and every file under it live, and
// which agent it is, by the tree it was loaded from.
//
// Nothing in here may throw at import time, and nothing in here imports
// another module of the agent. index.mjs needs to be able to load the whole
// agent on an unsupported platform and print a clean JSON error, and every
// other module asks this one where things are, the log and the locks included.

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

/** The running agent's own tree: the directory above src/, holding package.json and, in a release, the signed manifest. */
export const AGENT_TREE = path.resolve(fileURLToPath(new URL('..', import.meta.url)));

// agent/package.json is the one place the agent's version is written. The
// release tooling, the signed manifest and every contract fixture read it from
// there, and so does the agent, rather than keep a copy that a release can
// forget to bump. Reading it must not throw here, so a tree whose package.json
// cannot be read reports 0.0.0: no signed manifest names that version, and
// `version --check` fails on the problem, so such a tree cannot pass the
// self-test that every staged agent has to pass before it replaces one.
function readAgentMetadata() {
  try {
    const parsed = JSON.parse(fs.readFileSync(path.join(AGENT_TREE, 'package.json'), 'utf8'));
    if (typeof parsed?.version === 'string' && /^[0-9]+\.[0-9]+\.[0-9]+$/.test(parsed.version)) return { version: parsed.version, problem: null };
    return { version: null, problem: `${path.join(AGENT_TREE, 'package.json')} has no x.y.z version` };
  } catch (error) {
    return { version: null, problem: `${path.join(AGENT_TREE, 'package.json')} cannot be read: ${error.message}` };
  }
}

const AGENT_METADATA = readAgentMetadata();
export const AGENT_VERSION = AGENT_METADATA.version ?? '0.0.0';
/** Why the running tree's package.json gave no version, or null. `version --check` fails on it. */
export const AGENT_METADATA_PROBLEM = AGENT_METADATA.problem;

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
