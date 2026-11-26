import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import test from 'node:test';
import {
  formatPreviewInfo,
  inspectMachoArchs,
  normalizeCommit,
  readTomlSectionVersion,
  validatePreviewPubkeyPem,
  validatePreviewSequence,
  validateTargetOs,
} from '../scripts/build-desktop-preview.mjs';
import { createPreviewKey, currentSequence, publicKeyFingerprint } from '../scripts/make-update-set.mjs';

const SCRIPT_PATH = path.resolve(import.meta.dirname, '..', 'scripts', 'build-desktop-preview.mjs');

function makeEd25519Keypair() {
  const { publicKey, privateKey } = crypto.generateKeyPairSync('ed25519');
  return {
    pubPem: publicKey.export({ type: 'spki', format: 'pem' }).toString(),
    privPem: privateKey.export({ type: 'pkcs8', format: 'pem' }).toString(),
    fingerprint: publicKeyFingerprint(publicKey),
  };
}

test('validateTargetOs accepts desktop platforms only', () => {
  assert.equal(validateTargetOs('linux'), 'linux');
  assert.equal(validateTargetOs('macos'), 'macos');
  assert.equal(validateTargetOs('windows'), 'windows');
  assert.throws(() => validateTargetOs('android'), /Unsupported --os/);
  assert.throws(() => validateTargetOs(''), /Unsupported --os/);
});

test('validatePreviewSequence requires exactly 12 digits and a valid UTC timestamp', () => {
  const now = new Date('2026-11-25T12:00:00Z');
  assert.deepEqual(validatePreviewSequence('202611251200', { now }), {
    sequence: 202611251200,
    sequenceStr: '202611251200',
  });
  // Empty / missing
  assert.throws(() => validatePreviewSequence('', { now }), /12-digit UTC YYYYMMDDHHMM/);
  assert.throws(() => validatePreviewSequence(undefined, { now }), /12-digit UTC YYYYMMDDHHMM/);
  // Wrong length or non-digits (small test sequence is rejected for CI workflow_dispatch)
  assert.throws(() => validatePreviewSequence('42', { now }), /exactly 12 digits/);
  assert.throws(() => validatePreviewSequence('20261125120', { now }), /exactly 12 digits/);
  assert.throws(() => validatePreviewSequence('2026112512000', { now }), /exactly 12 digits/);
  assert.throws(() => validatePreviewSequence('20261125abcd', { now }), /exactly 12 digits/);
  // Invalid calendar date (Feb 30 / month 13)
  assert.throws(() => validatePreviewSequence('202602301200', { now }), /not a valid UTC/);
  assert.throws(() => validatePreviewSequence('202613011200', { now }), /not a valid UTC/);
  // More than 24h in the future
  assert.throws(() => validatePreviewSequence('202611271200', { now }), /Refusing future sequence/);
});

test('validatePreviewPubkeyPem names LEGION_PREVIEW_UPDATE_PUBKEY when missing and refuses private or non-Ed25519 keys', () => {
  assert.throws(
    () => validatePreviewPubkeyPem(''),
    /repository variable LEGION_PREVIEW_UPDATE_PUBKEY/,
  );
  assert.throws(
    () => validatePreviewPubkeyPem('   \n  '),
    /repository variable LEGION_PREVIEW_UPDATE_PUBKEY/,
  );

  const kp = makeEd25519Keypair();
  // Accepts CRLF and surrounding whitespace and emits canonical SPKI PEM + matching fingerprint.
  const valid = validatePreviewPubkeyPem(`  \r\n${kp.pubPem.replace(/\n/g, '\r\n')}\r\n  `);
  assert.equal(valid.fingerprint, kp.fingerprint);
  assert.match(valid.pem, /^-----BEGIN PUBLIC KEY-----\n/);
  assert.match(valid.pem, /\n-----END PUBLIC KEY-----\n$/);

  // Refuses private keys.
  assert.throws(() => validatePreviewPubkeyPem(kp.privPem), /Refusing private key material/);

  // Refuses non-Ed25519 public keys (RSA).
  const rsaPub = crypto
    .generateKeyPairSync('rsa', { modulusLength: 2048 })
    .publicKey.export({ type: 'spki', format: 'pem' })
    .toString();
  assert.throws(() => validatePreviewPubkeyPem(rsaPub), /expected an Ed25519 public key/);

  // Refuses trailing garbage outside PEM envelope.
  assert.throws(
    () => validatePreviewPubkeyPem(`${kp.pubPem}\nextra-garbage\n`),
    /unexpected content outside/,
  );
});

test('inspectMachoArchs detects thin arm64, thin x86_64, and universal Mach-O binaries', () => {
  const thin = (cpuType) => {
    const buf = Buffer.alloc(32);
    buf.writeUInt32BE(0xcffaedfe, 0); // MH_MAGIC_64 LE bytes
    buf.writeUInt32LE(cpuType, 4);
    return buf;
  };
  assert.deepEqual(inspectMachoArchs(thin(0x0100000c)), {
    archs: ['arm64'],
    hasArm64: true,
    isNativeArm64: true,
  });
  assert.deepEqual(inspectMachoArchs(thin(0x01000007)), {
    archs: ['x86_64'],
    hasArm64: false,
    isNativeArm64: false,
  });

  // Universal (fat) binary containing x86_64 + arm64.
  const fat = Buffer.alloc(64);
  fat.writeUInt32BE(0xcafebabe, 0);
  fat.writeUInt32BE(2, 4);
  fat.writeUInt32BE(0x01000007, 8);
  fat.writeUInt32BE(0x0100000c, 28);
  assert.deepEqual(inspectMachoArchs(fat), {
    archs: ['x86_64', 'arm64'],
    hasArm64: true,
    isNativeArm64: false,
  });

  assert.throws(() => inspectMachoArchs(Buffer.from('\x7fELFnotmacho')), /Unrecognized Mach-O/);
});

