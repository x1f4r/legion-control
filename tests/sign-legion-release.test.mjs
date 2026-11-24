import test from 'node:test';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createSigningKey } from '../scripts/release-integrity.mjs';
import { parseSha256Sums, signLegionRelease, signSha256SumsBytes } from '../scripts/sign-legion-release.mjs';

test('sign-legion-release signs draft SHA256SUMS, uploads SHA256SUMS.sig, and prints --latest=false publish command', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-sign-test-'));
  try {
    const privKey = path.join(dir, 'release-signing.key');
    const pubKey = path.join(dir, 'release-public-key.pem');
    createSigningKey(privKey, pubKey);

    const sumsContent = [
      `${'a'.repeat(64)}  legion-0.2.0-x86_64-unknown-linux-musl`,
      `${'b'.repeat(64)}  legion-0.2.0-aarch64-apple-darwin`,
      '',
    ].join('\n');
    const fixtureSums = path.join(dir, 'fixture-SHA256SUMS');
    const uploadedSig = path.join(dir, 'uploaded.sig');
    fs.writeFileSync(fixtureSums, sumsContent);

    // Fake `gh` script that handles view, download, and upload without touching the network.
    const fakeGh = path.join(dir, process.platform === 'win32' ? 'gh.cmd' : 'gh');
    const helper = path.join(dir, 'fake-gh.mjs');
    fs.writeFileSync(helper, `
import fs from 'node:fs';
import path from 'node:path';
const args = process.argv.slice(2);
if (args[0] === 'release' && args[1] === 'view') {
  console.log(JSON.stringify({
    tagName: args[2],
    isDraft: process.env.FAKE_DRAFT !== 'false',
    isPrerelease: false,
    assets: [{ name: 'SHA256SUMS' }],
  }));
} else if (args[0] === 'release' && args[1] === 'download') {
  const dirIdx = args.indexOf('--dir');
  fs.copyFileSync(${JSON.stringify(fixtureSums)}, path.join(args[dirIdx + 1], 'SHA256SUMS'));
} else if (args[0] === 'release' && args[1] === 'upload') {
  fs.copyFileSync(args[3], ${JSON.stringify(uploadedSig)});
} else {
  console.error('unexpected gh args:', args);
  process.exit(1);
}
`);
    if (process.platform === 'win32') {
      fs.writeFileSync(fakeGh, `@echo off\r\n"${process.execPath}" "${helper}" %*\r\n`);
    } else {
      fs.writeFileSync(fakeGh, `#!/bin/sh\nexec "${process.execPath}" "${helper}" "$@"\n`, { mode: 0o755 });
    }

    const lines = [];
    const result = signLegionRelease({
      tag: 'legion-v0.2.0',
      privateKey: privKey,
      publicKey: pubKey,
      gh: fakeGh,
      log: (line) => lines.push(line),
    });

    assert.equal(result.version, '0.2.0');
    assert.equal(result.entries.length, 2);
    assert.equal(result.publishCommand, 'gh release edit legion-v0.2.0 --draft=false --latest=false');
    assert.ok(lines.some((line) => line.includes('legion-0.2.0-x86_64-unknown-linux-musl')));
    assert.ok(lines.some((line) => line.includes('--draft=false --latest=false')));

    const sigText = fs.readFileSync(uploadedSig, 'utf8').trim();
    const ok = crypto.verify(
      null,
      Buffer.from(sumsContent, 'utf8'),
      crypto.createPublicKey(fs.readFileSync(pubKey)),
      Buffer.from(sigText, 'base64'),
    );
    assert.equal(ok, true);

    // Mismatched public key is refused before upload.
    const otherPriv = path.join(dir, 'other.key');
    const otherPub = path.join(dir, 'other.pem');
    createSigningKey(otherPriv, otherPub);
    assert.throws(
      () => signSha256SumsBytes(Buffer.from(sumsContent), privKey, otherPub),
      /does not match/,
    );
    assert.throws(() => parseSha256Sums('not a checksum\n'), /Malformed/);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('legion release workflow enforces main ancestry, --latest=false, and --prerelease without signing', () => {
  const workflow = fs.readFileSync(new URL('../.github/workflows/legion-release.yml', import.meta.url), 'utf8');
  assert.match(workflow, /git merge-base --is-ancestor HEAD origin\/main/);
  assert.match(workflow, /--latest=false/);
  assert.match(workflow, /--prerelease/);
  assert.match(workflow, /--draft/);
  assert.doesNotMatch(workflow, /--draft=false\s+dist\//);
  assert.match(workflow, /gh release edit legion-vX\.Y\.Z --draft=false --latest=false/);
  assert.doesNotMatch(workflow, /actions\/cache/);
});

test('install scripts enforce HTTPS-only TLS 1.2 downloads and silent PowerShell progress', () => {
  const sh = fs.readFileSync(new URL('../install/legion.sh', import.meta.url), 'utf8');
  assert.match(sh, /--proto '=https'/);
  assert.match(sh, /--proto-redir '=https'/);
  assert.match(sh, /--tlsv1\.2/);
  assert.match(sh, /--max-time 120/);
  assert.match(sh, /--https-only/);
  assert.match(sh, /--secure-protocol=TLSv1_2/);
  assert.match(sh, /require_hardened_wget/);

  const ps1 = fs.readFileSync(new URL('../install/legion.ps1', import.meta.url), 'utf8');
  assert.match(ps1, /\$ProgressPreference\s*=\s*'SilentlyContinue'/);
  assert.match(ps1, /Tls12/);
});

test('install/legion.ps1 preserves REG_EXPAND_SZ and unexpanded PATH variables', () => {
  const ps1 = fs.readFileSync(new URL('../install/legion.ps1', import.meta.url), 'utf8');
  assert.match(ps1, /OpenSubKey\('Environment',\s*\$true\)/);
  assert.match(ps1, /DoNotExpandEnvironmentNames/);
  assert.match(ps1, /GetValueKind\('Path'\)/);
  assert.match(ps1, /RegistryValueKind\]::ExpandString/);
  assert.match(ps1, /SetValue\('Path'/);
  assert.doesNotMatch(ps1, /SetEnvironmentVariable\('Path'/);
});
