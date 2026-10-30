// Loaded with --import ahead of the agent: a machine on which a detached
// worker cannot be started, so a --detach request falls back to doing the work
// in the session that asked for it. Only the worker's launch fails; every
// other command the agent runs starts as it always does.
import { createRequire, syncBuiltinESMExports } from 'node:module';

const require = createRequire(import.meta.url);
const childProcess = require('node:child_process');
const { spawn, spawnSync } = childProcess;

childProcess.spawn = function noWorkerSpawn(file, args, ...rest) {
  if (Array.isArray(args) && args.includes('op-run')) return spawn.call(childProcess, `${file}-that-is-not-there`, args, ...rest);
  return spawn.call(childProcess, file, args, ...rest);
};

// Windows starts the worker through CIM, from PowerShell.
childProcess.spawnSync = function noWorkerSpawnSync(file, args, ...rest) {
  if (/powershell/i.test(String(file)) && Array.isArray(args) && args.some((arg) => String(arg).includes('Win32_Process -MethodName Create'))) {
    return { status: 1, signal: null, stdout: '', stderr: 'the test refused to start a worker', pid: 0, output: [] };
  }
  return spawnSync.call(childProcess, file, args, ...rest);
};

syncBuiltinESMExports();
