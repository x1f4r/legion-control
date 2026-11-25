#!/usr/bin/env node
// Build preview-keyed desktop app bundles and the matching platform `legion` node binary for
// Linux (x86_64), macOS (arm64), or Windows (x86_64), verify every artifact's compiled build
// sequence and preview key fingerprint, and write a clean PREVIEW-INFO.txt summary.
//
// Used by `.github/workflows/legion-app.yml` on manual `workflow_dispatch` runs with
// `preview: true` and runnable locally on Linux/macOS/Windows with a throwaway key.
//
// Usage:
//   node scripts/build-desktop-preview.mjs \
//     --os <linux|macos|windows> \
//     --sequence <YYYYMMDDHHMM> \
//     --out <staging-dir> \
//     [--commit <sha>] \
//     [--pubkey-env <VAR_NAME>] \
//     [--pubkey-file <path>] \
//     [--node-target-dir <dir>] \
//     [--check-only]
import { execFileSync, spawn } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  checkShippedArtifact,
  publicKeyFingerprint,
  tauriBundleConfig,
  validateCreateSequence,
  validateEntryName,
} from './make-update-set.mjs';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const DEFAULT_PUBKEY_ENV = 'LEGION_PREVIEW_UPDATE_PUBKEY';
const VALID_OS = new Set(['linux', 'macos', 'windows']);

const MACHO_CPU_NAMES = new Map([
  [0x0100000c, 'arm64'],
  [0x01000007, 'x86_64'],
  [0x0000000c, 'arm'],
  [0x00000007, 'i386'],
]);

/** Validate target OS name. */
export function validateTargetOs(rawOs) {
  const osName = String(rawOs || '').trim().toLowerCase();
  if (!VALID_OS.has(osName)) {
    throw new Error(
      `Unsupported --os ${JSON.stringify(rawOs)}: expected linux, macos, or windows.`,
    );
  }
  return osName;
}

/**
 * Validate the preview build sequence: required, strictly 12 ASCII digits representing a valid
 * UTC YYYYMMDDHHMM timestamp and not further ahead than the update-set future horizon.
 */
export function validatePreviewSequence(rawSequence, { now = new Date() } = {}) {
  const trimmed = String(rawSequence ?? '').trim();
  if (!trimmed) {
    throw new Error(
      'Preview desktop builds require a 12-digit UTC YYYYMMDDHHMM sequence (pass the sequence workflow input or --sequence).',
    );
  }
  if (!/^\d{12}$/.test(trimmed)) {
    throw new Error(
      `Invalid preview sequence ${JSON.stringify(trimmed)}: expected exactly 12 digits (UTC YYYYMMDDHHMM, for example 202611251200).`,
    );
  }
  const sequence = validateCreateSequence(Number(trimmed), now);
  return { sequence, sequenceStr: trimmed };
}

/**
 * Validate an Ed25519 SPKI public key PEM from a repository variable or file and return canonical
 * SPKI PEM text plus its `SHA256:<base64>` fingerprint. Never accepts private key material.
 */
export function validatePreviewPubkeyPem(rawPem, { varName = DEFAULT_PUBKEY_ENV } = {}) {
  const normalized = String(rawPem ?? '').replace(/\r\n/g, '\n').trim();
  if (!normalized) {
    throw new Error(
      `Preview desktop builds require the repository variable ${varName} to be set to the Ed25519 public key PEM (-----BEGIN PUBLIC KEY-----).`,
    );
  }
  if (normalized.includes('PRIVATE KEY')) {
    throw new Error(
      `Refusing private key material in ${varName}; set it to the Ed25519 SPKI public key PEM (-----BEGIN PUBLIC KEY-----) only.`,
    );
  }

  let inside = false;
  let closed = false;
  let bodyLines = 0;
  for (const rawLine of normalized.split('\n')) {
    const line = rawLine.trim();
    if (!line) continue;
    if (line === '-----BEGIN PUBLIC KEY-----') {
      if (inside || closed) {
        throw new Error(`Invalid ${varName}: duplicate or unexpected BEGIN PUBLIC KEY header.`);
      }
      inside = true;
      continue;
    }
    if (line === '-----END PUBLIC KEY-----') {
      if (!inside) {
        throw new Error(`Invalid ${varName}: unexpected END PUBLIC KEY footer.`);
      }
      inside = false;
      closed = true;
      continue;
    }
    if (!inside) {
      throw new Error(
        `Invalid ${varName}: unexpected content outside -----BEGIN PUBLIC KEY----- / -----END PUBLIC KEY-----.`,
      );
    }
    bodyLines += 1;
  }
  if (!closed || bodyLines === 0) {
    throw new Error(
      `Invalid ${varName}: expected an Ed25519 SPKI public key PEM (-----BEGIN PUBLIC KEY-----).`,
    );
  }

  let keyObj;
  try {
    keyObj = crypto.createPublicKey(normalized);
  } catch (err) {
    throw new Error(`Invalid ${varName}: could not parse Ed25519 public key PEM (${err.message}).`);
  }
  if (keyObj.asymmetricKeyType !== 'ed25519') {
    throw new Error(
      `Invalid ${varName}: expected an Ed25519 public key, got ${keyObj.asymmetricKeyType || 'unknown'}.`,
    );
  }
  const fingerprint = publicKeyFingerprint(keyObj);
  const pem = keyObj.export({ type: 'spki', format: 'pem' }).toString();
  return { pem, fingerprint };
}

