// What `version --check` loads: the self-test a staged agent has to pass
// before it replaces a working one.
//
// Between them, the lists below name every module of the agent but index.mjs,
// which runs the self-test and so has loaded by the time it starts. A module
// left off them could go missing from a staged tree, or break in it, and the
// tree would still pass. The list used to be written out in index.mjs by hand
// and missed every module added since anybody last looked at it, so
// agent/test/selftest-modules.test.mjs now holds it to the source tree.

import { spawnSync } from 'node:child_process';

/**
 * Application modules, imported for real. Importing one runs nothing but its
 * own definitions, so a verified bundle that cannot start is caught while the
 * working agent is still in place.
 */
export const SELF_TEST_MODULES = [
  'actions.mjs', 'agent-swap.mjs', 'archive.mjs', 'args.mjs', 'boot.mjs', 'config-schema.mjs', 'config-store.mjs', 'config.mjs',
  'contract.mjs', 'controller.mjs', 'detached.mjs', 'dispatch.mjs', 'doctor.mjs', 'endpoint.mjs', 'floor.mjs', 'http.mjs',
  'launch.mjs', 'lock.mjs', 'log.mjs', 'mutex.mjs', 'operate.mjs', 'operations.mjs', 'paths.mjs', 'policy.mjs', 'power.mjs',
  'probes/busy.mjs', 'probes/health.mjs', 'probes/process.mjs', 'probes/relay.mjs', 'probes/t3-sqlite.mjs', 'probes/telemetry.mjs',
  'process-runner.mjs', 'providers/app.mjs', 'providers/command.mjs', 'providers/npm.mjs', 'redact.mjs', 'scheduler.mjs',
  'selftest-modules.mjs', 'selfupdate.mjs', 'service-config.mjs', 'service-profile-claude.mjs', 'service-profile-desktop.mjs',
  'service-profiles.mjs', 'service.mjs', 'setup-document.mjs', 'state.mjs', 'status-probes.mjs', 'status.mjs', 'trust.mjs',
  'update.mjs',
];

/** Child-process entry points, parsed with everything they import but never run. */
export const WORKER_ENTRIES = ['status-probe-worker.mjs', 'service-profile-probe.mjs'];

/**
 * What the workers import only when they run. A walk of static imports cannot
 * see a dynamic one, so each is a root of the walk as well.
 */
export const WORKER_IMPORTS = ['operations.mjs', 'probes/t3-sqlite.mjs'];

/** Import every application module. `{ loaded, failed }`, failed as "<module>: <why>". */
export async function importModules() {
  const failed = [];
  let loaded = 0;
  for (const name of SELF_TEST_MODULES) {
    try {
      await import(new URL(name, import.meta.url).href);
      loaded += 1;
    } catch (err) {
      failed.push(`${name}: ${err.message}`);
    }
  }
  return { loaded, failed };
}

// Parse child-process entry points without importing them: importing a worker
// can run probes, print a reply, or interpret the self-test's own argv.
const CHECK_WORKERS = `
import fs from 'node:fs';
import path from 'node:path';
import { isBuiltin } from 'node:module';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { SourceTextModule } from 'node:vm';
const base = path.resolve(process.argv[1]);
const roots = JSON.parse(process.argv[2]);
const checked = new Set();
function inspect(file) {
  if (checked.has(file)) return;
  if (!file.startsWith(base + path.sep)) throw new Error('dependency leaves the agent source tree');
  if (checked.size >= 256) throw new Error('module dependency count exceeds 256');
  const stat = fs.lstatSync(file);
  if (!stat.isFile() || stat.size > 8 * 1024 * 1024) throw new Error('module is not a bounded regular file: ' + file);
  const url = pathToFileURL(file);
  const module = new SourceTextModule(fs.readFileSync(file, 'utf8'), { identifier: url.href });
  checked.add(file);
  for (const specifier of module.dependencySpecifiers) {
    if (isBuiltin(specifier)) continue;
    if (!specifier.startsWith('./') && !specifier.startsWith('../')) throw new Error('unsupported module dependency: ' + specifier);
    inspect(fileURLToPath(new URL(specifier, url)));
  }
}
// Dynamic worker imports are explicit roots; static imports are walked recursively.
for (const name of roots) {
  try { inspect(path.join(base, name)); }
  catch (error) { throw new Error(name + ': ' + error.message); }
}
process.stdout.write(JSON.stringify({ ok: true, modules: checked.size }));
`;

export function checkWorkerModules(directory) {
  const roots = JSON.stringify([...WORKER_ENTRIES, ...WORKER_IMPORTS]);
  const result = spawnSync(process.execPath, ['--no-warnings', '--experimental-vm-modules', '--input-type=module', '--eval', CHECK_WORKERS, directory, roots], {
    encoding: 'utf8', timeout: 10_000, maxBuffer: 64 * 1024, windowsHide: true,
  });
  if (result.error || result.status !== 0) {
    return { ok: false, error: String(result.error?.message || result.stderr || `worker module check exited ${result.status}`).trim() };
  }
  try {
    const reply = JSON.parse(result.stdout);
    if (reply.ok === true && Number.isInteger(reply.modules)) return reply;
  } catch { /* malformed checker output fails the staged self-test */ }
  return { ok: false, error: 'worker module checker returned an invalid result' };
}
