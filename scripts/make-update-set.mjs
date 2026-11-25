#!/usr/bin/env node
// Assemble and sign a Legion update set, generate a preview Ed25519 update key outside the
// repository, verify every artifact's embedded build sequence without executing it, and derive
// platform version numbers from a build sequence.
//
// Usage:
//   node scripts/make-update-set.mjs keygen [--key PATH] [--public-key PATH]
//   node scripts/make-update-set.mjs sequence
//   node scripts/make-update-set.mjs version-code <sequence>
//   node scripts/make-update-set.mjs tauri-config --os <linux|macos|windows> --version VER --sequence SEQ
//   node scripts/make-update-set.mjs create \
//     --out <dir> --channel <preview|stable> --version <semver> --build <rev> \
//     [--sequence <YYYYMMDDHHMM>] [--notes <text> | --notes-file <path>] \
//     --key <private-key> [--public-key <public-key>] \
//     --artifact <path[:program=...,os=...,arch=...,kind=...[,name=...]]> ...
//   node scripts/make-update-set.mjs verify --dir <dir> --public-key <public-key>
//
// Default preview key locations (always outside the repository, mode 600 for private):
//   private key: ~/.legion-control/preview-update.key
//   public key:  ~/.legion-control/preview-update.pub.pem
import { execFileSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import zlib from 'node:zlib';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
export const DEFAULT_PREVIEW_PRIVATE_KEY = path.join(os.homedir(), '.legion-control', 'preview-update.key');
export const DEFAULT_PREVIEW_PUBLIC_KEY = path.join(os.homedir(), '.legion-control', 'preview-update.pub.pem');

const SIGNATURE_PATTERN = /^[A-Za-z0-9+/]{86}==$/;
const ED25519_SPKI_PREFIX = Buffer.from([
  0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
]);
const RESERVED_NAMES = new Set(['update.json', 'SHA256SUMS', 'SHA256SUMS.sig']);
const VALID_CHANNELS = new Set(['stable', 'preview']);
const VALID_PROGRAMS = new Set(['node', 'app']);
const VALID_OS = new Set(['linux', 'macos', 'windows', 'android', 'ios']);
const VALID_ARCH = new Set(['x86_64', 'aarch64', 'universal']);
const VALID_KINDS = new Set(['binary', 'deb', 'apk', 'appimage', 'app-tar', 'nsis']);
const MAX_SMALL_SEQUENCE = 20_000_000;
const MIN_CALENDAR_SEQUENCE = 202001010000;
const MAX_CALENDAR_SEQUENCE = 299912312359;
export const MAX_FUTURE_SEQUENCE_HOURS = 24;
export const MAX_ARTIFACT_BYTES = 512 * 1024 * 1024;

/** Format a UTC Date as a 12-digit YYYYMMDDHHMM update sequence integer. */
export function currentSequence(date = new Date()) {
  const pad = (n, width = 2) => String(n).padStart(width, '0');
  const text =
    pad(date.getUTCFullYear(), 4) +
    pad(date.getUTCMonth() + 1) +
    pad(date.getUTCDate()) +
    pad(date.getUTCHours()) +
    pad(date.getUTCMinutes());
  return Number(text);
}

/** Parse a calendar YYYYMMDDHHMM sequence into a UTC Date, or null for synthetic small sequences. */
export function sequenceToUtcDate(rawSequence) {
  const sequence = typeof rawSequence === 'number' ? rawSequence : Number(String(rawSequence).trim());
  if (!Number.isSafeInteger(sequence) || sequence <= 0) {
    throw new Error(`Invalid sequence ${JSON.stringify(rawSequence)}: must be a positive integer.`);
  }
  if (sequence <= MAX_SMALL_SEQUENCE) {
    return null;
  }
  if (sequence >= MIN_CALENDAR_SEQUENCE && sequence <= MAX_CALENDAR_SEQUENCE) {
    const minute = sequence % 100;
    const hour = Math.floor(sequence / 100) % 100;
    const day = Math.floor(sequence / 10_000) % 100;
    const month = Math.floor(sequence / 1_000_000) % 100;
    const year = Math.floor(sequence / 100_000_000);
    const dt = new Date(Date.UTC(year, month - 1, day, hour, minute, 0, 0));
    if (
      dt.getUTCFullYear() !== year ||
      dt.getUTCMonth() + 1 !== month ||
      dt.getUTCDate() !== day ||
      dt.getUTCHours() !== hour ||
      dt.getUTCMinutes() !== minute
    ) {
      throw new Error(`Sequence ${sequence} is not a valid UTC YYYYMMDDHHMM timestamp.`);
    }
    return dt;
  }
  throw new Error(
    `Sequence ${sequence} is not a valid update sequence: pass a UTC YYYYMMDDHHMM timestamp or a small test sequence in 1..=${MAX_SMALL_SEQUENCE}.`,
  );
}

/**
 * Refuse malformed sequences and calendar sequences more than 24 hours ahead of `now` so a clock
 * typo or stolen preview key cannot create a far-future set that pins devices.
 */
export function validateCreateSequence(rawSequence, now = new Date()) {
  const seqNum = Number(rawSequence);
  const dt = sequenceToUtcDate(seqNum);
  if (dt) {
    const maxFutureMs = now.getTime() + MAX_FUTURE_SEQUENCE_HOURS * 60 * 60 * 1000;
    if (dt.getTime() > maxFutureMs) {
      throw new Error(
        `Refusing future sequence ${seqNum}: it is more than ${MAX_FUTURE_SEQUENCE_HOURS} hours ahead of current UTC time (${now.toISOString()}).`,
      );
    }
  }
  return seqNum;
}

/**
 * Derive a strictly monotonic 32-bit Android versionCode from an update sequence.
 * Matches `legion_update::android_version_code` identically.
 */
export function androidVersionCode(rawSequence) {
  const sequence = typeof rawSequence === 'number' ? rawSequence : Number(String(rawSequence).trim());
  const dt = sequenceToUtcDate(sequence);
  if (!dt) {
    return sequence;
  }
  const unixMinutes = Math.floor(dt.getTime() / 60_000);
  if (unixMinutes <= 0 || unixMinutes > 2_100_000_000) {
    throw new Error(`Sequence ${sequence} overflows Android versionCode.`);
  }
  return unixMinutes;
}

/** Derive `<baseVersion>+<sequence>` for desktop bundle metadata. */
export function desktopBundleVersion(baseVersion, rawSequence) {
  const sequence = Number(rawSequence);
  if (!Number.isSafeInteger(sequence) || sequence <= 0) {
    throw new Error(`Invalid sequence ${JSON.stringify(rawSequence)}.`);
  }
  const clean = String(baseVersion || '').trim().split('+', 1)[0];
  if (!clean || !/^[A-Za-z0-9.-]+$/.test(clean)) {
    throw new Error(`Invalid base version ${JSON.stringify(baseVersion)}.`);
  }
  return `${clean}+${sequence}`;
}

/** Derive macOS CFBundleVersion string from sequence. */
export function macosBundleVersion(rawSequence) {
  const sequence = Number(rawSequence);
  if (!Number.isSafeInteger(sequence) || sequence <= 0) {
    throw new Error(`Invalid sequence ${JSON.stringify(rawSequence)}.`);
  }
  return String(sequence);
}

/**
 * Derive a Tauri `--config` JSON object that encodes the build sequence in platform bundle version
 * fields while respecting each bundler's constraints:
 * - Linux uses `<ver>+<seq>` (in `.deb` control `Version:` and desktop metadata).
 * - macOS sets `bundle.macOS.bundleVersion = <seq>` (in `Info.plist` `CFBundleVersion`).
 * - Windows uses `<ver>+seq<seq>` (non-numeric build metadata keeps `VIProductVersion` at
 *   `<major>.<minor>.<patch>.0` without overflowing 16-bit components while embedding the exact
 *   sequence in PE `VS_VERSION_INFO` `FileVersion`/`ProductVersion` as UTF-16LE).
 */
export function tauriBundleConfig({ baseVersion, sequence, os: targetOs }) {
  const clean = String(baseVersion || '').trim().split('+', 1)[0];
  if (!clean || !/^[A-Za-z0-9.-]+$/.test(clean)) {
    throw new Error(`Invalid base version ${JSON.stringify(baseVersion)}.`);
  }
  const seq = Number(sequence);
  if (!Number.isSafeInteger(seq) || seq <= 0) {
    throw new Error(`Invalid sequence ${JSON.stringify(sequence)}.`);
  }
  if (targetOs === 'linux') {
    return { version: desktopBundleVersion(clean, seq) };
  }
  if (targetOs === 'macos') {
    return {
      version: clean,
      bundle: { macOS: { bundleVersion: macosBundleVersion(seq) } },
    };
  }
  if (targetOs === 'windows') {
    return { version: `${clean}+seq${seq}` };
  }
  throw new Error(`Unsupported target OS for tauri-config: ${JSON.stringify(targetOs)} (expected linux, macos, or windows).`);
}

/** Validate a bare filename inside an update set (matching `legion_update::validate_entry_name`). */
export function validateEntryName(name) {
  if (typeof name !== 'string' || name.length === 0 || name.length > 255) {
    throw new Error(`Update file name ${JSON.stringify(name)} is empty or too long.`);
  }
  if (
    name.startsWith('.') ||
    name.includes('..') ||
    name.includes('/') ||
    name.includes('\\') ||
    name.includes(':') ||
    !/^[A-Za-z0-9._+-]+$/.test(name)
  ) {
    throw new Error(`Update file name ${JSON.stringify(name)} is not a valid bare filename.`);
  }
  return name;
}

function isValidArtifactCombo(program, osName, kind) {
  if (program === 'node') {
    if (kind === 'binary') return osName === 'linux' || osName === 'macos' || osName === 'windows';
    if (kind === 'deb') return osName === 'linux';
    return false;
  }
  if (program === 'app') {
    if (kind === 'apk') return osName === 'android';
    if (kind === 'appimage' || kind === 'deb') return osName === 'linux';
    if (kind === 'app-tar') return osName === 'macos';
    if (kind === 'nsis') return osName === 'windows';
    return false;
  }
  return false;
}

/** Extract the raw 32-byte Ed25519 public key and compute `SHA256:<base64-no-pad>`. */
export function publicKeyFingerprint(publicKey) {
  const keyObj = typeof publicKey === 'string' || Buffer.isBuffer(publicKey)
    ? crypto.createPublicKey(publicKey)
    : publicKey;
  if (keyObj.asymmetricKeyType !== 'ed25519') {
    throw new Error('Expected an Ed25519 public key.');
  }
  const der = keyObj.export({ type: 'spki', format: 'der' });
  if (der.length !== 44 || !der.subarray(0, 12).equals(ED25519_SPKI_PREFIX)) {
    throw new Error('Unexpected Ed25519 SPKI DER layout.');
  }
  const raw = der.subarray(12);
  const hash = crypto.createHash('sha256').update(raw).digest('base64').replace(/=+$/, '');
  return `SHA256:${hash}`;
}

function realOrResolved(p) {
  const resolved = path.resolve(p);
  try {
    return fs.realpathSync.native(resolved);
  } catch {
    const parent = path.dirname(resolved);
    try {
      return path.join(fs.realpathSync.native(parent), path.basename(resolved));
    } catch {
      return resolved;
    }
  }
}

function isInsideDir(candidate, dir) {
  const rel = path.relative(realOrResolved(dir), realOrResolved(candidate));
  return rel === '' || (!rel.startsWith('..') && !path.isAbsolute(rel));
}

/**
 * Generate an Ed25519 preview update keypair outside the repository with mode 0600 on the private
 * key. Never prints private key bytes and refuses to overwrite existing keys.
 */
export function createPreviewKey({
  privateKeyPath = DEFAULT_PREVIEW_PRIVATE_KEY,
  publicKeyPath = DEFAULT_PREVIEW_PUBLIC_KEY,
  repoRoot = REPO_ROOT,
} = {}) {
  const privPath = path.resolve(privateKeyPath);
  const pubPath = path.resolve(publicKeyPath);

  if (isInsideDir(privPath, repoRoot) || isInsideDir(pubPath, repoRoot)) {
    throw new Error(
      'Refusing to create a preview update key inside the repository. Keep keys outside the checkout (for example under ~/.legion-control/).',
    );
  }
  if (fs.existsSync(privPath) || fs.existsSync(pubPath)) {
    throw new Error(`Preview key already exists (${privPath} or ${pubPath}); refusing to overwrite.`);
  }

  fs.mkdirSync(path.dirname(privPath), { recursive: true, mode: 0o700 });
  fs.mkdirSync(path.dirname(pubPath), { recursive: true, mode: 0o755 });

  const { privateKey, publicKey } = crypto.generateKeyPairSync('ed25519');
  const privPem = privateKey.export({ type: 'pkcs8', format: 'pem' });
  const pubPem = publicKey.export({ type: 'spki', format: 'pem' });

  fs.writeFileSync(privPath, privPem, { mode: 0o600, flag: 'wx' });
  fs.chmodSync(privPath, 0o600);
  fs.writeFileSync(pubPath, pubPem, { mode: 0o644, flag: 'wx' });

  return {
    privateKeyPath: privPath,
    publicKeyPath: pubPath,
    fingerprint: publicKeyFingerprint(publicKey),
  };
}

function inferArchFromName(lower) {
  if (lower.includes('universal')) return 'universal';
  if (lower.includes('aarch64') || lower.includes('arm64')) return 'aarch64';
  if (lower.includes('x86_64') || lower.includes('amd64') || lower.includes('x64')) return 'x86_64';
  return null;
}

/**
 * Parse an `--artifact` specification:
 * `<filePath>` or `<filePath>:program=app,os=android,arch=universal,kind=apk[,name=override]`.
 */
export function parseArtifactSpec(rawSpec) {
  if (typeof rawSpec !== 'string' || !rawSpec.trim()) {
    throw new Error('Artifact specification cannot be empty.');
  }

  // Split trailing `:key=val,...` metadata while preserving Windows drive letters like `C:\foo`.
  let srcPath = rawSpec;
  let metaText = '';
  const metaMatch = /^(.+):([a-z]+=[^:,]+(?:,[a-z]+=[^:,]+)*)$/.exec(rawSpec);
  if (metaMatch) {
    srcPath = metaMatch[1];
    metaText = metaMatch[2];
  }

  const meta = {};
  if (metaText) {
    for (const part of metaText.split(',')) {
      const eq = part.indexOf('=');
      if (eq <= 0) throw new Error(`Invalid artifact metadata field: ${part}`);
      const key = part.slice(0, eq);
      const val = part.slice(eq + 1);
      if (!['program', 'os', 'arch', 'kind', 'name'].includes(key)) {
        throw new Error(`Unknown artifact field ${JSON.stringify(key)} in ${rawSpec}.`);
      }
      meta[key] = val;
    }
  }

  const resolvedSrc = path.resolve(srcPath);
  const name = meta.name || path.basename(resolvedSrc);
  const lower = name.toLowerCase();

  let program = meta.program || null;
  let osName = meta.os || null;
  let arch = meta.arch || inferArchFromName(lower);
  let kind = meta.kind || null;

  if (!kind) {
    if (lower.endsWith('.apk')) {
      program ||= 'app';
      osName ||= 'android';
      kind = 'apk';
    } else if (lower.endsWith('.appimage')) {
      program ||= 'app';
      osName ||= 'linux';
      kind = 'appimage';
    } else if (lower.endsWith('.app.tar.gz') || lower.endsWith('.app.tar')) {
      program ||= 'app';
      osName ||= 'macos';
      kind = 'app-tar';
    } else if (lower.endsWith('-setup.exe') || (lower.endsWith('.exe') && lower.includes('nsis'))) {
      program ||= 'app';
      osName ||= 'windows';
      kind = 'nsis';
    } else if (lower.endsWith('.deb')) {
      program ||= lower.includes('legion-app') ? 'app' : 'node';
      osName ||= 'linux';
      kind = 'deb';
    } else if (lower.startsWith('legion-')) {
      program ||= 'node';
      kind = 'binary';
      if (lower.includes('linux')) osName ||= 'linux';
      else if (lower.includes('darwin') || lower.includes('macos')) osName ||= 'macos';
      else if (lower.includes('windows') || lower.endsWith('.exe')) osName ||= 'windows';
    }
  }

  validateEntryName(name);
  if (RESERVED_NAMES.has(name)) {
    throw new Error(`Artifact name ${JSON.stringify(name)} is reserved.`);
  }
  if (!VALID_PROGRAMS.has(program)) {
    throw new Error(`Artifact ${name}: missing or invalid program (${program}). Specify program=app or program=node.`);
  }
  if (!VALID_OS.has(osName)) {
    throw new Error(`Artifact ${name}: missing or invalid os (${osName}).`);
  }
  if (!VALID_ARCH.has(arch)) {
    throw new Error(`Artifact ${name}: missing or invalid arch (${arch}).`);
  }
  if (!VALID_KINDS.has(kind)) {
    throw new Error(`Artifact ${name}: missing or invalid kind (${kind}).`);
  }
  if (!isValidArtifactCombo(program, osName, kind)) {
    throw new Error(`Artifact ${name}: unsupported combination program=${program} os=${osName} kind=${kind}.`);
  }

  return { srcPath: resolvedSrc, name, program, os: osName, arch, kind };
}

function sha256FileSync(filePath) {
  const fd = fs.openSync(filePath, 'r');
  try {
    const hash = crypto.createHash('sha256');
    const buf = Buffer.allocUnsafe(64 * 1024);
    while (true) {
      const n = fs.readSync(fd, buf, 0, buf.length, null);
      if (n === 0) break;
      hash.update(buf.subarray(0, n));
    }
    return hash.digest('hex');
  } finally {
    fs.closeSync(fd);
  }
}

// ── Non-executing artifact sequence inspection ──────────────────────────────────────────────

const SEQUENCE_MARKER_PREFIX = Buffer.from('LEGION_UPDATE_SEQUENCE=', 'ascii');

/** Scan a buffer for all `LEGION_UPDATE_SEQUENCE=<digits>\0` markers and return found sequences. */
function findSequenceMarkersInBuffer(buf) {
  const found = new Set();
  let idx = 0;
  while (true) {
    const pos = buf.indexOf(SEQUENCE_MARKER_PREFIX, idx);
    if (pos === -1) break;
    const start = pos + SEQUENCE_MARKER_PREFIX.length;
    let end = start;
    while (end < buf.length && buf[end] >= 0x30 && buf[end] <= 0x39 && end - start < 24) {
      end += 1;
    }
    if (end > start && end < buf.length && buf[end] === 0) {
      found.add(buf.subarray(start, end).toString('ascii'));
    }
    idx = start;
  }
  return [...found];
}

/** Stream-scan a file for `LEGION_UPDATE_SEQUENCE=<digits>\0` markers using bounded memory. */
function findSequenceMarkersInFile(filePath) {
  const fd = fs.openSync(filePath, 'r');
  const found = new Set();
  const chunkSize = 1024 * 1024;
  const overlap = 64;
  let tail = Buffer.alloc(0);
  try {
    const buf = Buffer.allocUnsafe(chunkSize);
    while (true) {
      const n = fs.readSync(fd, buf, 0, chunkSize, null);
      if (n === 0) break;
      const windowBuf = tail.length > 0 ? Buffer.concat([tail, buf.subarray(0, n)]) : buf.subarray(0, n);
      for (const seq of findSequenceMarkersInBuffer(windowBuf)) {
        found.add(seq);
      }
      tail = Buffer.from(windowBuf.subarray(Math.max(0, windowBuf.length - overlap)));
    }
  } finally {
    fs.closeSync(fd);
  }
  return [...found];
}

/** Scan a file for UTF-16LE `+seq<digits>` tokens (embedded by NSIS PE `VS_VERSION_INFO`). */
function findUtf16SeqMarkersInFile(filePath) {
  const prefix = Buffer.from('+seq', 'utf16le');
  const fd = fs.openSync(filePath, 'r');
  const found = new Set();
  const chunkSize = 1024 * 1024;
  const overlap = 128;
  let tail = Buffer.alloc(0);
  try {
    const buf = Buffer.allocUnsafe(chunkSize);
    while (true) {
      const n = fs.readSync(fd, buf, 0, chunkSize, null);
      if (n === 0) break;
      const windowBuf = tail.length > 0 ? Buffer.concat([tail, buf.subarray(0, n)]) : buf.subarray(0, n);
      let idx = 0;
      while (true) {
        const pos = windowBuf.indexOf(prefix, idx);
        if (pos === -1) break;
        const start = pos + prefix.length;
        let end = start;
        let digits = '';
        while (end + 1 < windowBuf.length && digits.length < 24) {
          const lo = windowBuf[end];
          const hi = windowBuf[end + 1];
          if (hi === 0 && lo >= 0x30 && lo <= 0x39) {
            digits += String.fromCharCode(lo);
            end += 2;
          } else {
            break;
          }
        }
        // Require non-digit UTF-16LE boundary (usually NUL 0x00 0x00) after the digits.
        if (digits.length > 0 && end + 1 < windowBuf.length && (windowBuf[end] === 0 && windowBuf[end + 1] === 0)) {
          found.add(digits);
        }
        idx = start;
      }
      tail = Buffer.from(windowBuf.subarray(Math.max(0, windowBuf.length - overlap)));
    }
  } finally {
    fs.closeSync(fd);
  }
  return [...found];
}

/** Extract a single entry by exact path from a ZIP buffer (stored or deflate). */
function readZipEntry(zipBuf, targetName) {
  // Locate End of Central Directory record (0x06054b50) within last 66 KiB.
  const searchStart = Math.max(0, zipBuf.length - 65557);
  let eocd = -1;
  for (let i = zipBuf.length - 22; i >= searchStart; i -= 1) {
    if (zipBuf.readUInt32LE(i) === 0x06054b50) {
      const commentLen = zipBuf.readUInt16LE(i + 20);
      if (i + 22 + commentLen === zipBuf.length) {
        eocd = i;
        break;
      }
    }
  }
  if (eocd === -1) {
    throw new Error('Not a valid ZIP/APK archive (missing End of Central Directory).');
  }
  const entryCount = zipBuf.readUInt16LE(eocd + 10);
  const cdSize = zipBuf.readUInt32LE(eocd + 12);
  const cdOffset = zipBuf.readUInt32LE(eocd + 16);
  let off = cdOffset;
  const cdEnd = cdOffset + cdSize;
  for (let idx = 0; idx < entryCount && off + 46 <= cdEnd; idx += 1) {
    if (zipBuf.readUInt32LE(off) !== 0x02014b50) {
      throw new Error('Corrupt ZIP Central Directory header.');
    }
    const method = zipBuf.readUInt16LE(off + 10);
    const compSize = zipBuf.readUInt32LE(off + 20);
    const nameLen = zipBuf.readUInt16LE(off + 28);
    const extraLen = zipBuf.readUInt16LE(off + 30);
    const commentLen = zipBuf.readUInt16LE(off + 32);
    const localOff = zipBuf.readUInt32LE(off + 42);
    const name = zipBuf.subarray(off + 46, off + 46 + nameLen).toString('utf8');
    off += 46 + nameLen + extraLen + commentLen;
    if (name !== targetName) continue;

    if (localOff + 30 > zipBuf.length || zipBuf.readUInt32LE(localOff) !== 0x04034b50) {
      throw new Error(`Corrupt ZIP local header for ${targetName}.`);
    }
    const lNameLen = zipBuf.readUInt16LE(localOff + 26);
    const lExtraLen = zipBuf.readUInt16LE(localOff + 28);
    const dataStart = localOff + 30 + lNameLen + lExtraLen;
    const raw = zipBuf.subarray(dataStart, dataStart + compSize);
    if (method === 0) return raw;
    if (method === 8) return zlib.inflateRawSync(raw);
    throw new Error(`Unsupported ZIP compression method ${method} for ${targetName}.`);
  }
  return null;
}

/** Parse Android Binary XML (AXML) `AndroidManifest.xml` to extract `<manifest android:versionCode>`. */
function parseApkManifestVersionCode(axml) {
  if (axml.length < 8 || axml.readUInt16LE(0) !== 0x0003) {
    throw new Error('AndroidManifest.xml is not valid Android binary XML (AXML).');
  }
  const strings = [];
  const resIds = [];
  let pos = axml.readUInt16LE(2);
  const totalLen = Math.min(axml.length, axml.readUInt32LE(4));

  while (pos + 8 <= totalLen) {
    const chunkType = axml.readUInt16LE(pos);
    const headerSize = axml.readUInt16LE(pos + 2);
    const chunkSize = axml.readUInt32LE(pos + 4);
    if (chunkSize < 8 || pos + chunkSize > totalLen) break;

    if (chunkType === 0x0001) {
      // RES_STRING_POOL_TYPE
      const stringCount = axml.readUInt32LE(pos + 8);
      const flags = axml.readUInt32LE(pos + 16);
      const stringsStart = axml.readUInt32LE(pos + 20);
      const isUtf8 = (flags & (1 << 8)) !== 0;
      const offsetsBase = pos + headerSize;
      const dataBase = pos + stringsStart;
      for (let i = 0; i < stringCount; i += 1) {
        const strOff = dataBase + axml.readUInt32LE(offsetsBase + i * 4);
        if (isUtf8) {
          let p = strOff;
          // Skip UTF-16 length varint, then read UTF-8 byte length varint.
          if ((axml[p] & 0x80) !== 0) p += 2;
          else p += 1;
          let byteLen = axml[p];
          if ((byteLen & 0x80) !== 0) {
            byteLen = ((byteLen & 0x7f) << 8) | axml[p + 1];
            p += 2;
          } else {
            p += 1;
          }
          strings.push(axml.subarray(p, p + byteLen).toString('utf8'));
        } else {
          let p = strOff;
          let charLen = axml.readUInt16LE(p);
          p += 2;
          if ((charLen & 0x8000) !== 0) {
            charLen = ((charLen & 0x7fff) << 16) | axml.readUInt16LE(p);
            p += 2;
          }
          strings.push(axml.subarray(p, p + charLen * 2).toString('utf16le'));
        }
      }
    } else if (chunkType === 0x0180) {
      // RES_XML_RESOURCE_MAP_TYPE
      const count = Math.floor((chunkSize - headerSize) / 4);
      for (let i = 0; i < count; i += 1) {
        resIds.push(axml.readUInt32LE(pos + headerSize + i * 4));
      }
    } else if (chunkType === 0x0102) {
      // RES_XML_START_ELEMENT_TYPE
      const nameIdx = axml.readInt32LE(pos + headerSize + 4);
      const attrStart = axml.readUInt16LE(pos + headerSize + 8);
      const attrSize = axml.readUInt16LE(pos + headerSize + 10);
      const attrCount = axml.readUInt16LE(pos + headerSize + 12);
      const elemName = strings[nameIdx] || '';
      if (elemName === 'manifest') {
        const attrsBase = pos + headerSize + attrStart;
        for (let i = 0; i < attrCount; i += 1) {
          const a = attrsBase + i * attrSize;
          if (a + 20 > pos + chunkSize) break;
          const attrNameIdx = axml.readInt32LE(a + 4);
          const attrName = strings[attrNameIdx] || '';
          const attrResId = resIds[attrNameIdx] || 0;
          if (attrResId === 0x0101021b || attrName === 'versionCode') {
            return axml.readInt32LE(a + 16);
          }
        }
      }
    }

    pos += chunkSize;
  }
  throw new Error('Could not find android:versionCode inside AndroidManifest.xml.');
}

/** Iterate regular file entries from a POSIX/USTAR/PAX tar buffer. */
function readTarEntries(tarBuf, onEntry) {
  let pos = 0;
  let paxPath = null;
  let gnuLongName = null;
  while (pos + 512 <= tarBuf.length) {
    const hdr = tarBuf.subarray(pos, pos + 512);
    if (hdr.every((b) => b === 0)) break;
    const rawName = hdr.subarray(0, 100).toString('utf8').replace(/\0.*$/, '');
    const prefix = hdr.subarray(345, 500).toString('utf8').replace(/\0.*$/, '');
    const sizeText = hdr.subarray(124, 136).toString('ascii').replace(/\0.*$/, '').trim();
    const size = sizeText ? parseInt(sizeText, 8) : 0;
    const typeFlag = String.fromCharCode(hdr[156]);
    const dataStart = pos + 512;
    const dataEnd = dataStart + size;
    const data = tarBuf.subarray(dataStart, dataEnd);
    pos = dataStart + Math.ceil(size / 512) * 512;

    if (typeFlag === 'x') {
      // PAX extended header: `<len> <key>=<value>\n`
      const text = data.toString('utf8');
      const m = /^\d+ path=(.*)$/m.exec(text);
      if (m) paxPath = m[1];
      continue;
    }
    if (typeFlag === 'L') {
      gnuLongName = data.toString('utf8').replace(/\0.*$/, '');
      continue;
    }

    const fullPath = paxPath || gnuLongName || (prefix ? `${prefix}/${rawName}` : rawName);
    paxPath = null;
    gnuLongName = null;
    if (typeFlag === '0' || typeFlag === '\0') {
      const stop = onEntry(fullPath, data);
      if (stop === true) break;
    }
  }
}

/** Decompress a `.tar`, `.tar.gz`, `.tar.zst`, or `.tar.xz` buffer. */
function decompressTarPayload(name, buf) {
  if (name.endsWith('.gz') || (buf.length >= 2 && buf[0] === 0x1f && buf[1] === 0x8b)) {
    return zlib.gunzipSync(buf);
  }
  if (name.endsWith('.zst') || (buf.length >= 4 && buf.readUInt32LE(0) === 0xfd2fb528)) {
    if (typeof zlib.zstdDecompressSync === 'function') {
      return zlib.zstdDecompressSync(buf);
    }
    return execFileSync('zstd', ['-dc'], { input: buf, maxBuffer: 64 * 1024 * 1024 });
  }
  if (name.endsWith('.xz') || (buf.length >= 6 && buf[0] === 0xfd && buf[1] === 0x37 && buf[2] === 0x7a)) {
    return execFileSync('xz', ['-dc'], { input: buf, maxBuffer: 64 * 1024 * 1024 });
  }
  return buf;
}

/** Parse a Debian `.deb` `ar` archive and return a map of member name -> Buffer. */
function parseArArchive(buf) {
  if (buf.length < 8 || buf.subarray(0, 8).toString('ascii') !== '!<arch>\n') {
    throw new Error('Not a valid Debian .deb (ar) archive.');
  }
  const members = new Map();
  let pos = 8;
  while (pos + 60 <= buf.length) {
    const name = buf.subarray(pos, pos + 16).toString('ascii').trim().replace(/\/$/, '');
    const sizeStr = buf.subarray(pos + 48, pos + 58).toString('ascii').trim();
    const magic = buf.subarray(pos + 58, pos + 60).toString('ascii');
    if (magic !== '`\n') {
      throw new Error(`Corrupt ar member header at offset ${pos}.`);
    }
    const size = parseInt(sizeStr, 10);
    if (!Number.isSafeInteger(size) || size < 0 || pos + 60 + size > buf.length) {
      throw new Error(`Invalid ar member size for ${name}.`);
    }
    members.set(name, buf.subarray(pos + 60, pos + 60 + size));
    pos += 60 + size + (size % 2);
  }
  return members;
}

/** Decompress a single SquashFS block (compression IDs 1=gzip, 4=xz, 6=zstd). */
function decompressSquashfsBlock(compId, raw) {
  if (compId === 1) return zlib.inflateSync(raw);
  if (compId === 6) {
    if (typeof zlib.zstdDecompressSync === 'function') {
      return zlib.zstdDecompressSync(raw);
    }
    return execFileSync('zstd', ['-dc'], { input: raw, maxBuffer: 8 * 1024 * 1024 });
  }
  if (compId === 4) {
    return execFileSync('xz', ['-dc'], { input: raw, maxBuffer: 8 * 1024 * 1024 });
  }
  throw new Error(`Unsupported SquashFS compression id ${compId}.`);
}

/**
 * Minimal pure-Node SquashFS 4.0 reader for inspecting AppImage `.desktop` and `usr/bin/*`
 * files without executing the AppImage.
 */
function inspectAppImageSquashfs(filePath, expectedSeqStr) {
  const fd = fs.openSync(filePath, 'r');
  const fileSize = fs.fstatSync(fd).size;
  const readAt = (offset, length) => {
    const out = Buffer.allocUnsafe(length);
    const n = fs.readSync(fd, out, 0, length, offset);
    return out.subarray(0, n);
  };
  try {
    // Locate SquashFS 4.0 superblock magic `hsqs` (0x73717368): check the exact ELF section-table
    // end first (the AppImage Type 2 spec offset; modern static runtimes exceed 900 KiB), then
    // fall back to scanning up to 4 MiB from the start of the file.
    const isValidSqAt = (offset) => {
      if (offset < 0 || offset + 96 > fileSize) return false;
      const cand = readAt(offset, 96);
      if (cand.length < 96) return false;
      if (cand.subarray(0, 4).toString('ascii') !== 'hsqs') return false;
      const major = cand.readUInt16LE(28);
      const minor = cand.readUInt16LE(30);
      const bSize = cand.readUInt32LE(12);
      const cId = cand.readUInt16LE(20);
      return major === 4 && minor === 0 && bSize >= 4096 && bSize <= 1048576 && cId >= 1 && cId <= 6;
    };

    let sqOff = -1;
    if (fileSize >= 64) {
      const elf = readAt(0, 64);
      if (elf[0] === 0x7f && elf[1] === 0x45 && elf[2] === 0x4c && elf[3] === 0x46) {
        let elfEnd = -1;
        if (elf[4] === 2) {
          // ELF64 little-endian: e_shoff (40), e_shentsize (58), e_shnum (60).
          const shoff = Number(elf.readBigUInt64LE(40));
          const shentsize = elf.readUInt16LE(58);
          const shnum = elf.readUInt16LE(60);
          if (shoff > 0 && shentsize > 0 && shnum > 0) {
            elfEnd = shoff + shentsize * shnum;
          }
        } else if (elf[4] === 1) {
          // ELF32 little-endian: e_shoff (32), e_shentsize (46), e_shnum (48).
          const shoff = elf.readUInt32LE(32);
          const shentsize = elf.readUInt16LE(46);
          const shnum = elf.readUInt16LE(48);
          if (shoff > 0 && shentsize > 0 && shnum > 0) {
            elfEnd = shoff + shentsize * shnum;
          }
        }
        if (isValidSqAt(elfEnd)) {
          sqOff = elfEnd;
        }
      }
    }

    if (sqOff === -1) {
      const probeSize = Math.min(fileSize, 4 * 1024 * 1024);
      const head = readAt(0, probeSize);
      const magic = Buffer.from('hsqs', 'ascii');
      let searchFrom = 0;
      while (true) {
        const idx = head.indexOf(magic, searchFrom);
        if (idx === -1) break;
        if (idx + 96 <= head.length) {
          const major = head.readUInt16LE(idx + 28);
          const minor = head.readUInt16LE(idx + 30);
          const blockSize = head.readUInt32LE(idx + 12);
          const compId = head.readUInt16LE(idx + 20);
          if (major === 4 && minor === 0 && blockSize >= 4096 && blockSize <= 1048576 && compId >= 1 && compId <= 6) {
            sqOff = idx;
            break;
          }
        }
        searchFrom = idx + 4;
      }
    }
    if (sqOff === -1) {
      return null;
    }

    const sb = readAt(sqOff, 96);
    const blockSize = sb.readUInt32LE(12);
    const fragCount = sb.readUInt32LE(16);
    const compId = sb.readUInt16LE(20);
    const rootInodeRef = sb.readBigUInt64LE(32);
    const inodeTableStart = Number(sb.readBigUInt64LE(64));
    const dirTableStart = Number(sb.readBigUInt64LE(72));
    const fragTableStart = Number(sb.readBigUInt64LE(80));

    const metaCache = new Map();
    const readMetaBlock = (absFileOff) => {
      if (metaCache.has(absFileOff)) return metaCache.get(absFileOff);
      const hdr = readAt(absFileOff, 2).readUInt16LE(0);
      const sz = hdr & 0x7fff;
      const uncompressed = (hdr & 0x8000) !== 0;
      const raw = readAt(absFileOff + 2, sz);
      const data = uncompressed ? raw : decompressSquashfsBlock(compId, raw);
      const entry = { data, nextOff: absFileOff + 2 + sz };
      metaCache.set(absFileOff, entry);
      return entry;
    };

    const readMetaStream = (tableStart, blockRelOff, byteOff, totalLen) => {
      const out = Buffer.alloc(totalLen);
      let written = 0;
      let curBlock = sqOff + tableStart + blockRelOff;
      let curOff = byteOff;
      while (written < totalLen) {
        const blk = readMetaBlock(curBlock);
        const avail = blk.data.length - curOff;
        const take = Math.min(avail, totalLen - written);
        blk.data.copy(out, written, curOff, curOff + take);
        written += take;
        curBlock = blk.nextOff;
        curOff = 0;
      }
      return out;
    };

    const readInode = (inodeRef) => {
      const blockOff = Number((inodeRef >> 16n) & 0xffffffffffffn);
      const byteOff = Number(inodeRef & 0xffffn);
      const base = readMetaStream(inodeTableStart, blockOff, byteOff, 64);
      const inodeType = base.readUInt16LE(0);
      if (inodeType === 1) {
        // Basic directory
        return {
          type: 'dir',
          blockIndex: base.readUInt32LE(16),
          fileSize: base.readUInt16LE(24),
          blockOffset: base.readUInt16LE(26),
        };
      }
      if (inodeType === 8) {
        // Extended directory
        return {
          type: 'dir',
          fileSize: base.readUInt32LE(20),
          blockIndex: base.readUInt32LE(24),
          blockOffset: base.readUInt16LE(34),
        };
      }
      if (inodeType === 2) {
        // Basic regular file
        const blocksStart = base.readUInt32LE(16);
        const fragIndex = base.readUInt32LE(20);
        const fragOffset = base.readUInt32LE(24);
        const fileLen = base.readUInt32LE(28);
        const hasFrag = fragIndex !== 0xffffffff;
        const numBlocks = hasFrag ? Math.floor(fileLen / blockSize) : Math.ceil(fileLen / blockSize);
        const full = readMetaStream(inodeTableStart, blockOff, byteOff, 32 + numBlocks * 4);
        const blockSizes = [];
        for (let i = 0; i < numBlocks; i += 1) {
          blockSizes.push(full.readUInt32LE(32 + i * 4));
        }
        return { type: 'file', blocksStart, fragIndex, fragOffset, fileSize: fileLen, blockSizes };
      }
      if (inodeType === 9) {
        // Extended regular file
        const blocksStart = Number(base.readBigUInt64LE(16));
        const fileLen = Number(base.readBigUInt64LE(24));
        const fragIndex = base.readUInt32LE(44);
        const fragOffset = base.readUInt32LE(48);
        const hasFrag = fragIndex !== 0xffffffff;
        const numBlocks = hasFrag ? Math.floor(fileLen / blockSize) : Math.ceil(fileLen / blockSize);
        const full = readMetaStream(inodeTableStart, blockOff, byteOff, 56 + numBlocks * 4);
        const blockSizes = [];
        for (let i = 0; i < numBlocks; i += 1) {
          blockSizes.push(full.readUInt32LE(56 + i * 4));
        }
        return { type: 'file', blocksStart, fragIndex, fragOffset, fileSize: fileLen, blockSizes };
      }
      return { type: 'other', inodeType };
    };

    const listDir = (dirInode) => {
      const targetLen = Math.max(0, dirInode.fileSize - 3);
      if (targetLen === 0) return [];
      const stream = readMetaStream(dirTableStart, dirInode.blockIndex, dirInode.blockOffset, targetLen);
      const items = [];
      let p = 0;
      while (p + 12 <= stream.length) {
        const count = stream.readUInt32LE(p) + 1;
        const startBlock = BigInt(stream.readUInt32LE(p + 4));
        p += 12;
        for (let i = 0; i < count && p + 8 <= stream.length; i += 1) {
          const offset = BigInt(stream.readUInt16LE(p));
          const type = stream.readUInt16LE(p + 4);
          const nameLen = stream.readUInt16LE(p + 6) + 1;
          const name = stream.subarray(p + 8, p + 8 + nameLen).toString('utf8');
          p += 8 + nameLen;
          items.push({ name, type, inodeRef: (startBlock << 16n) | offset });
        }
      }
      return items;
    };

    const readFileContent = (fileInode, maxBytes = 64 * 1024 * 1024) => {
      if (fileInode.fileSize > maxBytes) {
        throw new Error(`SquashFS file exceeds ${maxBytes}-byte inspection limit.`);
      }
      const chunks = [];
      let curPos = sqOff + fileInode.blocksStart;
      let remaining = fileInode.fileSize;
      for (const spec of fileInode.blockSizes) {
        const diskSize = spec & 0x00ffffff;
        const uncompressed = (spec & 0x01000000) !== 0;
        const expectOut = Math.min(blockSize, remaining);
        if (diskSize === 0) {
          chunks.push(Buffer.alloc(expectOut));
        } else {
          const raw = readAt(curPos, diskSize);
          curPos += diskSize;
          chunks.push(uncompressed ? raw : decompressSquashfsBlock(compId, raw));
        }
        remaining -= expectOut;
      }
      if (remaining > 0 && fileInode.fragIndex !== 0xffffffff && fileInode.fragIndex < fragCount) {
        const entriesPerBlock = 512;
        const ptrIdx = Math.floor(fileInode.fragIndex / entriesPerBlock);
        const entryIdx = fileInode.fragIndex % entriesPerBlock;
        const fragMetaOff = Number(readAt(sqOff + fragTableStart + ptrIdx * 8, 8).readBigUInt64LE(0));
        const fragMeta = readMetaBlock(sqOff + fragMetaOff).data;
        const eOff = entryIdx * 16;
        const fragStart = Number(fragMeta.readBigUInt64LE(eOff));
        const fragSpec = fragMeta.readUInt32LE(eOff + 8);
        const fragDiskSize = fragSpec & 0x00ffffff;
        const fragUncompressed = (fragSpec & 0x01000000) !== 0;
        const raw = readAt(sqOff + fragStart, fragDiskSize);
        const fragBuf = fragUncompressed ? raw : decompressSquashfsBlock(compId, raw);
        chunks.push(fragBuf.subarray(fileInode.fragOffset, fileInode.fragOffset + remaining));
      }
      return Buffer.concat(chunks).subarray(0, fileInode.fileSize);
    };

    const resolvePath = (parts) => {
      let cur = readInode(rootInodeRef);
      for (const part of parts) {
        if (cur.type !== 'dir') return null;
        const child = listDir(cur).find((e) => e.name === part);
        if (!child) return null;
        cur = readInode(child.inodeRef);
      }
      return cur;
    };

    const foundSequences = new Set();
    // 1. Check compiled binary marker inside usr/bin/*
    const usrBin = resolvePath(['usr', 'bin']);
    if (usrBin && usrBin.type === 'dir') {
      for (const item of listDir(usrBin)) {
        const node = readInode(item.inodeRef);
        if (node.type === 'file') {
          const bytes = readFileContent(node, 128 * 1024 * 1024);
          for (const s of findSequenceMarkersInBuffer(bytes)) {
            foundSequences.add(s);
          }
        }
      }
    }
    // 2. Also check X-AppImage-Version inside usr/share/applications/*.desktop or root *.desktop
    const checkDesktopDir = (dirNode) => {
      if (!dirNode || dirNode.type !== 'dir') return;
      for (const item of listDir(dirNode)) {
        if (!item.name.endsWith('.desktop')) continue;
        const node = readInode(item.inodeRef);
        if (node.type === 'file' && node.fileSize <= 64 * 1024) {
          const text = readFileContent(node, 64 * 1024).toString('utf8');
          const m = /^X-AppImage-Version=.*\+(\d+)\s*$/m.exec(text);
          if (m) foundSequences.add(m[1]);
        }
      }
    };
    checkDesktopDir(readInode(rootInodeRef));
    checkDesktopDir(resolvePath(['usr', 'share', 'applications']));

    if (foundSequences.has(expectedSeqStr)) {
      return { matched: true, found: [...foundSequences] };
    }
    return { matched: false, found: [...foundSequences] };
  } finally {
    fs.closeSync(fd);
  }
}

/**
 * Inspect an artifact file without executing it and verify that the build sequence embedded inside
 * the artifact matches `sequence`. Throws on mismatch or missing embedded sequence.
 */
export function verifyArtifactSequence(filePath, spec, sequence) {
  const expectedStr = String(sequence);
  const label = `${spec.name} (${spec.kind})`;

  if (spec.kind === 'apk') {
    const zipBuf = fs.readFileSync(filePath);
    const manifestAxml = readZipEntry(zipBuf, 'AndroidManifest.xml');
    if (!manifestAxml) {
      throw new Error(`Artifact ${label} is missing AndroidManifest.xml.`);
    }
    const actualCode = parseApkManifestVersionCode(manifestAxml);
    const expectedCode = androidVersionCode(sequence);
    if (actualCode !== expectedCode) {
      throw new Error(
        `Artifact ${label} has Android versionCode ${actualCode}, expected ${expectedCode} for sequence ${sequence}.`,
      );
    }
    return;
  }

  if (spec.kind === 'binary') {
    const found = findSequenceMarkersInFile(filePath);
    if (!found.includes(expectedStr)) {
      const detail = found.length > 0 ? `found ${found.join(', ')}` : 'no LEGION_UPDATE_SEQUENCE marker found';
      throw new Error(
        `Artifact ${label} does not match sequence ${sequence} (${detail}). Build with LEGION_BUILD_SEQUENCE=${sequence}.`,
      );
    }
    return;
  }

  if (spec.kind === 'deb') {
    const buf = fs.readFileSync(filePath);
    const members = parseArArchive(buf);
    const controlName = [...members.keys()].find((k) => k === 'control.tar' || k.startsWith('control.tar.'));
    if (!controlName) {
      throw new Error(`Artifact ${label} is missing control.tar.* inside the .deb archive.`);
    }
    const controlTar = decompressTarPayload(controlName, members.get(controlName));
    let controlVersion = null;
    readTarEntries(controlTar, (entryName, data) => {
      const clean = entryName.replace(/^\.\//, '');
      if (clean === 'control') {
        const m = /^Version:\s*(\S+)/m.exec(data.toString('utf8'));
        if (m) controlVersion = m[1];
        return true;
      }
      return false;
    });
    if (controlVersion) {
      const seqMatch = /\+(\d+)(?:-[A-Za-z0-9.+~]+)?$/.exec(controlVersion);
      if (seqMatch) {
        if (seqMatch[1] !== expectedStr) {
          throw new Error(
            `Artifact ${label} declares Debian Version ${controlVersion} (sequence ${seqMatch[1]}), expected sequence ${sequence}.`,
          );
        }
        return;
      }
    }
    // Node .deb packages use Version: <semver>-1 without +<seq>; check the compiled binary marker
    // inside data.tar.* instead.
    const dataName = [...members.keys()].find((k) => k === 'data.tar' || k.startsWith('data.tar.'));
    if (dataName) {
      const dataTar = decompressTarPayload(dataName, members.get(dataName));
      const found = new Set();
      readTarEntries(dataTar, (entryName, data) => {
        const clean = entryName.replace(/^\.\//, '');
        if (clean.startsWith('usr/bin/')) {
          for (const seq of findSequenceMarkersInBuffer(data)) {
            found.add(seq);
          }
        }
        return false;
      });
      if (found.has(expectedStr)) {
        return;
      }
      const detail = found.size > 0
        ? `found binary sequence ${[...found].join(', ')}`
        : `control Version is ${controlVersion || 'missing'} and no binary LEGION_UPDATE_SEQUENCE marker was found`;
      throw new Error(`Artifact ${label} does not match sequence ${sequence} (${detail}).`);
    }
    throw new Error(
      `Artifact ${label} does not carry sequence ${sequence} in Debian control Version (${controlVersion || 'missing'}).`,
    );
  }

  if (spec.kind === 'app-tar') {
    const raw = fs.readFileSync(filePath);
    const tarBuf = decompressTarPayload(spec.name, raw);
    let plistVersion = null;
    readTarEntries(tarBuf, (entryName, data) => {
      const clean = entryName.replace(/^\.\//, '');
      if (/^[^/]+\.app\/Contents\/Info\.plist$/.test(clean)) {
        const text = data.toString('utf8');
        const m = /<key>CFBundleVersion<\/key>\s*<string>([^<]+)<\/string>/.exec(text);
        if (m) plistVersion = m[1].trim();
        return true;
      }
      return false;
    });
    if (plistVersion !== expectedStr) {
      throw new Error(
        `Artifact ${label} has Info.plist CFBundleVersion ${JSON.stringify(plistVersion)}, expected ${JSON.stringify(expectedStr)}.`,
      );
    }
    return;
  }

  if (spec.kind === 'nsis') {
    const utf16Found = findUtf16SeqMarkersInFile(filePath);
    if (utf16Found.includes(expectedStr)) {
      return;
    }
    const asciiFound = findSequenceMarkersInFile(filePath);
    if (asciiFound.includes(expectedStr)) {
      return;
    }
    const allFound = [...new Set([...utf16Found, ...asciiFound])];
    const detail = allFound.length > 0
      ? `found ${allFound.join(', ')}`
      : 'no +seq<sequence> PE version string or LEGION_UPDATE_SEQUENCE marker found';
    throw new Error(
      `Artifact ${label} does not match sequence ${sequence} (${detail}). Build with tauri-config --os windows --sequence ${sequence}.`,
    );
  }

  if (spec.kind === 'appimage') {
    const sq = inspectAppImageSquashfs(filePath, expectedStr);
    if (sq) {
      if (sq.matched) return;
      const detail = sq.found.length > 0
        ? `found ${sq.found.join(', ')} inside SquashFS`
        : 'no matching binary LEGION_UPDATE_SEQUENCE marker or X-AppImage-Version inside SquashFS';
      throw new Error(`Artifact ${label} does not match sequence ${sequence} (${detail}).`);
    }
    const rawFound = findSequenceMarkersInFile(filePath);
    if (rawFound.includes(expectedStr)) {
      return;
    }
    const detail = rawFound.length > 0
      ? `found ${rawFound.join(', ')}`
      : 'no SquashFS image or LEGION_UPDATE_SEQUENCE marker found';
    throw new Error(`Artifact ${label} does not match sequence ${sequence} (${detail}).`);
  }

  throw new Error(`Unsupported artifact kind ${spec.kind} for sequence verification.`);
}

/** Assemble and sign a Legion update set directory. */
export function makeUpdateSet({
  outDir,
  channel,
  version,
  build,
  sequence = currentSequence(),
  publishedAt = new Date().toISOString().replace(/\.\d{3}Z$/, 'Z'),
  notes = '',
  privateKeyPath,
  publicKeyPath = null,
  artifacts = [],
  force = false,
  now = new Date(),
} = {}) {
  if (!outDir) throw new Error('--out is required.');
  if (!VALID_CHANNELS.has(channel)) {
    throw new Error(`Invalid channel ${JSON.stringify(channel)}: must be "stable" or "preview".`);
  }
  const cleanVersion = String(version || '').trim();
  if (!cleanVersion || cleanVersion.includes('..') || !/^[A-Za-z0-9._+-]+$/.test(cleanVersion)) {
    throw new Error(`Invalid version ${JSON.stringify(version)}.`);
  }
  const cleanBuild = String(build || '').trim();
  if (!cleanBuild) {
    throw new Error('--build is required.');
  }
  const seqNum = validateCreateSequence(sequence, now);
  if (!privateKeyPath) {
    throw new Error('--key is required.');
  }
  if (!Array.isArray(artifacts) || artifacts.length === 0) {
    throw new Error('At least one --artifact is required.');
  }

  const parsedArtifacts = artifacts.map((item) =>
    typeof item === 'string' ? parseArtifactSpec(item) : item,
  );
  const seenNames = new Set();
  const artifactSizes = new Map();
  for (const art of parsedArtifacts) {
    validateEntryName(art.name);
    if (RESERVED_NAMES.has(art.name)) {
      throw new Error(`Artifact name ${art.name} is reserved.`);
    }
    if (seenNames.has(art.name)) {
      throw new Error(`Duplicate artifact name: ${art.name}`);
    }
    seenNames.add(art.name);
    const st = fs.statSync(art.srcPath);
    if (!st.isFile()) {
      throw new Error(`Artifact source is not a regular file: ${art.srcPath}`);
    }
    if (st.size <= 0 || st.size > MAX_ARTIFACT_BYTES) {
      throw new Error(`Artifact ${art.name} has invalid size ${st.size} bytes (must be 1..=${MAX_ARTIFACT_BYTES}).`);
    }
    // Verify that the artifact's embedded build sequence matches the update set's sequence
    // without executing the artifact.
    verifyArtifactSequence(art.srcPath, art, seqNum);
    artifactSizes.set(art.name, st.size);
  }

  const privateKey = crypto.createPrivateKey(fs.readFileSync(privateKeyPath));
  if (privateKey.asymmetricKeyType !== 'ed25519') {
    throw new Error('Update set signing requires an Ed25519 private key.');
  }
  const derivedPub = crypto.createPublicKey(privateKey);
  if (publicKeyPath) {
    const expectedPub = crypto.createPublicKey(fs.readFileSync(publicKeyPath));
    if (
      expectedPub.asymmetricKeyType !== 'ed25519' ||
      publicKeyFingerprint(expectedPub) !== publicKeyFingerprint(derivedPub)
    ) {
      throw new Error('Private key does not match the supplied --public-key.');
    }
  }

  const destDir = path.resolve(outDir);
  if (fs.existsSync(destDir)) {
    const existing = fs.readdirSync(destDir);
    if (existing.length > 0 && !force) {
      throw new Error(`Output directory ${destDir} is not empty (pass --force to overwrite).`);
    }
  }
  fs.mkdirSync(destDir, { recursive: true });

  const sumRows = [];
  for (const art of parsedArtifacts) {
    const dst = path.join(destDir, art.name);
    if (realOrResolved(art.srcPath) !== realOrResolved(dst)) {
      fs.copyFileSync(art.srcPath, dst);
    }
    sumRows.push({ name: art.name, sha256: sha256FileSync(dst) });
  }

  const manifest = {
    channel,
    version: cleanVersion,
    build: cleanBuild,
    sequence: seqNum,
    published_at: publishedAt,
    notes: String(notes ?? ''),
    artifacts: parsedArtifacts.map(({ name, program, os: osName, arch, kind }) => ({
      name,
      program,
      os: osName,
      arch,
      kind,
      size: artifactSizes.get(name),
    })),
  };
  const manifestPath = path.join(destDir, 'update.json');
  const manifestBytes = Buffer.from(`${JSON.stringify(manifest, null, 2)}\n`, 'utf8');
  fs.writeFileSync(manifestPath, manifestBytes, { mode: 0o644 });
  sumRows.push({
    name: 'update.json',
    sha256: crypto.createHash('sha256').update(manifestBytes).digest('hex'),
  });

  // Sort deterministically by C/ASCII byte order.
  sumRows.sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
  const sumsText = sumRows.map((row) => `${row.sha256}  ${row.name}\n`).join('');
  const sumsBytes = Buffer.from(sumsText, 'utf8');
  fs.writeFileSync(path.join(destDir, 'SHA256SUMS'), sumsBytes, { mode: 0o644 });

  const signature = crypto.sign(null, sumsBytes, privateKey);
  const sigB64 = signature.toString('base64');
  if (!SIGNATURE_PATTERN.test(sigB64) || !crypto.verify(null, sumsBytes, derivedPub, signature)) {
    throw new Error('Generated Ed25519 signature failed self-verification.');
  }
  fs.writeFileSync(path.join(destDir, 'SHA256SUMS.sig'), `${sigB64}\n`, { mode: 0o644 });

  return {
    outDir: destDir,
    manifest,
    entries: sumRows,
    fingerprint: publicKeyFingerprint(derivedPub),
  };
}

/** Verify a signed update set directory against a public key PEM file. */
export function verifyUpdateSet({ dir, publicKeyPath }) {
  const targetDir = path.resolve(dir);
  const publicKey = crypto.createPublicKey(fs.readFileSync(publicKeyPath));
  if (publicKey.asymmetricKeyType !== 'ed25519') {
    throw new Error('Verification requires an Ed25519 public key.');
  }

  const sumsBytes = fs.readFileSync(path.join(targetDir, 'SHA256SUMS'));
  const sigText = fs.readFileSync(path.join(targetDir, 'SHA256SUMS.sig'), 'utf8').trim();
  if (!SIGNATURE_PATTERN.test(sigText)) {
    throw new Error('SHA256SUMS.sig is not a valid base64 Ed25519 signature.');
  }
  const sigBytes = Buffer.from(sigText, 'base64');
  if (!crypto.verify(null, sumsBytes, publicKey, sigBytes)) {
    throw new Error('SHA256SUMS.sig does not match the provided Ed25519 public key.');
  }

  const entries = new Map();
  for (const rawLine of sumsBytes.toString('utf8').split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line || line.startsWith('#')) continue;
    const match = /^([0-9a-fA-F]{64})\s+\*?(.+)$/.exec(line);
    if (!match) throw new Error(`Malformed line in SHA256SUMS: ${JSON.stringify(rawLine)}`);
    const name = validateEntryName(match[2].trim());
    if (entries.has(name)) throw new Error(`Duplicate entry in SHA256SUMS: ${name}`);
    entries.set(name, match[1].toLowerCase());
  }
  if (!entries.has('update.json')) {
    throw new Error('SHA256SUMS does not list update.json.');
  }

  for (const [name, expected] of entries) {
    const filePath = path.join(targetDir, name);
    const st = fs.lstatSync(filePath);
    if (!st.isFile()) throw new Error(`${name} is not a regular file.`);
    const actual = sha256FileSync(filePath);
    if (actual !== expected) {
      throw new Error(`SHA-256 mismatch for ${name}: expected ${expected}, got ${actual}.`);
    }
  }

  const manifest = JSON.parse(fs.readFileSync(path.join(targetDir, 'update.json'), 'utf8'));
  for (const art of manifest.artifacts || []) {
    if (!entries.has(art.name)) {
      throw new Error(`Artifact ${art.name} from update.json is not listed in SHA256SUMS.`);
    }
    const filePath = path.join(targetDir, art.name);
    const st = fs.lstatSync(filePath);
    if (!Number.isSafeInteger(art.size) || art.size <= 0 || art.size !== st.size) {
      throw new Error(`Size mismatch for ${art.name}: manifest declares ${art.size}, file is ${st.size} bytes.`);
    }
    verifyArtifactSequence(filePath, art, manifest.sequence);
  }

  return { manifest, fingerprint: publicKeyFingerprint(publicKey), count: entries.size };
}

function usage() {
  return [
    'Usage:',
    '  node scripts/make-update-set.mjs keygen [--key PATH] [--public-key PATH]',
    '  node scripts/make-update-set.mjs sequence',
    '  node scripts/make-update-set.mjs version-code <sequence>',
    '  node scripts/make-update-set.mjs tauri-config --os <linux|macos|windows> --version VER --sequence SEQ',
    '  node scripts/make-update-set.mjs create --out DIR --channel <preview|stable> --version VER --build REV [--sequence SEQ] [--notes TEXT | --notes-file PATH] --key KEY [--public-key PUB] --artifact SPEC [--artifact SPEC ...] [--force]',
    '  node scripts/make-update-set.mjs verify --dir DIR --public-key PUB',
  ].join('\n');
}

function runCli(argv) {
  const [cmd, ...rest] = argv;
  if (!cmd || cmd === '-h' || cmd === '--help') {
    console.log(usage());
    return;
  }
  if (cmd === 'sequence') {
    console.log(String(currentSequence()));
    return;
  }
  if (cmd === 'version-code') {
    const seq = rest[0];
    if (!seq) throw new Error('version-code requires <sequence>.');
    console.log(String(androidVersionCode(seq)));
    return;
  }
  if (cmd === 'tauri-config') {
    let targetOs;
    let baseVersion;
    let sequence;
    for (let i = 0; i < rest.length; i += 1) {
      if (rest[i] === '--os') targetOs = rest[++i];
      else if (rest[i] === '--version') baseVersion = rest[++i];
      else if (rest[i] === '--sequence') sequence = rest[++i];
      else throw new Error(`Unknown option: ${rest[i]}`);
    }
    console.log(JSON.stringify(tauriBundleConfig({ baseVersion, sequence, os: targetOs })));
    return;
  }
  if (cmd === 'keygen') {
    let privateKeyPath = DEFAULT_PREVIEW_PRIVATE_KEY;
    let publicKeyPath = DEFAULT_PREVIEW_PUBLIC_KEY;
    for (let i = 0; i < rest.length; i += 1) {
      if (rest[i] === '--key') privateKeyPath = rest[++i];
      else if (rest[i] === '--public-key') publicKeyPath = rest[++i];
      else throw new Error(`Unknown option: ${rest[i]}`);
    }
    const created = createPreviewKey({ privateKeyPath, publicKeyPath });
    console.log(`Created Ed25519 preview update keypair:`);
    console.log(`  private key (mode 600): ${created.privateKeyPath}`);
    console.log(`  public key:             ${created.publicKeyPath}`);
    console.log(`  fingerprint:            ${created.fingerprint}`);
    console.log('');
    console.log('To build a preview binary or app that trusts this key:');
    console.log(`  export LEGION_PREVIEW_UPDATE_KEY=${JSON.stringify(created.publicKeyPath)}`);
    return;
  }
  if (cmd === 'create') {
    const opts = { artifacts: [], force: false };
    for (let i = 0; i < rest.length; i += 1) {
      const arg = rest[i];
      if (arg === '--out') opts.outDir = rest[++i];
      else if (arg === '--channel') opts.channel = rest[++i];
      else if (arg === '--version') opts.version = rest[++i];
      else if (arg === '--build') opts.build = rest[++i];
      else if (arg === '--sequence') opts.sequence = Number(rest[++i]);
      else if (arg === '--published-at') opts.publishedAt = rest[++i];
      else if (arg === '--notes') opts.notes = rest[++i];
      else if (arg === '--notes-file') opts.notes = fs.readFileSync(rest[++i], 'utf8').trim();
      else if (arg === '--key') opts.privateKeyPath = rest[++i];
      else if (arg === '--public-key') opts.publicKeyPath = rest[++i];
      else if (arg === '--artifact') opts.artifacts.push(rest[++i]);
      else if (arg === '--force') opts.force = true;
      else throw new Error(`Unknown option: ${arg}`);
    }
    const res = makeUpdateSet(opts);
    console.log(`Created signed ${res.manifest.channel} update set at ${res.outDir}`);
    console.log(`  version:     ${res.manifest.version} (${res.manifest.build})`);
    console.log(`  sequence:    ${res.manifest.sequence}`);
    console.log(`  signer:      ${res.fingerprint}`);
    for (const row of res.entries) {
      console.log(`  ${row.sha256}  ${row.name}`);
    }
    return;
  }
  if (cmd === 'verify') {
    let dir;
    let publicKeyPath;
    for (let i = 0; i < rest.length; i += 1) {
      if (rest[i] === '--dir') dir = rest[++i];
      else if (rest[i] === '--public-key') publicKeyPath = rest[++i];
      else throw new Error(`Unknown option: ${rest[i]}`);
    }
    if (!dir || !publicKeyPath) throw new Error('verify requires --dir and --public-key.');
    const res = verifyUpdateSet({ dir, publicKeyPath });
    console.log(
      `Verified ${res.manifest.channel} update set ${res.manifest.version} (sequence ${res.manifest.sequence}, ${res.count} files, signer ${res.fingerprint}).`,
    );
    return;
  }
  throw new Error(`Unknown command: ${cmd}\n${usage()}`);
}

const sameFile = (a, b) =>
  process.platform === 'win32'
    ? realOrResolved(a).toLowerCase() === realOrResolved(b).toLowerCase()
    : realOrResolved(a) === realOrResolved(b);
if (process.argv[1] && sameFile(process.argv[1], fileURLToPath(import.meta.url))) {
  try {
    runCli(process.argv.slice(2));
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
