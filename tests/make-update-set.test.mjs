import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';

import {
  androidVersionCode,
  createPreviewKey,
  currentSequence,
  desktopBundleVersion,
  macosBundleVersion,
  makeUpdateSet,
  parseArtifactSpec,
  tauriBundleConfig,
  verifyUpdateSet,
} from '../scripts/make-update-set.mjs';

test('sequence and platform version derivation stay strictly monotonic', () => {
  assert.equal(currentSequence(new Date('2026-11-25T01:37:42Z')), 202611250137);
  assert.equal(androidVersionCode(1), 1);
  assert.equal(androidVersionCode(42), 42);
  const v1 = androidVersionCode(202611250130);
  const v2 = androidVersionCode(202611250131);
  const v3 = androidVersionCode(202611250200);
  assert.equal(v2, v1 + 1);
  assert.ok(v3 > v2);
  assert.ok(androidVersionCode(299912312359) < 2_100_000_000);
  assert.throws(() => androidVersionCode(0));
  assert.throws(() => androidVersionCode(202602300000));
  assert.equal(desktopBundleVersion('0.1.0', 202611250137), '0.1.0+202611250137');
  assert.equal(macosBundleVersion(202611250137), '202611250137');
  assert.deepEqual(tauriBundleConfig({ baseVersion: '0.1.0', sequence: 202611250137, os: 'linux' }), {
    version: '0.1.0+202611250137',
  });
  assert.deepEqual(tauriBundleConfig({ baseVersion: '0.1.0', sequence: 202611250137, os: 'macos' }), {
    version: '0.1.0',
    bundle: { macOS: { bundleVersion: '202611250137' } },
  });
  assert.deepEqual(tauriBundleConfig({ baseVersion: '0.1.0', sequence: 202611250137, os: 'windows' }), {
    version: '0.1.0',
  });
});

test('createPreviewKey writes mode 600 outside the repo and refuses overwrite or repo paths', () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-update-keygen-'));
  try {
    const priv = path.join(tmp, 'keys', 'preview-update.key');
    const pub = path.join(tmp, 'keys', 'preview-update.pub.pem');
    const created = createPreviewKey({ privateKeyPath: priv, publicKeyPath: pub });
    assert.ok(created.fingerprint.startsWith('SHA256:'));
    assert.ok(fs.existsSync(priv));
    assert.ok(fs.existsSync(pub));
    if (process.platform !== 'win32') {
      assert.equal(fs.statSync(priv).mode & 0o777, 0o600);
    }

    // Refuses to overwrite existing key.
    assert.throws(() => createPreviewKey({ privateKeyPath: priv, publicKeyPath: pub }), /already exists/);

    // Refuses key paths inside the repository.
    const repoRoot = path.resolve(import.meta.dirname, '..');
    assert.throws(
      () =>
        createPreviewKey({
          privateKeyPath: path.join(repoRoot, 'tmp-key.pem'),
          publicKeyPath: path.join(tmp, 'outside.pub.pem'),
        }),
      /inside the repository/,
    );
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});

test('makeUpdateSet assembles, signs, verifies, and detects tampering', () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-update-set-'));
  try {
    const priv = path.join(tmp, 'preview.key');
    const pub = path.join(tmp, 'preview.pub.pem');
    const otherPriv = path.join(tmp, 'other.key');
    const otherPub = path.join(tmp, 'other.pub.pem');
    createPreviewKey({ privateKeyPath: priv, publicKeyPath: pub });
    createPreviewKey({ privateKeyPath: otherPriv, publicKeyPath: otherPub });

    const apk = path.join(tmp, 'Legion-0.2.0-universal.apk');
    const appimage = path.join(tmp, 'Legion-0.2.0-x86_64.AppImage');
    fs.writeFileSync(apk, 'fake-apk-bytes');
    fs.writeFileSync(appimage, 'fake-appimage-bytes');

    const outDir = path.join(tmp, 'set');
    const created = makeUpdateSet({
      outDir,
      channel: 'preview',
      version: '0.2.0',
      build: 'abc123def456',
      sequence: 202611250137,
      notes: 'Automatic fleet updates.',
      privateKeyPath: priv,
      publicKeyPath: pub,
      artifacts: [apk, appimage],
    });
    assert.equal(created.manifest.artifacts.length, 2);
    assert.equal(created.entries.length, 3);

    const verified = verifyUpdateSet({ dir: outDir, publicKeyPath: pub });
    assert.equal(verified.manifest.sequence, 202611250137);
    assert.equal(verified.count, 3);

    // Wrong key fails.
    assert.throws(() => verifyUpdateSet({ dir: outDir, publicKeyPath: otherPub }), /does not match/);

    // Tampered artifact fails.
    fs.writeFileSync(path.join(outDir, 'Legion-0.2.0-universal.apk'), 'tampered');
    assert.throws(() => verifyUpdateSet({ dir: outDir, publicKeyPath: pub }), /SHA-256 mismatch/);
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});

test('parseArtifactSpec rejects invalid filenames and mismatched combinations', () => {
  const parsed = parseArtifactSpec('/tmp/Legion.app.tar.gz:program=app,os=macos,arch=aarch64,kind=app-tar');
  assert.equal(parsed.name, 'Legion.app.tar.gz');
  assert.equal(parsed.kind, 'app-tar');
  assert.throws(
    () => parseArtifactSpec('/tmp/a.apk:program=app,os=linux,arch=x86_64,kind=apk'),
    /unsupported combination/,
  );
  assert.throws(
    () => parseArtifactSpec('/tmp/a.apk:program=app,os=android,arch=aarch64,kind=apk,name=../evil.apk'),
    /not a valid bare filename/,
  );
});