test('formatPreviewInfo includes commit, sequence, fingerprint, macOS arm64 status, and never leaks key paths', () => {
  const kp = makeEd25519Keypair();
  const macText = formatPreviewInfo({
    os: 'macos',
    arch: 'aarch64',
    commit: '0123456789abcdef0123456789abcdef01234567',
    buildRev: '0123456789ab',
    sequence: '202611251200',
    appVersion: '0.1.0',
    nodeVersion: '0.1.0',
    fingerprint: kp.fingerprint,
    macosAppArchs: ['arm64'],
    macosNodeArchs: ['arm64'],
    files: [
      { name: 'legion-macos-aarch64', size: 1024, sha256: 'a'.repeat(64) },
      { name: 'Legion-macos-aarch64.app.tar.gz', size: 2048, sha256: 'b'.repeat(64) },
    ],
  });
  assert.match(macText, /^artifact: legion-macos-preview$/m);
  assert.match(macText, /^commit: 0123456789abcdef0123456789abcdef01234567$/m);
  assert.match(macText, /^sequence: 202611251200$/m);
  assert.match(macText, new RegExp(`^preview_key_fingerprint: ${kp.fingerprint.replace(/[+]/g, '\\+')}$`, 'm'));
  assert.match(macText, /^macos_app_arch: arm64$/m);
  assert.match(macText, /^macos_app_arm64: yes$/m);
  assert.match(macText, /^macos_node_arch: arm64$/m);
  assert.doesNotMatch(macText, /PRIVATE KEY|\/home\/|\/Users\/|\.pem/);

  const linuxText = formatPreviewInfo({
    os: 'linux',
    arch: 'x86_64',
    commit: '0123456789abcdef0123456789abcdef01234567',
    buildRev: '0123456789ab',
    sequence: '202611251200',
    appVersion: '0.1.0',
    nodeVersion: '0.1.0',
    fingerprint: kp.fingerprint,
    files: [{ name: 'legion-linux-x86_64', size: 1024, sha256: 'c'.repeat(64) }],
  });
  assert.match(linuxText, /^artifact: legion-linux-preview$/m);
  assert.doesNotMatch(linuxText, /macos_app_arch/);
});

test('readTomlSectionVersion and normalizeCommit parse repo metadata', () => {
  const repoRoot = path.resolve(import.meta.dirname, '..');
  const rootVer = readTomlSectionVersion(
    fs.readFileSync(path.join(repoRoot, 'Cargo.toml'), 'utf8'),
    'workspace.package',
  );
  const appVer = readTomlSectionVersion(
    fs.readFileSync(path.join(repoRoot, 'apps', 'legion-app', 'src-tauri', 'Cargo.toml'), 'utf8'),
    'package',
  );
  assert.equal(rootVer, appVer);
  assert.deepEqual(normalizeCommit('ABCDEF0123456789abcdef0123456789abcdef01'), {
    commit: 'ABCDEF0123456789abcdef0123456789abcdef01',
    buildRev: 'abcdef012345',
  });
});

test('CLI --check-only fails clearly when LEGION_PREVIEW_UPDATE_PUBKEY is missing and succeeds when valid', () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-preview-cli-'));
  try {
    const priv = path.join(tmp, 'preview.key');
    const pub = path.join(tmp, 'preview.pub.pem');
    const created = createPreviewKey({ privateKeyPath: priv, publicKeyPath: pub });
    const pubPem = fs.readFileSync(pub, 'utf8');
    const seq = String(currentSequence());

    // Missing variable fails with a sentence naming LEGION_PREVIEW_UPDATE_PUBKEY.
    const cleanEnv = { ...process.env };
    delete cleanEnv.LEGION_PREVIEW_UPDATE_PUBKEY;
    assert.throws(
      () =>
        execFileSync(process.execPath, [SCRIPT_PATH, '--check-only', '--os', 'macos', '--sequence', seq], {
          env: cleanEnv,
          stdio: 'pipe',
          encoding: 'utf8',
        }),
      (err) => {
        assert.match(err.stderr, /repository variable LEGION_PREVIEW_UPDATE_PUBKEY/);
        return true;
      },
    );

    // Valid env passes --check-only and prints the fingerprint without key material.
    const out = execFileSync(
      process.execPath,
      [SCRIPT_PATH, '--check-only', '--os', 'macos', '--sequence', seq],
      {
        env: { ...cleanEnv, LEGION_PREVIEW_UPDATE_PUBKEY: pubPem },
        stdio: 'pipe',
        encoding: 'utf8',
      },
    );
    assert.match(out, new RegExp(created.fingerprint.replace(/[+]/g, '\\+')));
    assert.doesNotMatch(out, /BEGIN PUBLIC KEY/);
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});
