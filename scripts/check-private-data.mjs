#!/usr/bin/env node
// Check that the checkout names nobody's home directory, network or private name.
//
//   node scripts/check-private-data.mjs               this checkout
//   node scripts/check-private-data.mjs <directory>   another one
//
// The repository is public, and each of these has reached it before: a path with a real user name
// in it, a home network's addresses, the names of the people and machines it was written on. Every
// file git tracks or would track is read, and each file's name as well, for three things:
//
// - A home directory whose user is not a placeholder: /home/<user>, /Users/<user>, or
//   <drive>:\Users\<user> with \, \\ or / between the parts, and the same homes reached through a
//   mount, /mnt/<drive>/Users/<user> in WSL and /var/home/<user> on an image-based Fedora. The user
//   the fixtures, vectors, examples and tests are written for is `me`; PLACEHOLDER_HOMES has the
//   few others there are.
// - A private IPv4 address outside the ranges the same files are written with, which are
//   10.0.0.0/24 and the /24s of 192.168.1, 192.168.10 and 192.168.50. The rest of 10.0.0.0/8,
//   172.16.0.0/12 and 192.168.0.0/16 is somebody's network, and so is a three-part 192.168 prefix,
//   the way a site names its network. Addresses that are nobody's are in SHARED_ADDRESSES.
//   100.64.0.0/10 is a placeholder range as well, but a tailnet's real addresses are in it too, so
//   those belong in the private terms.
// - Private terms: the real names, user names, host names and addresses that would identify the
//   maintainers, which a public check cannot spell out. They come from LEGION_PRIVATE_TERMS, which
//   CI fills from a repository secret, and from ~/.legion-control/private/terms.txt, outside the
//   repository, in the same format: one term per line or several separated by commas, # for a
//   comment. A term matches in any letter case. Either source may be missing, and the check says
//   which it read. It never prints a term: a finding gives the file, the line and the entry of the
//   source it matched, so the log of a public CI run shows no more than the change itself does.
//
// scripts/check.sh runs it, and CI runs it once more with the secret.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { checkoutFiles } from './checkout-files.mjs';

const repository = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

// The users a home directory may belong to: the placeholder, the accounts GitHub's runners build
// as (runner on Linux and macOS, runneradmin on Windows), and the folders macOS and Windows keep
// beside their users' own.
export const PLACEHOLDER_HOMES = new Set(['me', 'runner', 'runneradmin', 'Shared', 'Public', 'Default']);

// Private addresses that are nobody's network. The wake and acceptance vectors and the wake
// reference need blocks named as a whole, and values on either side of the boundary a prefix or a
// CIDR block draws, which no placeholder range can stand in for. And an Android emulator reaches
// the machine it runs on at 10.0.2.2, wherever that is.
export const SHARED_ADDRESSES = new Set([
  '10.0.2.2',
  '10.0.10.12', '10.20.30.40',
  '172.16.0.0', '172.31.255.1',
  '192.168.0.0', '192.168.11.1', '192.168.16.0', '192.168.19', '192.168.100', '192.168.100.7', '192.168.199', '192.168.255.255',
]);

export const TERMS_VARIABLE = 'LEGION_PRIVATE_TERMS';
export const TERMS_FILE = '~/.legion-control/private/terms.txt';

// A home directory: /home/ or /Users/ where a path starts, or a drive's \Users\. What follows is
// read separately, so a placeholder such as <user>, $USER or ${user} names nobody. The mounts are
// spelled out, since the start of a path is what keeps a web address's /home/ or /users/ out, and
// /mnt/c/Users/ is the way a Windows home comes back in a Linux test or doc.
const HOME = /(?<![\w.-])(?:(?:\/var)?\/home\/|(?:\/mnt\/[A-Za-z])?\/Users\/|[A-Za-z]:(\\\\|\\|\/)Users\1)/gi;
const USER = /[A-Za-z0-9_][\w.-]*/y;
// A dotted IPv4 address, or three parts of one. Not inside a longer run of numbers and dots, which
// is a version or a build number.
const IPV4 = /(?<![\w.])(\d{1,3})\.(\d{1,3})\.(\d{1,3})(?:\.(\d{1,3}))?(?!\w|\.\d)/g;

/** The terms of one source's text, each with its entry number in that source. */
function parseTerms(text, source) {
  // Comments first, so a comma in one does not make a term of what follows it.
  const entries = text.split('\n').filter(line => !line.trim().startsWith('#'))
    .flatMap(line => line.split(',')).map(entry => entry.trim()).filter(Boolean);
  return entries.map((entry, index) => {
    // One letter or two would match nearly every file, and with the term never printed, a failure
    // that says nothing about why. A typo in the secret should say so.
    if (entry.length < 3) throw new Error(`entry ${index + 1} of ${source} is shorter than three characters, which would match almost anything.`);
    return { term: entry.toLowerCase(), source, entry: index + 1 };
  });
}

/**
 * The private terms to look for, from the environment variable and the file, and the names of the
 * sources that had any.
 */
