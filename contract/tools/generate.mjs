#!/usr/bin/env node
// Writes, or checks, every generated file of the contract.
//
//   node contract/tools/generate.mjs           write what differs, remove what is extra
//   node contract/tools/generate.mjs --check   write nothing; exit 1 if anything differs
//
// The one entry point for contract/fixtures, contract/vectors and
// contract/hash-vectors.json. There used to be one script per directory, each
// writing its own files, and nothing noticed when somebody edited a fixture by
// hand or forgot to re-run one of them. `--check` is what CI runs: a committed
// byte that differs from what the generators produce, or a generated file that
// is missing or that no generator produces, fails it.
//
// The output depends on nothing but contract/tools and agent/package.json: no
// clock, no random source, no environment, no platform. Running this twice, on
// any machine, gives the same bytes.

import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { compare, write } from './emit.mjs';

const CONTRACT = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const HINT = '(run node contract/tools/generate.mjs)';

async function build() {
  // Imported here rather than at the top so that a generator that throws while
  // it is being loaded, which is where a disagreeing `want` is found, is
  // reported as a generation failure instead of a stack trace.
  const { buildFixtures } = await import('./make-fixtures.mjs');
  const { buildHashVectors } = await import('./make-hash-vectors.mjs');
  const { buildVectors } = await import('./make-vectors.mjs');
  const files = new Map();
  for (const part of [buildFixtures(), buildHashVectors(), buildVectors()]) {
    for (const [relative, text] of part) {
      if (files.has(relative)) throw new Error(`two generators produce ${relative}`);
      files.set(relative, text);
    }
  }
  return files;
}

async function main() {
  const check = process.argv.slice(2).includes('--check');
  let files;
  try {
    files = await build();
  } catch (error) {
    process.stderr.write(`generation failed: ${error.message}\n`);
    process.exitCode = 1;
    return;
  }

  if (check) {
    const { stale, missing, extra } = compare(files, CONTRACT);
    const lines = [
      ...stale.map((relative) => `stale ${relative} ${HINT}`),
      ...missing.map((relative) => `missing ${relative} ${HINT}`),
      ...extra.map((relative) => `extra ${relative} ${HINT}`),
    ];
    if (lines.length === 0) {
      process.stdout.write(`${files.size} generated files are up to date\n`);
      return;
    }
    process.stdout.write(`${lines.join('\n')}\n`);
    process.exitCode = 1;
    return;
  }

  const { written, removed, unchanged } = write(files, CONTRACT);
  for (const relative of written) process.stdout.write(`wrote ${relative}\n`);
  for (const relative of removed) process.stdout.write(`removed ${relative}\n`);
  process.stdout.write(`${written.length} written, ${removed.length} removed, ${unchanged} unchanged\n`);
}

await main();
