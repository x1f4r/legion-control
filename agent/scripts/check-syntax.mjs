import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const source = fileURLToPath(new URL('../src/', import.meta.url));
const files = fs.readdirSync(source, { recursive: true }).filter((file) => file.endsWith('.mjs')).sort();
for (const file of files) {
  const result = spawnSync(process.execPath, ['--check', path.join(source, file)], { stdio: 'inherit', shell: false });
  if (result.error) console.error(result.error.message);
  if (result.status !== 0) process.exit(result.status ?? 1);
}
console.log(`Syntax checked ${files.length} source modules.`);

// Zero-width, bidirectional and byte order marks make source text read
// differently from how it runs, which is how a reviewer approves a guard that
// does something else. The agent refuses them in anything a person approves,
// so its own code writes them as \u escapes, never as the characters.
const install = fileURLToPath(new URL('../install/', import.meta.url));
const shipped = [
  ...files.map((file) => path.join(source, file)),
  ...fs.readdirSync(install).filter((file) => /\.(mjs|sh|ps1)$/.test(file)).sort().map((file) => path.join(install, file)),
];
const invisible = /[​-‏‪-‮⁠-⁩﻿]/gu;
let found = 0;
for (const file of shipped) {
  fs.readFileSync(file, 'utf8').split('\n').forEach((line, index) => {
    const points = [...line.matchAll(invisible)].map(([character]) => `U+${character.codePointAt(0).toString(16).toUpperCase().padStart(4, '0')}`);
    if (points.length === 0) return;
    console.error(`${path.relative(path.dirname(source), file)}:${index + 1}: ${points.join(' ')}; write it as a \\u escape`);
    found += 1;
  });
}
if (found > 0) process.exit(1);
console.log(`No invisible or bidirectional characters in ${shipped.length} shipped files.`);