export function privateTerms({ env = process.env, home = os.homedir() } = {}) {
  const terms = [];
  const sources = [];
  if (env[TERMS_VARIABLE]?.trim()) {
    terms.push(...parseTerms(env[TERMS_VARIABLE], TERMS_VARIABLE));
    sources.push(TERMS_VARIABLE);
  }
  let text = null;
  try { text = fs.readFileSync(path.join(home, '.legion-control', 'private', 'terms.txt'), 'utf8'); }
  catch (error) { if (error.code !== 'ENOENT') throw new Error(`${TERMS_FILE} cannot be read (${error.code ?? error.message}).`); }
  if (text?.trim()) {
    terms.push(...parseTerms(text, TERMS_FILE));
    sources.push(TERMS_FILE);
  }
  return { terms, sources };
}

/** Whether dotted parts are a private address, or a 192.168 prefix, outside the placeholder ranges. */
function privateAddress(parts) {
  const [a, b, c] = parts.map(Number);
  if (parts.length === 3 && !(a === 192 && b === 168)) return false;
  if (parts.some(part => Number(part) > 255)) return false;
  if (SHARED_ADDRESSES.has(parts.map(Number).join('.'))) return false;
  if (a === 10) return !(b === 0 && c === 0);
  if (a === 172) return b >= 16 && b <= 31;
  if (a === 192 && b === 168) return ![1, 10, 50].includes(c);
  return false;
}

/** What one line of text, or a file's name, gives away, as messages. */
function lineFindings(line, terms, holds = 'holds') {
  const found = [];
  for (const match of line.matchAll(HOME)) {
    USER.lastIndex = match.index + match[0].length;
    // A user at the end of a sentence keeps its full stop out of the name.
    const user = USER.exec(line)?.[0].replace(/\.+$/, '');
    if (user && !PLACEHOLDER_HOMES.has(user)) found.push(`${match[0]}${user} is somebody's home directory; write it for the placeholder user me.`);
  }
  for (const match of line.matchAll(IPV4)) {
    const parts = match.slice(1).filter(part => part !== undefined);
    if (privateAddress(parts)) found.push(`${match[0]} is on a private network; use an address from 192.0.2.0/24, 198.51.100.0/24 or 10.0.0.0/24, or the 192.168.1, 192.168.10 and 192.168.50 networks.`);
  }
  const lower = line.toLowerCase();
  for (const { term, source, entry } of terms) {
    if (lower.includes(term)) found.push(`${holds} entry ${entry} of ${source}.`);
  }
  return found;
}

/**
 * Every private value in the checkout under `root`, as `{ file, line, message }` (line 0 for a
 * file's name), and how many files and terms were looked at, from which sources.
 */
export function checkPrivateData(root = repository, options = {}) {
  const { terms, sources } = privateTerms(options);
  const findings = [];
  const files = checkoutFiles(root);
  for (const file of files) {
    // A name is read as a line is: a fixture named for somebody's address or home gives it away
    // as surely as one that holds it.
    for (const message of lineFindings(file, terms, 'its name holds')) findings.push({ file, line: 0, message });
    let bytes;
    try { bytes = fs.readFileSync(path.join(root, file)); } catch { continue; }
    // A NUL in the first few kilobytes is how git tells a binary file, and an image or a keystore
    // has no lines to point at.
    if (bytes.subarray(0, 8000).includes(0)) continue;
    bytes.toString('utf8').split('\n').forEach((line, index) => {
      for (const message of lineFindings(line, terms)) findings.push({ file, line: index + 1, message });
    });
  }
  return { files: files.length, terms: terms.length, sources, findings };
}

// Compared through realpath, as contract/validate.mjs does. Node resolves the symlinks in
// import.meta.url but not in argv[1], so through a symlinked checkout a plain comparison never
// runs the check and the gate passes having looked at nothing.
const real = (file) => { try { return fs.realpathSync(file); } catch { return path.resolve(file); } };
if (process.argv[1] && real(process.argv[1]) === real(fileURLToPath(import.meta.url))) {
  let result;
  try {
    result = checkPrivateData(path.resolve(process.argv[2] ?? repository));
  } catch (error) {
    console.error(error.message);
    process.exit(2);
  }
  if (result.sources.length) console.log(`Looked for ${result.terms} private terms from ${result.sources.join(' and ')}.`);
  else console.log(`Looked for no private terms: ${TERMS_VARIABLE} is not set and there is no ${TERMS_FILE}. Home directories and addresses were still checked.`);
  for (const { file, line, message } of result.findings) console.error(line ? `${file}:${line}: ${message}` : `${file}: ${message}`);
  if (result.findings.length) {
    const named = new Set(result.findings.map(finding => finding.file)).size;
    console.error(`${result.findings.length} private values in ${named} of ${result.files} files. The repository is public: write placeholders instead.`);
    process.exitCode = 1;
  } else {
    console.log(`Checked ${result.files} files: no private home directories, addresses or terms.`);
  }
}