/** Read `version = "..."` from a named Cargo.toml section (such as `package` or `workspace.package`). */
export function readTomlSectionVersion(tomlText, sectionName) {
  let inSection = false;
  for (const rawLine of tomlText.replace(/\r\n/g, '\n').split('\n')) {
    const line = rawLine.trim();
    if (!line || line.startsWith('#')) continue;
    if (line.startsWith('[')) {
      inSection = line === `[${sectionName}]`;
      continue;
    }
    if (inSection) {
      const m = /^version\s*=\s*"([^"]+)"/.exec(line);
      if (m) return m[1].trim();
    }
  }
  throw new Error(`Could not read [${sectionName}].version from Cargo.toml.`);
}

/** Normalize a commit string into `{ commit, buildRev }` (12-char lowercase prefix for 40-char SHAs). */
export function normalizeCommit(rawCommit) {
  const trimmed = String(rawCommit || '').trim();
  if (!trimmed) {
    throw new Error('Could not determine git commit SHA; pass --commit.');
  }
  const buildRev =
    trimmed.length > 12 && /^[0-9a-fA-F]+$/.test(trimmed)
      ? trimmed.slice(0, 12).toLowerCase()
      : trimmed;
  return { commit: trimmed, buildRev };
}

/**
 * Inspect Mach-O thin or universal (fat) headers directly from bytes or a file path and return
 * `{ archs, hasArm64, isNativeArm64 }`.
 */
export function inspectMachoArchs(filePathOrBuffer) {
  let buf;
  if (Buffer.isBuffer(filePathOrBuffer)) {
    buf = filePathOrBuffer;
  } else {
    const fd = fs.openSync(filePathOrBuffer, 'r');
    try {
      const head = Buffer.allocUnsafe(4096);
      const n = fs.readSync(fd, head, 0, head.length, 0);
      buf = head.subarray(0, n);
    } finally {
      fs.closeSync(fd);
    }
  }
  if (buf.length < 8) {
    throw new Error('Binary is too short to be a Mach-O executable.');
  }

  const magicBe = buf.readUInt32BE(0);
  const archs = [];
  const addCpu = (cpuType) => {
    const name = MACHO_CPU_NAMES.get(cpuType) || `unknown(0x${cpuType.toString(16)})`;
    if (!archs.includes(name)) archs.push(name);
  };

  if (magicBe === 0xcffaedfe || magicBe === 0xcefaedfe) {
    // Little-endian 64-bit or 32-bit thin Mach-O (magic written in host LE order).
    addCpu(buf.readUInt32LE(4));
  } else if (magicBe === 0xfeedfacf || magicBe === 0xfeedface) {
    // Big-endian 64-bit or 32-bit thin Mach-O.
    addCpu(buf.readUInt32BE(4));
  } else if (magicBe === 0xcafebabe || magicBe === 0xcafebabf) {
    // Universal / fat Mach-O (big-endian header fields).
    const stride = magicBe === 0xcafebabf ? 32 : 20;
    const count = buf.readUInt32BE(4);
    if (count === 0 || count > 32 || 8 + count * stride > buf.length) {
      throw new Error(`Invalid Mach-O fat header slice count (${count}).`);
    }
    for (let i = 0; i < count; i += 1) {
      addCpu(buf.readUInt32BE(8 + i * stride));
    }
  } else if (magicBe === 0xbebafeca || magicBe === 0xbfbafeca) {
    const stride = magicBe === 0xbfbafeca ? 32 : 20;
    const count = buf.readUInt32LE(4);
    if (count === 0 || count > 32 || 8 + count * stride > buf.length) {
      throw new Error(`Invalid Mach-O fat header slice count (${count}).`);
    }
    for (let i = 0; i < count; i += 1) {
      addCpu(buf.readUInt32LE(8 + i * stride));
    }
  } else {
    throw new Error(`Unrecognized Mach-O magic 0x${magicBe.toString(16).padStart(8, '0')}.`);
  }

  return {
    archs,
    hasArm64: archs.includes('arm64'),
    isNativeArm64: archs.length === 1 && archs[0] === 'arm64',
  };
}

