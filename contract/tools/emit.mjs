// Compares or writes the generated files of contract/.
//
// Generated means every *.json in contract/fixtures and contract/vectors, and
// contract/hash-vectors.json: the managed set. A file in it that no generator
// produced is "extra", because a fixture or a vector family nobody generates is
// one nobody will ever update, and clients that read the index would trust it.
// Comparison is by bytes. A file that parses to the same JSON but differs in a
// space or a line ending is still stale: clients hash and diff these files, and
// two checkouts that disagree byte for byte are two contracts.

import fs from 'node:fs';
import path from 'node:path';

const MANAGED_DIRECTORIES = ['fixtures', 'vectors'];
const MANAGED_FILES = ['hash-vectors.json'];

/** Whether a path relative to contract/ is one the generators own. */
export function isManaged(relative) {
  if (MANAGED_FILES.includes(relative)) return true;
  const [directory, name, ...rest] = relative.split('/');
  return MANAGED_DIRECTORIES.includes(directory) && rest.length === 0 && typeof name === 'string' && name.endsWith('.json');
}

/** The managed files that exist under `root` (contract/), as sorted relative paths. */
export function managedOnDisk(root) {
  const found = [];
  for (const directory of MANAGED_DIRECTORIES) {
    const dir = path.join(root, directory);
    if (!fs.existsSync(dir)) continue;
    for (const name of fs.readdirSync(dir)) {
      if (name.endsWith('.json') && fs.statSync(path.join(dir, name)).isFile()) found.push(`${directory}/${name}`);
    }
  }
  for (const file of MANAGED_FILES) if (fs.existsSync(path.join(root, file))) found.push(file);
  return found.sort();
}

/**
 * What differs between the generated `files` (a Map from relative path to text)
 * and what is on disk under `root`.
 */
export function compare(files, root) {
  for (const relative of files.keys()) {
    if (!isManaged(relative)) throw new Error(`a generator produced ${relative}, which is outside the managed files`);
  }
  const stale = [];
  const missing = [];
  for (const [relative, text] of [...files].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))) {
    const file = path.join(root, relative);
    if (!fs.existsSync(file)) missing.push(relative);
    else if (!fs.readFileSync(file).equals(Buffer.from(text, 'utf8'))) stale.push(relative);
  }
  const extra = managedOnDisk(root).filter((relative) => !files.has(relative));
  return { stale, missing, extra };
}

/** Bring the managed files under `root` in line with `files`: write what differs, remove what is extra. */
export function write(files, root) {
  const { stale, missing, extra } = compare(files, root);
  for (const relative of [...missing, ...stale]) {
    const file = path.join(root, relative);
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, files.get(relative), 'utf8');
  }
  for (const relative of extra) fs.rmSync(path.join(root, relative));
  return { written: [...missing, ...stale].sort(), removed: extra, unchanged: files.size - missing.length - stale.length };
}
