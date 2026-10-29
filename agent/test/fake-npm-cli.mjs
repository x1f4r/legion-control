// A stand-in for npm's own entry point, npm-cli.js, so the npm path of the
// updater can be driven end to end without a registry and without installing
// anything real. fakeNpm() in helpers.mjs puts an npm-cli.js into a global
// prefix that hands its arguments here, behind the data directory and the
// prefix it belongs to.
//
// It answers the things the agent asks npm out of files in the data directory,
// the same directory fixture-service.mjs reads, and writes every call it was
// asked to `npm-calls`, one line each, so a test can say what was and was not
// installed.
import fs from 'node:fs';
import path from 'node:path';

const [dir, ownPrefix, ...args] = process.argv.slice(2);
const file = (name) => path.join(dir, name);
const read = (name, fallback = '') => {
  try {
    return fs.readFileSync(file(name), 'utf8').trim();
  } catch {
    return fallback;
  }
};
fs.appendFileSync(file('npm-calls'), `${args.join(' ')}\n`);

/** Where npm keeps a global prefix's packages, as the agent reads them. */
const modulesOf = (prefix) => (process.platform === 'win32' ? path.join(prefix, 'node_modules') : path.join(prefix, 'lib', 'node_modules'));

/** The name and version of a `name@version` spec, scoped names included. */
function spec(text) {
  const at = text.lastIndexOf('@');
  return at > 0 ? { name: text.slice(0, at), version: text.slice(at + 1) } : { name: text, version: null };
}

/**
 * Asleep, not spinning, for as long as the named file is there, the way
 * fixture-service.mjs holds its updater: a test can catch the install in its
 * running phase however slowly the agent it is watching starts.
 */
async function heldWhile(name) {
  const deadline = Date.now() + 120000;
  while (fs.existsSync(file(name)) && Date.now() < deadline) await new Promise((resolve) => setTimeout(resolve, 50));
}

const [verb] = args;
if (verb === '--version') {
  process.stdout.write('10.9.0\n');
} else if (verb === 'view') {
  if (read('latest-fails') === 'on') process.exit(1);
  process.stdout.write(`${JSON.stringify(read('latest', '1.0.0'))}\n`);
} else if (verb === 'cache') {
  process.exit(read('npm-cache-fails') === 'on' ? 1 : 0);
} else if (verb === 'prefix') {
  process.stdout.write(`${ownPrefix}\n`);
} else if (verb === 'install') {
  await heldWhile('npm-install-gate');
  const prefix = args[args.indexOf('--prefix') + 1];
  const wanted = spec(args[args.length - 1]);
  const root = path.join(modulesOf(prefix), wanted.name);
  // npm moves the old tree out of the way before it writes the new one, so an
  // install that fails part way can leave no package behind at all.
  fs.rmSync(root, { recursive: true, force: true });
  if (read('npm-install-fails') === wanted.version) {
    process.stderr.write(`npm error could not install ${wanted.name}@${wanted.version}\n`);
    process.exit(1);
  }
  fs.mkdirSync(root, { recursive: true });
  fs.writeFileSync(path.join(root, 'package.json'), `${JSON.stringify({ name: wanted.name, version: wanted.version })}\n`);
} else {
  process.exit(9);
}
