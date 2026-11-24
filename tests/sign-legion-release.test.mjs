import test from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { createSigningKey } from '../scripts/release-integrity.mjs';
import { parseSha256Sums, signLegionRelease, signSha256SumsBytes } from '../scripts/sign-legion-release.mjs';

const repoFile = (rel) => fileURLToPath(new URL(`../${rel}`, import.meta.url));

test('sign-legion-release signs draft SHA256SUMS, uploads SHA256SUMS.sig, and prints --latest=false publish command', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-sign-test-'));
  try {
    const privKey = path.join(dir, 'release-signing.key');
    const pubKey = path.join(dir, 'release-public-key.pem');
    createSigningKey(privKey, pubKey);

    const sumsBytes = Buffer.from(
      [
        `${'a'.repeat(64)}  legion-0.2.0-x86_64-unknown-linux-musl`,
        `${'b'.repeat(64)}  legion-0.2.0-aarch64-apple-darwin`,
        `${'c'.repeat(64)}  legion_0.2.0-1_amd64.deb`,
        `${'d'.repeat(64)}  legion_0.2.0-1_arm64.deb`,
        '',
      ].join('\n'),
      'utf8',
    );
    const fixtureSums = path.join(dir, 'fixture-SHA256SUMS');
    const uploadedSig = path.join(dir, 'uploaded.sig');
    fs.writeFileSync(fixtureSums, sumsBytes);

    // Fake `gh` invoked directly via `[process.execPath, helper]` on every OS (no `.cmd` shim).
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

    const lines = [];
    const result = signLegionRelease({
      tag: 'legion-v0.2.0',
      privateKey: privKey,
      publicKey: pubKey,
      gh: [process.execPath, helper],
      log: (line) => lines.push(line),
    });

    assert.equal(result.version, '0.2.0');
    assert.equal(result.entries.length, 4);
    assert.equal(result.publishCommand, 'gh release edit legion-v0.2.0 --draft=false --latest=false');
    assert.ok(lines.some((line) => line.includes('legion-0.2.0-x86_64-unknown-linux-musl')));
    assert.ok(lines.some((line) => line.includes('legion_0.2.0-1_amd64.deb')));
    assert.ok(lines.some((line) => line.includes('--draft=false --latest=false')));

    const sigBytes = fs.readFileSync(uploadedSig);
    assert.equal(sigBytes[sigBytes.length - 1], 0x0a, 'SHA256SUMS.sig must end with LF');
    const sigText = sigBytes.toString('ascii').trim();
    const ok = crypto.verify(
      null,
      sumsBytes,
      crypto.createPublicKey(fs.readFileSync(pubKey)),
      Buffer.from(sigText, 'base64'),
    );
    assert.equal(ok, true);

    // Mismatched public key is refused before upload.
    const otherPriv = path.join(dir, 'other.key');
    const otherPub = path.join(dir, 'other.pem');
    createSigningKey(otherPriv, otherPub);
    assert.throws(
      () => signSha256SumsBytes(sumsBytes, privKey, otherPub),
      /does not match/,
    );
    assert.throws(() => parseSha256Sums('not a checksum\n'), /Malformed/);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('legion release workflow enforces main ancestry, --latest=false, and --prerelease without signing', () => {
  const workflow = fs.readFileSync(repoFile('.github/workflows/legion-release.yml'), 'utf8');
  assert.match(workflow, /git merge-base --is-ancestor HEAD origin\/main/);
  assert.match(workflow, /--latest=false/);
  assert.match(workflow, /--prerelease/);
  assert.match(workflow, /--draft/);
  assert.doesNotMatch(workflow, /--draft=false\s+dist\//);
  assert.match(workflow, /gh release edit legion-vX\.Y\.Z --draft=false --latest=false/);
  assert.doesNotMatch(workflow, /actions\/cache/);
  const buildRevMatches = workflow.match(/LEGION_BUILD_REV:\s*\$\{\{\s*github\.sha\s*\}\}/g) ?? [];
  assert.equal(buildRevMatches.length, 3, 'every release build step must set LEGION_BUILD_REV from github.sha');
  assert.match(workflow, /deb_arch:\s*amd64/);
  assert.match(workflow, /deb_arch:\s*arm64/);
  assert.match(workflow, /bash packaging\/deb\/build-deb\.sh/);
  assert.match(workflow, /sha256sum legion-\* legion_\*\.deb > SHA256SUMS/);
});

test('install scripts enforce HTTPS-only TLS 1.2 downloads and silent PowerShell progress', () => {
  const sh = fs.readFileSync(repoFile('install/legion.sh'), 'utf8');
  assert.match(sh, /--proto '=https'/);
  assert.match(sh, /--proto-redir '=https'/);
  assert.match(sh, /--tlsv1\.2/);
  assert.match(sh, /--max-time 120/);
  assert.match(sh, /--https-only/);
  assert.match(sh, /--secure-protocol=TLSv1_2/);
  assert.match(sh, /require_hardened_wget/);

  const ps1 = fs.readFileSync(repoFile('install/legion.ps1'), 'utf8');
  assert.match(ps1, /\$ProgressPreference\s*=\s*'SilentlyContinue'/);
  assert.match(ps1, /Tls12/);
});

test('install/legion.ps1 preserves REG_EXPAND_SZ and unexpanded PATH variables', () => {
  const ps1 = fs.readFileSync(repoFile('install/legion.ps1'), 'utf8');
  assert.match(ps1, /OpenSubKey\('Environment',\s*\$true\)/);
  assert.match(ps1, /DoNotExpandEnvironmentNames/);
  assert.match(ps1, /GetValueKind\('Path'\)/);
  assert.match(ps1, /RegistryValueKind\]::ExpandString/);
  assert.match(ps1, /SetValue\('Path'/);
  assert.doesNotMatch(ps1, /SetEnvironmentVariable\('Path'/);
});

test('install/legion.sh wraps execution in main() and refuses a directory destination cleanly', () => {
  const shPath = repoFile('install/legion.sh');
  const sh = fs.readFileSync(shPath, 'utf8');
  const lines = sh.split(/\r?\n/).filter((line) => line.trim() !== '');
  assert.equal(lines[lines.length - 1], 'main "$@"');
  assert.match(sh, /^main\(\) \{/m);

  if (process.platform === 'win32') return;
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-sh-dir-test-'));
  try {
    const destDir = path.join(dir, '.local', 'bin', 'legion');
    fs.mkdirSync(destDir, { recursive: true });
    const fakeBin = path.join(dir, 'fake-legion');
    fs.writeFileSync(fakeBin, '#!/bin/sh\necho legion 0.2.0\n', { mode: 0o755 });

    const run = spawnSync('sh', [shPath, '--from', fakeBin], {
      env: { ...process.env, HOME: dir },
      encoding: 'utf8',
    });
    assert.notEqual(run.status, 0);
    assert.match(run.stderr, /is a directory/);
    assert.deepEqual(fs.readdirSync(destDir), []);
    assert.deepEqual(fs.readdirSync(path.join(dir, '.local', 'bin')), ['legion']);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('install scripts handle Rosetta 2 on macOS and honest x64 emulation on Windows ARM64', () => {
  const sh = fs.readFileSync(repoFile('install/legion.sh'), 'utf8');
  assert.match(sh, /sysctl\.proc_translated/);
  assert.match(sh, /Rosetta 2 translation detected/);

  const ps1 = fs.readFileSync(repoFile('install/legion.ps1'), 'utf8');
  assert.match(ps1, /'ARM64'\s*\{[\s\S]*Write-Warning[\s\S]*x86_64-pc-windows-msvc/);
});

test('install/legion.sh --from accepts both old and build-tagged --version outputs', () => {
  if (process.platform === 'win32') return;
  const shPath = repoFile('install/legion.sh');
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-sh-ver-test-'));
  try {
    for (const line of [
      'legion 0.1.0',
      'legion 0.1.0 (a59ec0d12345)',
      'legion 0.1.0 (a59ec0d12345, modified)',
    ]) {
      const fakeBin = path.join(dir, 'fake-legion');
      fs.writeFileSync(fakeBin, `#!/bin/sh\nprintf '%s\\n' '${line}'\n`, { mode: 0o755 });
      const run = spawnSync('sh', [shPath, '--from', fakeBin], {
        env: { ...process.env, HOME: dir },
        encoding: 'utf8',
      });
      assert.equal(run.status, 0, run.stderr);
      assert.ok(run.stdout.includes(`Installed ${line} to `), run.stdout);
    }
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
