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
