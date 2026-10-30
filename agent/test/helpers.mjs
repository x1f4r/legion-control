// Shared scaffolding for the agent tests.
//
// Every test runs against its OWN base directory, created in the system temp
// area and thrown away afterwards. Nothing here ever reads or writes the real
// ~/.legion-control, which is the whole point: these tests exercise recovery,
// interrupted operations and broken configuration, and doing that to a machine
// somebody depends on would be the exact damage the agent exists to prevent.

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { spawnSync } from 'node:child_process';

export const SRC = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', 'src');
export const ENTRY = path.join(SRC, 'index.mjs');
export const REPO = path.resolve(SRC, '..', '..');
/** The tiny controllable service the CLI tests drive. */
export const FIXTURE = path.join(REPO, 'agent', 'test', 'fixture-service.mjs');
/** The version agent/package.json states. It is written nowhere else, so the tests read it from there too. */
export const AGENT_VERSION = JSON.parse(fs.readFileSync(path.join(SRC, '..', 'package.json'), 'utf8')).version;

/**
 * An isolated base directory, with LEGIONCTL_HOME pointed at it for the
 * duration of `body`. Restored afterwards even when the body throws, so one
 * failing test cannot leak its home into the next.
 */
export async function withHome(body) {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'legionctl-test-'));
  const previous = process.env.LEGIONCTL_HOME;
  process.env.LEGIONCTL_HOME = home;
  try {
    return await body(home);
  } finally {
    if (previous === undefined) delete process.env.LEGIONCTL_HOME;
    else process.env.LEGIONCTL_HOME = previous;
    fs.rmSync(home, { recursive: true, force: true, maxRetries: 5, retryDelay: 50 });
  }
}

/** Import a source module with the module cache bypassed, so state is not shared. */
export async function fresh(relative) {
  return import(`${pathToFileURL(path.join(SRC, relative)).href}?t=${Date.now()}-${Math.random()}`);
}

/**
 * Run the CLI once and parse its single JSON object. `nodeArgs` go to Node
 * itself, in front of the entry point, for a test that loads stand-ins.
 */
export function cli(args, { home, stdin = null, env = {}, nodeArgs = [] } = {}) {
  const result = spawnSync(process.execPath, [...nodeArgs, ENTRY, ...args], {
    encoding: 'utf8',
    input: stdin ?? undefined,
    env: { ...process.env, LEGIONCTL_HOME: home, ...env },
    timeout: 120000,
  });
  let payload = null;
  try {
    payload = JSON.parse(result.stdout);
  } catch {
    payload = null;
  }
  return { payload, exitCode: result.status, stdout: result.stdout, stderr: result.stderr };
}

/** A working directory for the fixture service, plus its initial files. */
export function fixtureData(home, initial = {}) {
  const dir = path.join(home, 'fixture');
  fs.mkdirSync(dir, { recursive: true });
  const defaults = { installed: '1.0.0', latest: '1.0.0', running: 'on' };
  for (const [name, value] of Object.entries({ ...defaults, ...initial })) {
    fs.writeFileSync(path.join(dir, name), String(value));
  }
  return dir;
}

export function readFixture(dir, name, fallback = '') {
  try {
    return fs.readFileSync(path.join(dir, name), 'utf8').trim();
  } catch {
    return fallback;
  }
}

export function writeFixture(dir, name, value) {
  fs.writeFileSync(path.join(dir, name), String(value));
}

/** A command-kind service wired to the fixture, with whatever overrides a test needs. */
export function commandService(dir, overrides = {}) {
  const call = (verb) => [process.execPath, FIXTURE, verb, dir];
  return {
    id: 'demo',
    name: 'Demo',
    kind: 'command',
    installedVersion: call('installed'),
    latestVersion: call('latest'),
    update: call('update'),
    rollback: call('rollback'),
    process: { type: 'command', running: call('running'), start: call('start'), stop: call('stop') },
    health: { type: 'command', command: call('health') },
    busy: { type: 'command', command: call('busy') },
    ...overrides,
  };
}

