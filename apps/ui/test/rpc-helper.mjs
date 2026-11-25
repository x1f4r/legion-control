#!/usr/bin/env node
// Starts `legion rpc` (or `legion stream` when the trailing argument is swapped by legion-net)
// for a second local test node under its own LEGION_HOME and HOME.
import { spawn } from 'node:child_process';

const [legionBin, legionHome, userHome, subcommand = 'rpc'] = process.argv.slice(2);
if (!legionBin || !legionHome || !userHome || (subcommand !== 'rpc' && subcommand !== 'stream')) {
  process.stderr.write(
    'usage: rpc-helper.mjs <legion-bin> <legion-home> <user-home> [rpc|stream]\n',
  );
  process.exit(2);
}

const env = { ...process.env, LEGION_HOME: legionHome, HOME: userHome, USERPROFILE: userHome };
delete env.XDG_CONFIG_HOME;
delete env.XDG_STATE_HOME;

const child = spawn(legionBin, [subcommand], { stdio: 'inherit', env });
child.on('exit', (code, signal) => {
  if (signal) process.kill(process.pid, signal);
  process.exit(code ?? 1);
});
