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

/** Run the CLI once and parse its single JSON object. */
export function cli(args, { home, stdin = null, env = {} } = {}) {
  const result = spawnSync(process.execPath, [ENTRY, ...args], {
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