/** The stand-in for npm's own entry point, and the module that makes the agent use it. */
export const FAKE_NPM = path.join(REPO, 'agent', 'test', 'fake-npm-cli.mjs');
const HIDE_NODE_NPM = path.join(REPO, 'agent', 'test', 'hide-node-npm.mjs');

/** Where a global prefix keeps its packages, as npm lays it out on this platform. */
function modulesOf(prefix) {
  return process.platform === 'win32' ? path.join(prefix, 'node_modules') : path.join(prefix, 'lib', 'node_modules');
}

/**
 * A global npm prefix holding fake-npm-cli.mjs as its npm, and `installed` of
 * the package already on disk. `nodeArgs` go to every agent run that should
 * reach this npm and no other (hide-node-npm.mjs says how).
 */
export function fakeNpm(home, dir, { pkg = 'demo-pkg', installed = '1.0.0' } = {}) {
  const prefix = path.join(home, 'npm-prefix');
  const bin = path.join(modulesOf(prefix), 'npm', 'bin');
  fs.mkdirSync(bin, { recursive: true });
  fs.writeFileSync(path.join(bin, 'npm-cli.js'), [
    `process.argv.splice(2, 0, ${JSON.stringify(dir)}, ${JSON.stringify(prefix)});`,
    `import(${JSON.stringify(pathToFileURL(FAKE_NPM).href)});`,
    '',
  ].join('\n'));
  if (installed !== null) writeNpmPackage(prefix, pkg, installed);
  return { prefix, pkg, nodeArgs: ['--import', pathToFileURL(HIDE_NODE_NPM).href] };
}

/** Put a version of the package on disk under the prefix, as a finished install leaves it. */
export function writeNpmPackage(prefix, pkg, version) {
  const root = path.join(modulesOf(prefix), pkg);
  fs.mkdirSync(root, { recursive: true });
  fs.writeFileSync(path.join(root, 'package.json'), `${JSON.stringify({ name: pkg, version })}\n`);
}

/** The version of the package on disk under the prefix, or null when there is none. */
export function npmPackageVersion(prefix, pkg) {
  try {
    return JSON.parse(fs.readFileSync(path.join(modulesOf(prefix), pkg, 'package.json'), 'utf8')).version ?? null;
  } catch {
    return null;
  }
}

/** Every npm call the fake was asked, one argv string each. */
export function npmCalls(dir) {
  return readFixture(dir, 'npm-calls').split('\n').filter(Boolean);
}

/** An npm-kind service installed by the fake npm, with the fixture's process, health and busy. */
export function npmService(dir, npm, overrides = {}) {
  const call = (verb) => [process.execPath, FIXTURE, verb, dir];
  return {
    id: 'demo',
    name: 'Demo',
    kind: 'npm',
    package: npm.pkg,
    npmPrefix: npm.prefix,
    process: { type: 'command', running: call('running'), start: call('start'), stop: call('stop') },
    health: { type: 'command', command: call('health') },
    busy: { type: 'command', command: call('busy') },
    ...overrides,
  };
}

/** A whole config document around one fixture service. */
export function fixtureConfig(dir, overrides = {}) {
  return {
    configVersion: 3,
    system: { id: 'test', name: 'Test machine' },
    updates: { automatic: true, maintenanceWindows: [] },
    services: [commandService(dir)],
    boot: { targets: {} },
    ...overrides,
  };
}

export function writeConfig(home, document) {
  fs.writeFileSync(path.join(home, 'config.json'), `${JSON.stringify(document, null, 2)}\n`);
}

/** A pid that is certainly not running, for the lock and recovery tests. */
export function deadPid() {
  // Spawn something trivial, wait for it, and reuse its pid. A pid picked out of
  // the air might belong to a live process and make the test lie.
  const result = spawnSync(process.execPath, ['-e', 'process.exit(0)']);
  return result.pid;
}
