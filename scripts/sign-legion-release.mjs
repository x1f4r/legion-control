#!/usr/bin/env node
// Download a draft Legion release's SHA256SUMS, display every artifact and digest, sign the exact
// bytes offline with the owner's Ed25519 release key, upload SHA256SUMS.sig to the draft release,
// and print the exact publish command (which keeps --latest=false so older app checks stay intact).
//
//   node scripts/sign-legion-release.mjs <legion-vX.Y.Z> [--key PATH] [--public-key PATH] [--repo OWNER/REPO]
//
// Uses the same key locations as scripts/sign-release.mjs:
//   private key: ~/.legion-control/release-signing.key
//   public key:  contract/release-public-key.pem
import { execFileSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const DEFAULT_PRIVATE_KEY = path.join(os.homedir(), '.legion-control/release-signing.key');
const DEFAULT_PUBLIC_KEY = path.join(root, 'contract/release-public-key.pem');
const TAG_PATTERN = /^legion-v([0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?)$/;
const SIGNATURE_PATTERN = /^[A-Za-z0-9+/]{86}==$/;
const MAX_SUMS_BYTES = 256 * 1024;

/** Parse SHA256SUMS text into `{ sha256, name }[]`, refusing malformed or empty files. */
export function parseSha256Sums(text) {
  const entries = [];
  for (const rawLine of text.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line || line.startsWith('#')) continue;
    const match = /^([0-9a-fA-F]{64})\s+\*?(.+)$/.exec(line);
    if (!match) throw new Error(`Malformed line in SHA256SUMS: ${JSON.stringify(rawLine)}`);
    entries.push({ sha256: match[1].toLowerCase(), name: match[2].trim() });
  }
  if (entries.length === 0) throw new Error('SHA256SUMS contains no artifact entries.');
  return entries;
}

/** Sign exact SHA256SUMS bytes with the Ed25519 private key and immediately verify with publicKey. */
export function signSha256SumsBytes(bytes, privateKeyPath, publicKeyPath) {
  if (!Buffer.isBuffer(bytes) || bytes.length === 0 || bytes.length > MAX_SUMS_BYTES) {
    throw new Error('SHA256SUMS is empty or exceeds 256 KiB.');
  }
  const privateKey = crypto.createPrivateKey(fs.readFileSync(privateKeyPath));
  if (privateKey.asymmetricKeyType !== 'ed25519') {
    throw new Error('Release signing requires an Ed25519 private key.');
  }
  const publicKey = crypto.createPublicKey(fs.readFileSync(publicKeyPath));
  if (publicKey.asymmetricKeyType !== 'ed25519') {
    throw new Error('Pinned public key is not an Ed25519 key.');
  }
  const signature = crypto.sign(null, bytes, privateKey);
  const encoded = signature.toString('base64');
  if (!SIGNATURE_PATTERN.test(encoded)) {
    throw new Error('Generated Ed25519 signature has an unexpected length.');
  }
  if (!crypto.verify(null, bytes, publicKey, signature)) {
    throw new Error('Private key does not match the pinned public key in contract/release-public-key.pem.');
  }
  return `${encoded}\n`;
}

/**
 * Download `SHA256SUMS` from draft release `tag`, sign it, verify against `publicKey`, upload
 * `SHA256SUMS.sig`, and return the release summary plus the exact publish command.
 */
export function signLegionRelease({
  tag,
  privateKey = DEFAULT_PRIVATE_KEY,
  publicKey = DEFAULT_PUBLIC_KEY,
  repo,
  gh = 'gh',
  log = (line) => console.log(line),
} = {}) {
  const tagMatch = TAG_PATTERN.exec(tag ?? '');
  if (!tagMatch) {
    throw new Error('Usage: node scripts/sign-legion-release.mjs <legion-vX.Y.Z> [--key PATH] [--public-key PATH] [--repo OWNER/REPO]');
  }
  const version = tagMatch[1];
  const repoArgs = repo ? ['--repo', repo] : [];
  const runGh = (args) => execFileSync(gh, [...args, ...repoArgs], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });

  const meta = JSON.parse(runGh(['release', 'view', tag, '--json', 'tagName,isDraft,isPrerelease,assets']));
  if (!meta.isDraft) {
    throw new Error(`Release ${tag} is not a draft; refusing to sign an already-published release.`);
  }
  const hasSums = Array.isArray(meta.assets) && meta.assets.some((a) => a.name === 'SHA256SUMS');
  if (!hasSums) {
    throw new Error(`Draft release ${tag} has no SHA256SUMS asset.`);
  }

  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-sign-'));
  try {
    runGh(['release', 'download', tag, '--pattern', 'SHA256SUMS', '--dir', tmpDir]);
    const sumsPath = path.join(tmpDir, 'SHA256SUMS');
    const sumsBytes = fs.readFileSync(sumsPath);
    const entries = parseSha256Sums(sumsBytes.toString('utf8'));

    log(`Signing Legion release ${tag} (version ${version}):`);
    for (const entry of entries) {
      log(`  ${entry.sha256}  ${entry.name}`);
    }

    const sigText = signSha256SumsBytes(sumsBytes, privateKey, publicKey);
    const sigPath = path.join(tmpDir, 'SHA256SUMS.sig');
    fs.writeFileSync(sigPath, sigText, { mode: 0o644 });

    runGh(['release', 'upload', tag, sigPath, '--clobber']);

    const publishCommand = repo
      ? `gh release edit ${tag} --repo ${repo} --draft=false --latest=false`
      : `gh release edit ${tag} --draft=false --latest=false`;

    log('');
    log(`Uploaded SHA256SUMS.sig to draft release ${tag} and verified against ${path.relative(root, publicKey) || publicKey}.`);
    log('Publish the draft without changing the repository latest release:');
    log(`  ${publishCommand}`);

    return { tag, version, entries, publishCommand };
  } finally {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  }
}

function parseCliArgs(argv) {
  let tag;
  let privateKey = DEFAULT_PRIVATE_KEY;
  let publicKey = DEFAULT_PUBLIC_KEY;
  let repo;
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--key') {
      privateKey = argv[++i];
      if (!privateKey) throw new Error('--key requires a file path.');
    } else if (arg === '--public-key') {
      publicKey = argv[++i];
      if (!publicKey) throw new Error('--public-key requires a file path.');
    } else if (arg === '--repo') {
      repo = argv[++i];
      if (!repo) throw new Error('--repo requires OWNER/REPO.');
    } else if (arg.startsWith('-')) {
      throw new Error(`Unknown option: ${arg}`);
    } else if (!tag) {
      tag = arg;
    } else {
      throw new Error(`Unexpected extra argument: ${arg}`);
    }
  }
  return { tag, privateKey, publicKey, repo };
}

const real = (file) => { try { return fs.realpathSync(file); } catch { return path.resolve(file); } };
if (process.argv[1] && real(process.argv[1]) === real(fileURLToPath(import.meta.url))) {
  try {
    const options = parseCliArgs(process.argv.slice(2));
    signLegionRelease(options);
  } catch (error) {
    console.error(error.stderr ? String(error.stderr).trim() : error.message);
    process.exitCode = 1;
  }
}
