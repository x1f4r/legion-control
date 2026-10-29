import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';
import { invisibleCharacters, reviewedFiles } from './invisible.mjs';

const source = fileURLToPath(new URL('../src/', import.meta.url));
const files = fs.readdirSync(source, { recursive: true }).filter((file) => file.endsWith('.mjs')).sort();
for (const file of files) {
  const result = spawnSync(process.execPath, ['--check', path.join(source, file)], { stdio: 'inherit', shell: false });
  if (result.error) console.error(result.error.message);
  if (result.status !== 0) process.exit(result.status ?? 1);
}
console.log(`Syntax checked ${files.length} source modules.`);

// Zero-width, bidirectional and byte order marks are refused in every file a
// person reviews, this script and the check it runs included; invisible.mjs
// says why.
const agent = fileURLToPath(new URL('../', import.meta.url));
const reviewed = reviewedFiles(agent);
let found = 0;
for (const file of reviewed) {
  for (const { line, points } of invisibleCharacters(fs.readFileSync(file, 'utf8'), file)) {
    console.error(`${path.relative(agent, file)}:${line}: ${points.join(' ')}; write it as a \\u escape`);
    found += 1;
  }
}
if (found > 0) process.exit(1);
console.log(`No invisible or bidirectional characters in ${reviewed.length} reviewed files.`);
