import { spawnSync } from 'node:child_process';

// Parse child-process entry points without importing them: importing a worker
// can run probes, print a reply, or interpret the self-test's own argv.
const CHECK_WORKERS = `
import fs from 'node:fs';
import path from 'node:path';
import { isBuiltin } from 'node:module';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { SourceTextModule } from 'node:vm';
const base = path.resolve(process.argv[1]);
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
for (const name of ['status-probe-worker.mjs', 'service-profile-probe.mjs', 'operations.mjs', 'probes/t3-sqlite.mjs']) {
  try { inspect(path.join(base, name)); }
  catch (error) { throw new Error(name + ': ' + error.message); }
}
process.stdout.write(JSON.stringify({ ok: true, modules: checked.size }));
`;

export function checkWorkerModules(directory) {
  const result = spawnSync(process.execPath, ['--no-warnings', '--experimental-vm-modules', '--input-type=module', '--eval', CHECK_WORKERS, directory], {
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
