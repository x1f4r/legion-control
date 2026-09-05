// Reading a tar archive that somebody else made, safely.
//
// THE ARCHIVE IS NEVER HANDED TO `tar`. Not because tar is bad, but because the
// dangerous part of extraction is the part where a program decides what a path
// means, and the three tars the agent might meet (GNU, bsdtar, Windows tar.exe)
// each decide slightly differently about absolute paths, symlinks, hardlinks,
// device nodes and pax records. A parser here is a hundred lines and its rules
// are visible.
//
// NOTHING IS WRITTEN BEFORE EVERYTHING IS VERIFIED. The archive is decompressed
// and parsed into memory, the manifest signature is checked against the pinned
// key, every file's hash is checked against the manifest, and only then does a
// single byte reach the filesystem. An archive that fails any check has not
// touched the disk at all, so there is nothing to clean up and nothing that
// could be executed by mistake.
//
// What is refused, and every one of these is a real attack against a naive
// extractor:
//
//   absolute paths          /etc/cron.d/x — writes outside the destination
//   parent traversal        ../../.ssh/authorized_keys — the same, by another route
//   Windows drive letters   C:\... and \\server\share — the same, on Windows
//   symlinks and hardlinks  a link to /etc, then a write "inside" it
//   device nodes and fifos  not files, and nothing here needs them
//   duplicate paths         write a safe file, then overwrite it after the check
//   unlisted files          anything the signed manifest does not name
//   size and count limits   a decompression bomb

import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import zlib from 'node:zlib';
import { verifyDetachedSignature } from './trust.mjs';

/** Compressed bytes we are willing to read. The agent tarball is well under a MB. */
export const MAX_ARCHIVE_BYTES = 64 * 1024 * 1024;
/** Uncompressed bytes. A ratio of 100:1 is normal for source; 2000:1 is a bomb. */
export const MAX_TOTAL_BYTES = 128 * 1024 * 1024;
export const MAX_ENTRY_BYTES = 32 * 1024 * 1024;
export const MAX_ENTRIES = 5000;

const BLOCK = 512;
const REGULAR_TYPES = new Set(['0', '\0', '']);

function fail(message) {
  const error = new Error(message);
  error.archiveRejection = true;
  return error;
}

/** Read a NUL-terminated field. */
function readString(block, offset, length) {
  const slice = block.subarray(offset, offset + length);
  const end = slice.indexOf(0);
  return slice.subarray(0, end === -1 ? slice.length : end).toString('utf8');
}

/** Tar numbers are octal ASCII, except GNU's base-256 form for large values. */
function readNumber(block, offset, length) {
  const slice = block.subarray(offset, offset + length);
  if (slice.length > 0 && (slice[0] & 0x80) !== 0) {
    let value = 0n;
    for (const byte of slice.subarray(1)) value = (value << 8n) | BigInt(byte);
    if (value > BigInt(Number.MAX_SAFE_INTEGER)) throw fail('a tar header carries an implausible size');
    return Number(value);
  }
  const text = readString(block, offset, length).trim();
  if (text.length === 0) return 0;
  const parsed = Number.parseInt(text, 8);
  if (!Number.isFinite(parsed) || parsed < 0) throw fail(`a tar header field is not a number: ${JSON.stringify(text)}`);
  return parsed;
}

/**
 * The header checksum, computed with the checksum field itself read as spaces.
 *
 * Checking it is cheap and it is the only thing that distinguishes a real header
 * from 512 bytes of anything else, which matters when the parser is deciding how
 * far to skip.
 */
function checksumMatches(block) {
  const stored = readNumber(block, 148, 8);
  let unsigned = 0;
  let signed = 0;
  for (let index = 0; index < BLOCK; index += 1) {
    const byte = index >= 148 && index < 156 ? 0x20 : block[index];
    unsigned += byte;
    signed += byte > 127 ? byte - 256 : byte;
  }
  return stored === unsigned || stored === signed;
}

/** Parse the `size key=value\n` records of a pax extended header. */
function parsePax(buffer) {
  const attributes = {};
  let offset = 0;
  while (offset < buffer.length) {
    const space = buffer.indexOf(0x20, offset);
    if (space === -1) break;
    const length = Number.parseInt(buffer.subarray(offset, space).toString('ascii'), 10);
    if (!Number.isFinite(length) || length <= 0 || offset + length > buffer.length) break;
    const record = buffer.subarray(space + 1, offset + length).toString('utf8');
    const equals = record.indexOf('=');
    if (equals !== -1) attributes[record.slice(0, equals)] = record.slice(equals + 1).replace(/\n$/, '');
    offset += length;
  }
  return attributes;
}

