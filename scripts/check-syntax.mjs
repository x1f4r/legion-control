#!/usr/bin/env node
// Check that every JavaScript file in the checkout parses.
//
//   node scripts/check-syntax.mjs               this checkout
//   node scripts/check-syntax.mjs <directory>   another one
//
// npm --prefix agent run check parses agent/src, the modules the agent ships, and nothing else.
// The rest runs too: the release and check scripts, the installers' helpers, the contract's
// generators, references and validator, and every test. A script that does not parse fails only
// once something runs it, and for a release script that is in the middle of a release.
//
// scripts/check.sh runs it.
import { execFile } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';
import { checkoutFiles } from './checkout-files.mjs';

const repository = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const run = promisify(execFile);

/**
 * Parses each JavaScript file of the checkout with `node --check`, a few at a time, and returns
 * how many there were and, for each that does not parse, `{ file, message }` with what node said.
 */
export async function checkSyntax(root = repository) {
  const files = checkoutFiles(root, ['*.mjs', '*.cjs', '*.js']).filter(file => fs.existsSync(path.join(root, file)));
  const problems = [];
  let next = 0;
  const worker = async () => {
    while (next < files.length) {
      const file = files[next++];
      try {
        await run(process.execPath, ['--check', path.join(root, file)], { maxBuffer: 1024 * 1024 });
      } catch (error) {
        problems.push({ file, message: String(error.stderr || error.message).trim() });
      }
    }
  };
  await Promise.all(Array.from({ length: Math.min(os.availableParallelism(), 8) }, worker));
  problems.sort((a, b) => (a.file < b.file ? -1 : a.file > b.file ? 1 : 0));
  return { files: files.length, problems };
}

// Compared through realpath, as contract/validate.mjs does. Node resolves the symlinks in
// import.meta.url but not in argv[1], so through a symlinked checkout a plain comparison never
// runs the check and the gate passes having looked at nothing.
const real = (file) => { try { return fs.realpathSync(file); } catch { return path.resolve(file); } };
if (process.argv[1] && real(process.argv[1]) === real(fileURLToPath(import.meta.url))) {
  const result = await checkSyntax(path.resolve(process.argv[2] ?? repository));
  for (const { file, message } of result.problems) console.error(`${file} does not parse:\n${message}\n`);
  if (result.problems.length) {
    console.error(`${result.problems.length} of ${result.files} JavaScript files do not parse.`);
    process.exitCode = 1;
  } else {
    console.log(`Syntax checked ${result.files} JavaScript files.`);
  }
}
