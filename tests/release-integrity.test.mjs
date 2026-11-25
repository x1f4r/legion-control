import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createSigningKey, signManifest, verifyManifest, artifactDigest, verifyArtifact } from '../scripts/release-integrity.mjs';

test('release signature authenticates exact manifest bytes and refuses another signer', async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-release-test-'));
  try {
    const file = (name) => path.join(root, name);
    createSigningKey(file('private.pem'), file('public.pem'));
    fs.writeFileSync(file('manifest.json'), '{"version":"2.0.0"}\n');
    signManifest(file('manifest.json'), file('private.pem'), file('manifest.sig'));
    assert.equal(verifyManifest(file('manifest.json'), file('manifest.sig'), file('public.pem')).version, '2.0.0');
    createSigningKey(file('other-private.pem'), file('other-public.pem'));
    assert.throws(() => verifyManifest(file('manifest.json'), file('manifest.sig'), file('other-public.pem')), /trusted key/);
    fs.appendFileSync(file('manifest.json'), ' ');
    assert.throws(() => verifyManifest(file('manifest.json'), file('manifest.sig'), file('public.pem')), /trusted key/);
    assert.throws(() => createSigningKey(file('private.pem'), file('public.pem')), /refusing/);
    fs.writeFileSync(file('asset.zip'), 'original artifact');
    const digest = await artifactDigest(file('asset.zip'));
    assert.equal(await verifyArtifact(file('asset.zip'), digest), true);
    fs.writeFileSync(file('asset.zip'), 'tampered artifact');
    await assert.rejects(verifyArtifact(file('asset.zip'), digest), /do not match/);
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});

test('Arch legion-bin PKGBUILD pins the exact contract release public key and verifies signatures', () => {
  const repoRoot = path.resolve(import.meta.dirname, '..');
  const contractPem = fs.readFileSync(path.join(repoRoot, 'contract', 'release-public-key.pem'), 'utf8').trim();
  const pkgbuild = fs.readFileSync(path.join(repoRoot, 'packaging', 'arch', 'legion-bin', 'PKGBUILD'), 'utf8');
  assert.ok(pkgbuild.includes(contractPem), 'packaging/arch/legion-bin/PKGBUILD must pin contract/release-public-key.pem verbatim');
  assert.match(pkgbuild, /prepare\(\)\s*\{/, 'packaging/arch/legion-bin/PKGBUILD must verify release signatures in prepare()');
  assert.match(pkgbuild, /openssl pkeyutl -verify -pubin/, 'packaging/arch/legion-bin/PKGBUILD must verify SHA256SUMS.sig with OpenSSL Ed25519');
  assert.match(pkgbuild, /SHA256SUMS\.sig/, 'packaging/arch/legion-bin/PKGBUILD must fetch SHA256SUMS.sig');
});