/**
 * Whether a path may be written under a destination directory.
 *
 * Deliberately conservative: only the characters a source tree actually needs.
 * Anything unusual is refused rather than normalised, because normalisation is
 * where extractors get this wrong.
 */
export function rejectUnsafePath(name) {
  if (typeof name !== 'string' || name.length === 0) return 'an entry has no name';
  if (name.length > 1024) return `an entry name is longer than 1024 characters`;
  if (name.includes('\0')) return 'an entry name contains a NUL byte';
  if (name.startsWith('/') || name.startsWith('\\')) return `"${name}" is an absolute path`;
  if (/^[A-Za-z]:/.test(name)) return `"${name}" carries a Windows drive letter`;
  if (name.startsWith('~')) return `"${name}" starts with a home shortcut`;
  const parts = name.split('/');
  for (const part of parts) {
    if (part === '') return `"${name}" has an empty path segment`;
    if (part === '.' || part === '..') return `"${name}" walks out of the archive with "${part}"`;
    if (/[\\:<>"|?*\x00-\x1f]/.test(part) || /[. ]$/.test(part) || /^(con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\.|$)/i.test(part)) return `"${name}" is not a portable file path`;
  }
  return null;
}

/**
 * Parse a tar archive into entries, in memory.
 *
 * Returns `{ files: Map<path, Buffer>, directories: string[] }`. Throws on
 * anything unsafe, and the message says what and why.
 */
export function readTar(buffer) {
  const files = new Map();
  const directories = [];
  const seen = new Set();
  let offset = 0;
  let total = 0;
  let count = 0;
  let pending = {};

  while (offset + BLOCK <= buffer.length) {
    const header = buffer.subarray(offset, offset + BLOCK);
    // Two consecutive zero blocks end the archive; one is enough to stop on.
    if (header.every((byte) => byte === 0)) break;
    if (!checksumMatches(header)) throw fail('the archive contains a block that is not a valid tar header');

    const size = readNumber(header, 124, 12);
    const typeflag = readString(header, 156, 1);
    const prefix = readString(header, 345, 155);
    const rawName = readString(header, 0, 100);
    const body = buffer.subarray(offset + BLOCK, offset + BLOCK + size);
    offset += BLOCK + Math.ceil(size / BLOCK) * BLOCK;

    // Extended headers describe the NEXT entry rather than being one.
    if (typeflag === 'x') {
      pending = { ...pending, ...parsePax(body) };
      continue;
    }
    if (typeflag === 'g') {
      // A global header applies to everything that follows. Nothing the agent
      // needs comes from one, so it is read and dropped.
      continue;
    }
    if (typeflag === 'L') {
      pending.path = body.toString('utf8').replace(/\0+$/, '');
      continue;
    }
    if (typeflag === 'K') {
      pending.linkpath = body.toString('utf8').replace(/\0+$/, '');
      continue;
    }

    const rawPath = pending.path ?? (prefix ? `${prefix}/${rawName}` : rawName);
    const name = typeflag === '5' && rawPath.endsWith('/') ? rawPath.slice(0, -1) : rawPath;
    pending = {};

    count += 1;
    if (count > MAX_ENTRIES) throw fail(`the archive has more than ${MAX_ENTRIES} entries`);

    const unsafe = rejectUnsafePath(name);
    if (unsafe) throw fail(unsafe);
    const collisionKey = name.toLowerCase();
    if (seen.has(collisionKey)) throw fail(`"${name}" appears twice in the archive`);
    seen.add(collisionKey);

    if (typeflag === '5') {
      directories.push(name);
      continue;
    }
    if (typeflag === '1' || typeflag === '2') {
      throw fail(`"${name}" is a link, and this archive may only contain regular files and directories`);
    }
    if (!REGULAR_TYPES.has(typeflag)) {
      throw fail(`"${name}" is a tar entry of type ${JSON.stringify(typeflag)}, which is not a regular file`);
    }

    if (size > MAX_ENTRY_BYTES) throw fail(`"${name}" is larger than ${MAX_ENTRY_BYTES} bytes`);
    total += size;
    if (total > MAX_TOTAL_BYTES) throw fail(`the archive expands to more than ${MAX_TOTAL_BYTES} bytes`);
    if (body.length !== size) throw fail(`"${name}" is truncated`);
    // A second entry for the same path is how a checked file gets replaced by an
    // unchecked one after verification.
    if (files.has(name)) throw fail(`"${name}" appears twice in the archive`);

    files.set(name, Buffer.from(body));
  }

  return { files, directories };
}

function sha256(buffer) {
  return crypto.createHash('sha256').update(buffer).digest('hex');
}

/**
 * Verify a signed agent bundle, entirely in memory.
 *
 * Returns `{ ok, files, manifest, fingerprint }` or `{ ok: false, error }`.
 * Nothing is written; the caller stages the returned files itself once it has
 * decided it wants them.
 */
export function verifyAgentArchive(archiveBytes, { root = 'agent' } = {}) {
  const reject = (error) => ({ ok: false, error, files: null, manifest: null });

  if (!Buffer.isBuffer(archiveBytes) || archiveBytes.length === 0) return reject('the bundle is empty');
  if (archiveBytes.length > MAX_ARCHIVE_BYTES) {
    return reject(`the bundle is larger than ${MAX_ARCHIVE_BYTES} bytes`);
  }

  let tar;
  try {
    // gzip is what package-agent.mjs produces; a plain tar is accepted too so a
    // bundle can be inspected without recompressing it.
    const looksGzipped = archiveBytes[0] === 0x1f && archiveBytes[1] === 0x8b;
    tar = looksGzipped ? zlib.gunzipSync(archiveBytes, { maxOutputLength: MAX_TOTAL_BYTES }) : archiveBytes;
  } catch (err) {
    return reject(`the bundle could not be decompressed: ${err.message}`);
  }

  let parsed;
  try {
    parsed = readTar(tar);
  } catch (err) {
    return reject(err.message);
  }

  return verifyAgentFiles(parsed.files, root);
}

function verifyAgentFiles(archiveFiles, root = 'agent') {
  const reject = (error) => ({ ok: false, error, files: null, manifest: null });
  const parsed = { files: archiveFiles };
  const prefix = `${root}/`;
  for (const name of parsed.files.keys()) {
    if (!name.startsWith(prefix)) return reject(`"${name}" is outside the "${root}/" directory the bundle must contain`);
  }

  const manifestBytes = parsed.files.get(`${prefix}MANIFEST.json`);
  const signatureBytes = parsed.files.get(`${prefix}MANIFEST.json.sig`);
  if (!manifestBytes) return reject(`the bundle has no ${prefix}MANIFEST.json`);
  if (!signatureBytes) return reject(`the bundle has no ${prefix}MANIFEST.json.sig`);

  // The signature is checked FIRST, over the exact manifest bytes, before a
  // single value from the manifest is trusted for anything.
  const signature = verifyDetachedSignature(manifestBytes, signatureBytes.toString('utf8'));
  if (!signature.ok) return reject(signature.error);

  let manifest;
  try {
    manifest = JSON.parse(manifestBytes.toString('utf8'));
  } catch (err) {
    return reject(`MANIFEST.json does not parse: ${err.message}`);
  }
  if (!manifest || typeof manifest !== 'object' || Array.isArray(manifest)) return reject('MANIFEST.json is not an object');
  if (manifest.schema !== 1) return reject(`MANIFEST.json declares schema ${JSON.stringify(manifest.schema)}, not 1`);
  if (typeof manifest.version !== 'string' || !/^\d+\.\d+\.\d+$/.test(manifest.version)) {
    return reject(`MANIFEST.json has no usable version, got ${JSON.stringify(manifest.version)}`);
  }
  if (!Number.isInteger(manifest.contract) || manifest.contract < 3) return reject('MANIFEST.json has no supported contract');
  if (!Array.isArray(manifest.files) || manifest.files.length === 0 || manifest.files.length > MAX_ENTRIES) return reject('MANIFEST.json lists no files');

  const listed = new Set();
  for (const entry of manifest.files) {
    if (!entry || typeof entry !== 'object') return reject('MANIFEST.json has an entry that is not an object');
    const unsafe = rejectUnsafePath(entry.path);
    if (unsafe) return reject(`MANIFEST.json lists an unusable path: ${unsafe}`);
    if (!/^[0-9a-f]{64}$/.test(entry.sha256 ?? '')) return reject(`MANIFEST.json has no sha256 for "${entry.path}"`);
    if (!Number.isSafeInteger(entry.size) || entry.size < 0) return reject(`MANIFEST.json has no size for "${entry.path}"`);
    if (entry.path === 'MANIFEST.json' || entry.path === 'MANIFEST.json.sig') return reject('MANIFEST.json must not list itself or its signature');
    if ([...listed].some((name) => name.toLowerCase() === entry.path.toLowerCase())) return reject(`MANIFEST.json lists "${entry.path}" twice`);
    listed.add(entry.path);

    const bytes = parsed.files.get(prefix + entry.path);
    if (!bytes) return reject(`the bundle is missing "${entry.path}", which the manifest lists`);
    if (bytes.length !== entry.size) {
      return reject(`"${entry.path}" is ${bytes.length} bytes and the manifest says ${entry.size}`);
    }
    if (sha256(bytes) !== entry.sha256) return reject(`"${entry.path}" does not match its hash in the manifest`);
  }

  // Nothing may ride along unlisted. The manifest cannot list itself or its own
  // signature — a file cannot contain its own hash — so those two are the only
  // permitted exceptions, and they are permitted by name.
  for (const name of parsed.files.keys()) {
    const relative = name.slice(prefix.length);
    if (relative === 'MANIFEST.json' || relative === 'MANIFEST.json.sig') continue;
    if (!listed.has(relative)) return reject(`"${relative}" is in the bundle but not in the signed manifest`);
  }

  if (!listed.has('package.json') || !listed.has('src/index.mjs')) return reject('the signed inventory must include package.json and src/index.mjs');
  try {
    const metadata = JSON.parse(parsed.files.get(prefix + 'package.json').toString('utf8'));
    if (metadata.version !== manifest.version) return reject('package.json version differs from the signed manifest');
  } catch { return reject('the signed package.json does not parse'); }

  const files = new Map();
  for (const entry of manifest.files) files.set(entry.path, parsed.files.get(prefix + entry.path));
  files.set('MANIFEST.json', manifestBytes);
  files.set('MANIFEST.json.sig', signatureBytes);

  return { ok: true, error: null, files, manifest, entryCount: manifest.files.length };
}

/** Verify a tree before executing it, including rejection of extra files and links. */
export function verifyAgentTree(directory) {
  const reject = (error) => ({ ok: false, error, files: null, manifest: null });
  const files = new Map();
  let total = 0;
  let count = 0;
  try {
    const walk = (relative = '') => {
      const absolute = path.join(directory, relative);
      const stat = fs.lstatSync(absolute);
      if (stat.isSymbolicLink()) throw fail(`"${relative || directory}" is a link`);
      if (stat.isDirectory()) {
        for (const name of fs.readdirSync(absolute)) walk(relative ? `${relative}/${name}` : name);
        return;
      }
      if (!stat.isFile()) throw fail(`"${relative}" is not a regular file`);
      const unsafe = rejectUnsafePath(relative);
      if (unsafe) throw fail(unsafe);
      if (++count > MAX_ENTRIES || stat.size > MAX_ENTRY_BYTES || (total += stat.size) > MAX_TOTAL_BYTES) throw fail('the installed tree exceeds bundle size limits');
      const bytes = fs.readFileSync(absolute);
      if (bytes.length !== stat.size) throw fail(`"${relative}" changed while it was verified`);
      files.set(`agent/${relative}`, bytes);
    };
    walk();
    return verifyAgentFiles(files);
  } catch (error) { return reject(error.message); }
}

/**
 * Write verified files into a directory that must not already exist.
 *
 * Only ever called with the output of verifyAgentArchive, and only after it said
 * ok. The paths were validated there; they are re-resolved here anyway, because
 * a check that is only made once is a check that a refactor can remove.
 */
export function stageFiles(files, destination) {
  if (fs.existsSync(destination)) return { ok: false, error: `${destination} already exists` };
  const resolvedRoot = path.resolve(destination);
  try {
    fs.mkdirSync(resolvedRoot, { recursive: true });
    for (const [relative, bytes] of files) {
      const target = path.resolve(resolvedRoot, relative);
      if (target !== resolvedRoot && !target.startsWith(resolvedRoot + path.sep)) {
        throw fail(`"${relative}" resolves outside ${destination}`);
      }
      fs.mkdirSync(path.dirname(target), { recursive: true });
      fs.writeFileSync(target, bytes, { mode: relative.endsWith('.sh') ? 0o755 : 0o644 });
    }
    return { ok: true, error: null, path: resolvedRoot };
  } catch (err) {
    try {
      fs.rmSync(resolvedRoot, { recursive: true, force: true });
    } catch {
      /* the caller reports the original failure */
    }
    return { ok: false, error: err.message };
  }
}
