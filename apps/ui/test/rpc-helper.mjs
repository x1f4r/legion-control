#!/usr/bin/env node
// Starts `legion rpc` for a second local test node under its own LEGION_HOME and HOME.
import { spawn } from 'node:child_process';

const [legionBin, legionHome, userHome] = process.argv.slice(2);
if (!legionBin || !legionHome || !userHome) {
  process.stderr.write('usage: rpc-helper.mjs <legion-bin> <legion-home> <user-home>\n');
  process.exit(2);
}

const env = { ...process.env, LEGION_HOME: legionHome, HOME: userHome, USERPROFILE: userHome };
delete env.XDG_CONFIG_HOME;
delete env.XDG_STATE_HOME;

const child = spawn(legionBin, ['rpc'], { stdio: 'inherit', env });
child.on('exit', (code, signal) => {
  if (signal) process.kill(process.pid, signal);
  process.exit(code ?? 1);
});
