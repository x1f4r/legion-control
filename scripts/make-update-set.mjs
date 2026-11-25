#!/usr/bin/env node
// Assemble and sign a Legion update set, generate a preview Ed25519 update key outside the
// repository, and derive platform version numbers from a build sequence.
//
// Usage:
//   node scripts/make-update-set.mjs keygen [--key PATH] [--public-key PATH]
//   node scripts/make-update-set.mjs sequence
//   node scripts/make-update-set.mjs version-code <sequence>
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
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

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

/**
 * Derive a strictly monotonic 32-bit Android versionCode from an update sequence.
 * Matches `legion_update::android_version_code` identically.
 */
export function androidVersionCode(rawSequence) {
  const sequence = typeof rawSequence === 'number' ? rawSequence : Number(String(rawSequence).trim());
  if (!Number.isSafeInteger(sequence) || sequence <= 0) {
    throw new Error(`Invalid sequence ${JSON.stringify(rawSequence)}: must be a positive integer.`);
  }
  if (sequence <= MAX_SMALL_SEQUENCE) {
    return sequence;
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
    const unixMinutes = Math.floor(dt.getTime() / 60_000);
    if (unixMinutes <= 0 || unixMinutes > 2_100_000_000) {
      throw new Error(`Sequence ${sequence} overflows Android versionCode.`);
    }
    return unixMinutes;
  }
  throw new Error(
    `Sequence ${sequence} cannot be mapped to Android versionCode: pass a UTC YYYYMMDDHHMM timestamp or a small test sequence in 1..=${MAX_SMALL_SEQUENCE}.`,
  );
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
  const seqNum = Number(sequence);
  if (!Number.isSafeInteger(seqNum) || seqNum <= 0) {
    throw new Error(`Invalid sequence ${JSON.stringify(sequence)}: must be a positive integer.`);
  }
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
  }

  return { manifest, fingerprint: publicKeyFingerprint(publicKey), count: entries.size };
}

function usage() {
  return [
    'Usage:',
    '  node scripts/make-update-set.mjs keygen [--key PATH] [--public-key PATH]',
    '  node scripts/make-update-set.mjs sequence',
    '  node scripts/make-update-set.mjs version-code <sequence>',
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