/** Format the PREVIEW-INFO.txt metadata file carried inside each preview artifact. */
export function formatPreviewInfo({
  os: targetOs,
  arch,
  commit,
  buildRev,
  sequence,
  appVersion,
  nodeVersion,
  fingerprint,
  macosAppArchs = null,
  macosNodeArchs = null,
  files = [],
}) {
  const lines = [
    `artifact: legion-${targetOs}-preview`,
    'channel: preview',
    `commit: ${commit}`,
    `build: ${buildRev}`,
    `sequence: ${sequence}`,
    `app_version: ${appVersion}`,
    `node_version: ${nodeVersion}`,
    `os: ${targetOs}`,
    `arch: ${arch}`,
    `preview_key_fingerprint: ${fingerprint}`,
  ];

  if (targetOs === 'macos') {
    const appList = Array.isArray(macosAppArchs) ? macosAppArchs : [];
    const nodeList = Array.isArray(macosNodeArchs) ? macosNodeArchs : [];
    const appArm64 =
      appList.length === 1 && appList[0] === 'arm64'
        ? 'yes'
        : appList.includes('arm64')
          ? `yes (universal: ${appList.join(', ')})`
          : 'no';
    lines.push(`macos_app_arch: ${appList.join(', ') || 'unknown'}`);
    lines.push(`macos_app_arm64: ${appArm64}`);
    lines.push(`macos_node_arch: ${nodeList.join(', ') || 'unknown'}`);
  }

  if (files.length > 0) {
    lines.push('files:');
    const sorted = [...files].sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
    for (const file of sorted) {
      lines.push(`  ${file.sha256}  ${file.size}  ${file.name}`);
    }
  }

  return `${lines.join('\n')}\n`;
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

function runLoggedCommand(cmd, args, { cwd, env, logPath }) {
  return new Promise((resolve, reject) => {
    const logStream = logPath ? fs.createWriteStream(logPath, { flags: 'a' }) : null;
    const child = spawn(cmd, args, {
      cwd,
      env,
      stdio: ['ignore', 'pipe', 'pipe'],
      shell: false,
    });
    child.stdout.on('data', (chunk) => {
      process.stdout.write(chunk);
      if (logStream) logStream.write(chunk);
    });
    child.stderr.on('data', (chunk) => {
      process.stderr.write(chunk);
      if (logStream) logStream.write(chunk);
    });
    child.on('error', (err) => {
      if (logStream) logStream.end();
      reject(new Error(`Failed to start ${cmd}: ${err.message}`));
    });
    child.on('close', (code, signal) => {
      const finish = () => {
        if (code === 0) {
          resolve();
        } else {
          reject(
            new Error(
              `${cmd} ${args.join(' ')} failed (${signal ? `signal ${signal}` : `exit code ${code}`}).`,
            ),
          );
        }
      };
      if (logStream) {
        logStream.end(finish);
      } else {
        finish();
      }
    });
  });
}

function findExactlyOne(dir, predicate, description) {
  if (!fs.existsSync(dir)) {
    throw new Error(`Expected ${description} directory does not exist: ${dir}`);
  }
  const matches = fs
    .readdirSync(dir, { withFileTypes: true })
    .filter((entry) => predicate(entry))
    .map((entry) => path.join(dir, entry.name))
    .sort();
  if (matches.length !== 1) {
    throw new Error(
      `Expected exactly 1 ${description} in ${dir}, found ${matches.length}${matches.length ? ` (${matches.map((m) => path.basename(m)).join(', ')})` : ''}.`,
    );
  }
  return matches[0];
}

function resolveCommit(rawCommit, repoRoot) {
  if (rawCommit && String(rawCommit).trim()) {
    return normalizeCommit(rawCommit);
  }
  if (process.env.GITHUB_SHA && process.env.GITHUB_SHA.trim()) {
    return normalizeCommit(process.env.GITHUB_SHA);
  }
  const sha = execFileSync('git', ['rev-parse', 'HEAD'], {
    cwd: repoRoot,
    encoding: 'utf8',
  }).trim();
  return normalizeCommit(sha);
}

function removeIfExists(targetPath) {
  try {
    fs.rmSync(targetPath, { recursive: true, force: true });
  } catch {
    // Best-effort cleanup of build outputs.
  }
}

/**
 * Build the preview-keyed desktop app bundle(s) and `legion` node binary for `targetOs`, verify
 * compiled sequence + preview key markers on every artifact, and stage them with PREVIEW-INFO.txt.
 */
export async function buildDesktopPreview({
  os: rawOs,
  sequence: rawSequence,
  outDir,
  commit: rawCommit = '',
  pubkeyPem = null,
  pubkeyEnv = DEFAULT_PUBKEY_ENV,
  pubkeyFile = null,
  nodeTargetDir = null,
  checkOnly = false,
  repoRoot = REPO_ROOT,
} = {}) {
  const targetOs = validateTargetOs(rawOs);
  const { sequence, sequenceStr } = validatePreviewSequence(rawSequence);

  let rawPem = pubkeyPem;
  if (rawPem === null) {
    if (pubkeyFile) {
      rawPem = fs.readFileSync(pubkeyFile, 'utf8');
    } else {
      rawPem = process.env[pubkeyEnv] ?? '';
    }
  }
  const { pem, fingerprint } = validatePreviewPubkeyPem(rawPem, {
    varName: pubkeyFile ? pubkeyFile : pubkeyEnv,
  });

  if (checkOnly) {
    return {
      ok: true,
      os: targetOs,
      sequence,
      sequenceStr,
      fingerprint,
    };
  }

  if (!outDir) {
    throw new Error('--out is required unless --check-only is passed.');
  }

  const resolvedOut = path.resolve(outDir);
  const { commit, buildRev } = resolveCommit(rawCommit, repoRoot);
  const appDir = path.join(repoRoot, 'apps', 'legion-app');
  const appCargoToml = path.join(appDir, 'src-tauri', 'Cargo.toml');
  const rootCargoToml = path.join(repoRoot, 'Cargo.toml');
  const appVersion = readTomlSectionVersion(fs.readFileSync(appCargoToml, 'utf8'), 'package');
  const nodeVersion = readTomlSectionVersion(
    fs.readFileSync(rootCargoToml, 'utf8'),
    'workspace.package',
  );

  const bundleDir = path.join(appDir, 'src-tauri', 'target', 'release', 'bundle');
  const appReleaseBin = path.join(
    appDir,
    'src-tauri',
    'target',
    'release',
    targetOs === 'windows' ? 'legion-app.exe' : 'legion-app',
  );

  fs.rmSync(resolvedOut, { recursive: true, force: true });
  fs.mkdirSync(resolvedOut, { recursive: true });

  const workParent = process.env.RUNNER_TEMP || path.dirname(resolvedOut) || os.tmpdir();
  fs.mkdirSync(workParent, { recursive: true });
  const workDir = fs.mkdtempSync(path.join(workParent, '.legion-preview-work-'));
  const pubkeyPath = path.join(workDir, 'preview-update.pub.pem');
  const nodeBuildLog = path.join(workDir, 'node-build.log');
  const appBuildLog = path.join(workDir, 'app-build.log');
  const effectiveNodeTargetDir = path.resolve(nodeTargetDir || path.join(workDir, 'node-target'));

  let macosAppArchs = null;
  let macosNodeArchs = null;
  const stagedNames = [];

  try {
    fs.writeFileSync(pubkeyPath, pem, { mode: 0o600 });

    const baseBuildEnv = { ...process.env };
    delete baseBuildEnv[DEFAULT_PUBKEY_ENV];
    delete baseBuildEnv[pubkeyEnv];
    baseBuildEnv.LEGION_PREVIEW_UPDATE_KEY = pubkeyPath;
    baseBuildEnv.LEGION_BUILD_SEQUENCE = sequenceStr;
    baseBuildEnv.LEGION_BUILD_REV = buildRev;

    // 1. Build and verify the platform `legion` node binary with the preview key and sequence.
    const nodeEnv = {
      ...baseBuildEnv,
      CARGO_TARGET_DIR: effectiveNodeTargetDir,
    };
    const nodeArgs = ['build', '--release', '--locked', '--bin', 'legion'];
    let builtNodePath;
    let stagedNodeName;
    if (targetOs === 'windows') {
      const triple = 'x86_64-pc-windows-msvc';
      nodeArgs.push('--target', triple);
      // Match legion-release.yml: statically link the MSVC CRT so Windows machines need no VC++
      // redistributable. Passing `--target` ensures RUSTFLAGS applies only to target crates.
      nodeEnv.RUSTFLAGS = [nodeEnv.RUSTFLAGS, '-C target-feature=+crt-static']
        .filter(Boolean)
        .join(' ');
      builtNodePath = path.join(effectiveNodeTargetDir, triple, 'release', 'legion.exe');
      stagedNodeName = 'legion-windows-x86_64.exe';
    } else if (targetOs === 'macos') {
      const triple = 'aarch64-apple-darwin';
      nodeArgs.push('--target', triple);
      builtNodePath = path.join(effectiveNodeTargetDir, triple, 'release', 'legion');
      stagedNodeName = 'legion-macos-aarch64';
    } else {
      builtNodePath = path.join(effectiveNodeTargetDir, 'release', 'legion');
      stagedNodeName = 'legion-linux-x86_64';
    }

    console.log(`Building preview legion node binary for ${targetOs} (sequence ${sequenceStr}, ${fingerprint})...`);
    await runLoggedCommand('cargo', nodeArgs, {
      cwd: repoRoot,
      env: nodeEnv,
      logPath: nodeBuildLog,
    });
    if (!fs.existsSync(builtNodePath)) {
      throw new Error(`Expected built node binary not found at ${builtNodePath}`);
    }
    const stagedNodePath = path.join(resolvedOut, validateEntryName(stagedNodeName));
    fs.copyFileSync(builtNodePath, stagedNodePath);
    if (targetOs !== 'windows') {
      fs.chmodSync(stagedNodePath, 0o755);
    }
    checkShippedArtifact({
      filePath: stagedNodePath,
      kind: 'binary',
      sequence,
      requireExactlyOne: true,
      expectPreviewKey: fingerprint,
      buildLogPath: nodeBuildLog,
    });
    if (targetOs === 'macos') {
      const nodeArchInfo = inspectMachoArchs(stagedNodePath);
      macosNodeArchs = nodeArchInfo.archs;
      if (!nodeArchInfo.hasArm64) {
        throw new Error(
          `Built macOS node binary ${stagedNodeName} is not arm64 (found: ${nodeArchInfo.archs.join(', ')}).`,
        );
      }
    }
    stagedNames.push(stagedNodeName);

    // 2. Clean prior bundles and build desktop app bundle(s) with the same preview key and sequence.
    removeIfExists(bundleDir);
    const cfg = JSON.stringify(
      tauriBundleConfig({ baseVersion: appVersion, sequence, os: targetOs }),
    );
    const appEnv = { ...baseBuildEnv };
    if (targetOs === 'linux') {
      appEnv.APPIMAGE_EXTRACT_AND_RUN = '1';
    }
    const bundlesArg =
      targetOs === 'linux' ? 'deb,appimage' : targetOs === 'macos' ? 'app,dmg' : 'nsis';

    console.log(`Building preview desktop app (${bundlesArg}) for ${targetOs} (sequence ${sequenceStr}, ${fingerprint})...`);
    await runLoggedCommand(
      'cargo',
      ['tauri', 'build', '--bundles', bundlesArg, '--config', cfg],
      {
        cwd: appDir,
        env: appEnv,
        logPath: appBuildLog,
      },
    );

    if (targetOs === 'linux') {
      const debFile = findExactlyOne(
        path.join(bundleDir, 'deb'),
        (e) => e.isFile() && e.name.endsWith('.deb'),
        '.deb bundle',
      );
      const appImageFile = findExactlyOne(
        path.join(bundleDir, 'appimage'),
        (e) => e.isFile() && e.name.endsWith('.AppImage'),
        '.AppImage bundle',
      );
      checkShippedArtifact({
        filePath: appReleaseBin,
        kind: 'binary',
        sequence,
        requireExactlyOne: true,
        expectPreviewKey: fingerprint,
        buildLogPath: appBuildLog,
      });
      checkShippedArtifact({
        filePath: debFile,
        kind: 'deb',
        sequence,
        requireExactlyOne: true,
        expectPreviewKey: fingerprint,
        buildLogPath: appBuildLog,
      });
      checkShippedArtifact({
        filePath: appImageFile,
        kind: 'appimage',
        sequence,
        requireExactlyOne: true,
        expectPreviewKey: fingerprint,
        buildLogPath: appBuildLog,
      });

      const stagedDebName = validateEntryName(path.basename(debFile));
      const stagedAppImageName = validateEntryName(path.basename(appImageFile));
      fs.copyFileSync(debFile, path.join(resolvedOut, stagedDebName));
      const stagedAppImagePath = path.join(resolvedOut, stagedAppImageName);
      fs.copyFileSync(appImageFile, stagedAppImagePath);
      fs.chmodSync(stagedAppImagePath, 0o755);
      stagedNames.push(stagedDebName, stagedAppImageName);
    } else if (targetOs === 'macos') {
      const appBundle = findExactlyOne(
        path.join(bundleDir, 'macos'),
        (e) => e.isDirectory() && e.name.endsWith('.app'),
        'macOS .app bundle',
      );
      const dmgFile = findExactlyOne(
        path.join(bundleDir, 'dmg'),
        (e) => e.isFile() && e.name.endsWith('.dmg'),
        'macOS .dmg installer',
      );

      // Ensure the .app bundle has a valid ad-hoc signature before archiving app-tar.
      try {
        execFileSync('codesign', ['--verify', '--deep', '--strict', appBundle], {
          stdio: 'ignore',
        });
      } catch {
        execFileSync('codesign', ['--force', '--deep', '--sign', '-', appBundle], {
          stdio: 'inherit',
        });
      }

      const plistText = fs.readFileSync(path.join(appBundle, 'Contents', 'Info.plist'), 'utf8');
      const execMatch = /<key>CFBundleExecutable<\/key>\s*<string>([^<]+)<\/string>/.exec(
        plistText,
      );
      if (!execMatch) {
        throw new Error(`macOS bundle ${appBundle} Contents/Info.plist is missing CFBundleExecutable.`);
      }
      const appExecPath = path.join(appBundle, 'Contents', 'MacOS', execMatch[1].trim());
      const appArchInfo = inspectMachoArchs(appExecPath);
      macosAppArchs = appArchInfo.archs;
      if (!appArchInfo.hasArm64) {
        throw new Error(
          `Built macOS app ${path.basename(appBundle)} is not arm64 (found: ${appArchInfo.archs.join(', ')}).`,
        );
      }

      checkShippedArtifact({
        filePath: appBundle,
        kind: 'app-bundle',
        sequence,
        requireExactlyOne: true,
        expectPreviewKey: fingerprint,
        buildLogPath: appBuildLog,
      });

      const stagedTarName = validateEntryName('Legion-macos-aarch64.app.tar.gz');
      const stagedTarPath = path.join(resolvedOut, stagedTarName);
      execFileSync(
        'tar',
        ['-czf', stagedTarPath, '-C', path.dirname(appBundle), path.basename(appBundle)],
        { stdio: 'inherit' },
      );
      checkShippedArtifact({
        filePath: stagedTarPath,
        kind: 'app-tar',
        sequence,
        requireExactlyOne: true,
        expectPreviewKey: fingerprint,
        buildLogPath: appBuildLog,
      });

      // Verify the .app inside the .dmg carries the same sequence and preview key.
      const dmgMount = path.join(workDir, 'dmg-verify');
      fs.mkdirSync(dmgMount, { recursive: true });
      try {
        execFileSync(
          'hdiutil',
          ['attach', '-readonly', '-nobrowse', '-mountpoint', dmgMount, dmgFile],
          { stdio: 'inherit' },
        );
        const dmgApp = findExactlyOne(
          dmgMount,
          (e) => e.isDirectory() && e.name.endsWith('.app'),
          '.app inside mounted .dmg',
        );
        checkShippedArtifact({
          filePath: dmgApp,
          kind: 'app-bundle',
          sequence,
          requireExactlyOne: true,
          expectPreviewKey: fingerprint,
          buildLogPath: appBuildLog,
        });
      } finally {
        try {
          execFileSync('hdiutil', ['detach', dmgMount, '-force'], { stdio: 'ignore' });
        } catch {
          // Best-effort detach if mount failed before attaching.
        }
      }

      const stagedDmgName = validateEntryName(path.basename(dmgFile));
      fs.copyFileSync(dmgFile, path.join(resolvedOut, stagedDmgName));
      stagedNames.push(stagedTarName, stagedDmgName);
    } else if (targetOs === 'windows') {
      const nsisFile = findExactlyOne(
        path.join(bundleDir, 'nsis'),
        (e) => e.isFile() && e.name.endsWith('.exe'),
        'NSIS installer',
      );
      checkShippedArtifact({
        filePath: appReleaseBin,
        kind: 'binary',
        sequence,
        requireExactlyOne: true,
        expectPreviewKey: fingerprint,
        buildLogPath: appBuildLog,
      });
      checkShippedArtifact({
        filePath: nsisFile,
        kind: 'nsis',
        sequence,
        requireExactlyOne: true,
        expectPreviewKey: fingerprint,
        buildLogPath: appBuildLog,
      });
      const stagedNsisName = validateEntryName(path.basename(nsisFile));
      fs.copyFileSync(nsisFile, path.join(resolvedOut, stagedNsisName));
      stagedNames.push(stagedNsisName);
    }

    // 3. Write PREVIEW-INFO.txt summarizing the build (never key paths or private data).
    const fileEntries = stagedNames.map((name) => {
      const fullPath = path.join(resolvedOut, name);
      return {
        name,
        size: fs.statSync(fullPath).size,
        sha256: sha256FileSync(fullPath),
      };
    });
    const arch = targetOs === 'macos' ? 'aarch64' : 'x86_64';
    const infoText = formatPreviewInfo({
      os: targetOs,
      arch,
      commit,
      buildRev,
      sequence: sequenceStr,
      appVersion,
      nodeVersion,
      fingerprint,
      macosAppArchs,
      macosNodeArchs,
      files: fileEntries,
    });
    const infoPath = path.join(resolvedOut, 'PREVIEW-INFO.txt');
    fs.writeFileSync(infoPath, infoText, { mode: 0o644 });
    console.log('');
    console.log(infoText.trimEnd());

    return {
      ok: true,
      os: targetOs,
      arch,
      commit,
      buildRev,
      sequence,
      sequenceStr,
      fingerprint,
      outDir: resolvedOut,
      infoPath,
      files: fileEntries,
    };
  } finally {
    // Never leave preview-keyed installers or release app binaries in cached src-tauri/target/.
    removeIfExists(bundleDir);
    removeIfExists(appReleaseBin);
    removeIfExists(workDir);
  }
}

function usage() {
  return [
    'Usage:',
    '  node scripts/build-desktop-preview.mjs --os <linux|macos|windows> --sequence <YYYYMMDDHHMM> --out <dir> [--commit <sha>] [--pubkey-env <VAR>] [--pubkey-file <path>] [--node-target-dir <dir>] [--check-only]',
  ].join('\n');
}

async function runCli(argv) {
  const opts = {};
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '-h' || arg === '--help') {
      console.log(usage());
      return;
    }
    if (arg === '--os') opts.os = argv[++i];
    else if (arg === '--sequence') opts.sequence = argv[++i];
    else if (arg === '--out') opts.outDir = argv[++i];
    else if (arg === '--commit') opts.commit = argv[++i];
    else if (arg === '--pubkey-env') opts.pubkeyEnv = argv[++i];
    else if (arg === '--pubkey-file') opts.pubkeyFile = argv[++i];
    else if (arg === '--node-target-dir') opts.nodeTargetDir = argv[++i];
    else if (arg === '--check-only') opts.checkOnly = true;
    else throw new Error(`Unknown option: ${arg}`);
  }
  const res = await buildDesktopPreview(opts);
  if (opts.checkOnly) {
    console.log(
      `Preview build inputs valid for ${res.os}: sequence ${res.sequenceStr}, key ${res.fingerprint}.`,
    );
  }
}

const real = (p) => {
  try {
    return fs.realpathSync(p);
  } catch {
    return path.resolve(p);
  }
};
const sameFile = (a, b) =>
  process.platform === 'win32'
    ? real(a).toLowerCase() === real(b).toLowerCase()
    : real(a) === real(b);

if (process.argv[1] && sameFile(process.argv[1], fileURLToPath(import.meta.url))) {
  runCli(process.argv.slice(2)).catch((error) => {
    console.error(error.message);
    process.exitCode = 1;
  });
}
