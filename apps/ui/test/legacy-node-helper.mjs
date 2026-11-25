#!/usr/bin/env node
// Runs an older Legion node for browser compatibility tests.
// When LEGION_LEGACY_BIN is set, spawns that real binary for both `rpc` and `stream`.
// Otherwise runs the current binary for `rpc` and reproduces pre-content-streaming `origin/main`
// `legion stream` behavior (`{"protocol": 1}` followed by `{"size": N}` without `length`/`mtime`).
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import readline from 'node:readline';

const [legionBin, legionHome, userHome, subcommand = 'rpc'] = process.argv.slice(2);
if (!legionBin || !legionHome || !userHome || (subcommand !== 'rpc' && subcommand !== 'stream')) {
  process.stderr.write(
    'usage: legacy-node-helper.mjs <legion-bin> <legion-home> <user-home> [rpc|stream]\n',
  );
  process.exit(2);
}

const env = { ...process.env, LEGION_HOME: legionHome, HOME: userHome, USERPROFILE: userHome };
delete env.XDG_CONFIG_HOME;
delete env.XDG_STATE_HOME;

const legacyBin = process.env.LEGION_LEGACY_BIN;
if (legacyBin && fs.existsSync(legacyBin)) {
  const child = spawn(legacyBin, [subcommand], { stdio: 'inherit', env });
  child.on('exit', (code, signal) => {
    if (signal) process.kill(process.pid, signal);
    process.exit(code ?? 1);
  });
} else if (subcommand === 'rpc') {
  const child = spawn(legionBin, ['rpc'], { stdio: 'inherit', env });
  child.on('exit', (code, signal) => {
    if (signal) process.kill(process.pid, signal);
    process.exit(code ?? 1);
  });
} else {
  // Simulate origin/main's `legion stream` protocol 1 `get` reply: `{"size": N}` without
  // `length` or `mtime`, which current doors and legion-link treat as `unsupported`.
  process.stdout.write(`${JSON.stringify({ protocol: 1 })}\n`);
  const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
  rl.once('line', (line) => {
    let req = null;
    try {
      req = JSON.parse(line);
    } catch {
      process.stdout.write(
        `${JSON.stringify({ error: { code: 'invalid_args', message: 'Invalid stream header.' } })}\n`,
      );
      process.exit(0);
    }
    const rawPath = typeof req?.path === 'string' ? req.path : '';
    const resolved =
      rawPath === '~'
        ? userHome
        : rawPath.startsWith('~/')
          ? path.join(userHome, rawPath.slice(2))
          : rawPath;
    try {
      const st = fs.statSync(resolved);
      process.stdout.write(`${JSON.stringify({ size: st.size })}\n`);
    } catch {
      process.stdout.write(
        `${JSON.stringify({ error: { code: 'not_found', message: 'File not found.' } })}\n`,
      );
    }
    process.exit(0);
  });
}
