#!/usr/bin/env node
// Sets up a throwaway local Legion node and sample media fixtures (16x16 PNG + sample.webm) for
// CI content-address media verification across Linux, macOS, Windows, iOS, and Android.
//
// Usage:
//   node scripts/ci-media-fixture.mjs local --work <dir> --legion <bin> [--out <report.json>]
//   node scripts/ci-media-fixture.mjs ssh   --work <dir> --legion <bin> --host <127.0.0.1|10.0.2.2> [--sshd <path>] [--out <report.json>] [--no-out-file]

import { execFileSync, spawn } from 'node:child_process';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import zlib from 'node:zlib';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(__dirname, '..');
const SAMPLE_WEBM = path.join(REPO_ROOT, 'apps', 'ui', 'test', 'fixtures', 'sample.webm');

function parseArgs(argv) {
  const mode = argv[2];
  const opts = {};
  for (let i = 3; i < argv.length; i += 1) {
    const key = argv[i];
    if (!key.startsWith('--')) continue;
    const name = key.slice(2);
    if (name === 'no-out-file') {
      opts.noOutFile = true;
      continue;
    }
    opts[name] = argv[i + 1] ?? '';
    i += 1;
  }
  return { mode, opts };
}

function crc32(buf) {
  let c = 0xffffffff;
  for (let i = 0; i < buf.length; i += 1) {
    c ^= buf[i];
    for (let k = 0; k < 8; k += 1) {
      c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    }
  }
  return (c ^ 0xffffffff) >>> 0;
}

function pngChunk(type, data) {
  const typeBuf = Buffer.from(type, 'ascii');
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length, 0);
  const body = Buffer.concat([typeBuf, data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body), 0);
  return Buffer.concat([len, body, crc]);
}

function makeSolidPng(width, height, rgba) {
  const sig = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8;
  ihdr[9] = 6;
  ihdr[10] = 0;
  ihdr[11] = 0;
  ihdr[12] = 0;

  const stride = 1 + width * 4;
  const raw = Buffer.alloc(stride * height);
  for (let y = 0; y < height; y += 1) {
    const rowStart = y * stride;
    raw[rowStart] = 0;
    for (let x = 0; x < width; x += 1) {
      const px = rowStart + 1 + x * 4;
      raw[px] = rgba[0];
      raw[px + 1] = rgba[1];
      raw[px + 2] = rgba[2];
      raw[px + 3] = rgba[3];
    }
  }
  const idat = zlib.deflateSync(raw);
  return Buffer.concat([
    sig,
    pngChunk('IHDR', ihdr),
    pngChunk('IDAT', idat),
    pngChunk('IEND', Buffer.alloc(0)),
  ]);
}

function pickFreePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.on('error', reject);
    srv.listen(0, '127.0.0.1', () => {
      const addr = srv.address();
      const port = typeof addr === 'object' && addr ? addr.port : 0;
      srv.close(() => resolve(port));
    });
  });
}

function waitForPort(port, timeoutMs = 10000) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolve, reject) => {
    const tryOnce = () => {
      const sock = net.createConnection({ host: '127.0.0.1', port });
      sock.once('connect', () => {
        sock.destroy();
        resolve();
      });
      sock.once('error', () => {
        sock.destroy();
        if (Date.now() > deadline) {
          reject(new Error(`Timed out waiting for sshd on 127.0.0.1:${port}`));
        } else {
          setTimeout(tryOnce, 100);
        }
      });
    };
    tryOnce();
  });
}

function prepareMedia(workDir) {
  const mediaDir = path.join(workDir, 'media');
  fs.mkdirSync(mediaDir, { recursive: true });
  const imagePath = path.join(mediaDir, 'sample.png');
  const videoPath = path.join(mediaDir, 'sample.webm');
  fs.writeFileSync(imagePath, makeSolidPng(16, 16, [45, 136, 255, 255]));
  fs.copyFileSync(SAMPLE_WEBM, videoPath);
  return { mediaDir, imagePath, videoPath };
}

function tomlEscape(value) {
  return value.replace(/\\/g, '\\\\').replace(/"/g, '\\"');
}

function findSshd(explicit) {
  if (explicit && fs.existsSync(explicit)) return explicit;
  for (const cand of ['/usr/sbin/sshd', '/usr/bin/sshd', '/sbin/sshd']) {
    if (fs.existsSync(cand)) return cand;
  }
  throw new Error('Could not find sshd binary');
}

function supportsPerSourcePenalties(sshdBin, hostKey) {
  try {
    execFileSync(sshdBin, ['-T', '-o', 'PerSourcePenalties=no', '-h', hostKey], {
      stdio: 'ignore',
    });
    return true;
  } catch {
    return false;
  }
}

async function main() {
  const { mode, opts } = parseArgs(process.argv);
  if (!mode || !opts.work || !opts.legion) {
    console.error(
      'Usage: ci-media-fixture.mjs <local|ssh> --work <dir> --legion <bin> [--host <ip>] [--out <file>] [--no-out-file]',
    );
    process.exit(1);
  }

  const workDir = path.resolve(opts.work);
  const legionBin = path.resolve(opts.legion);
  fs.mkdirSync(workDir, { recursive: true });
  const nodeHome = path.join(workDir, 'node');
  fs.mkdirSync(nodeHome, { recursive: true });

  const { mediaDir, imagePath, videoPath } = prepareMedia(workDir);
  const env = { ...process.env, LEGION_HOME: nodeHome };

  execFileSync(legionBin, ['init', '--name', 'tower'], { env, stdio: ['ignore', 'ignore', 'inherit'] });

  const specPath = path.join(workDir, 'media-check.json');
  const outFile = opts.out ? path.resolve(opts.out) : path.join(workDir, 'media-check-result.json');

  if (mode === 'local') {
    const spec = {
      machine: 'tower',
      image_path: imagePath,
      video_path: videoPath,
      ...(opts.noOutFile ? {} : { out_file: outFile }),
    };
    fs.writeFileSync(specPath, `${JSON.stringify(spec, null, 2)}\n`);
    console.log(JSON.stringify({ nodeHome, specPath, outFile, imagePath, videoPath }));
    return;
  }

  if (mode === 'ssh') {
    const mediaGlob = tomlEscape(path.join(mediaDir, '**'));
    const policy = [
      '[roles.observer]',
      'allow = ["effect:read"]',
      `paths = ["${mediaGlob}"]`,
      '',
      '[roles.operator]',
      'allow = ["effect:read", "effect:write"]',
      `paths = ["${mediaGlob}"]`,
      '',
    ].join('\n');
    fs.writeFileSync(path.join(nodeHome, 'config', 'policy.toml'), policy);

    const connectHost = opts.host || '127.0.0.1';
    const sshdBin = findSshd(opts.sshd);
    const port = await pickFreePort();
    const user = os.userInfo().username;
    const hostKey = path.join(workDir, 'host_ed25519');
    const authKeys = path.join(workDir, 'authorized_keys');
    const pidFile = path.join(workDir, 'sshd.pid');
    const sshdCfg = path.join(workDir, 'sshd_config');
    const sshdLog = path.join(workDir, 'sshd.log');

    execFileSync('ssh-keygen', ['-q', '-t', 'ed25519', '-N', '', '-C', 'ci-media-host', '-f', hostKey]);
    fs.chmodSync(hostKey, 0o600);
    fs.writeFileSync(authKeys, '');
    fs.chmodSync(authKeys, 0o600);

    const nodeTomlPath = path.join(nodeHome, 'config', 'node.toml');
    const extraNode = [
      '',
      '[devices]',
      `hosts = ["${tomlEscape(connectHost)}"]`,
      `port = ${port}`,
      `user = "${tomlEscape(user)}"`,
      `authorized_keys = "${tomlEscape(authKeys)}"`,
      `host_keys = ["${tomlEscape(`${hostKey}.pub`)}"]`,
      '',
    ].join('\n');
    fs.appendFileSync(nodeTomlPath, extraNode);

    const penalties = supportsPerSourcePenalties(sshdBin, hostKey) ? 'PerSourcePenalties no' : '';
    const sshdConfigText = [
      'ListenAddress 127.0.0.1',
      `Port ${port}`,
      `HostKey "${hostKey}"`,
      `PidFile none`,
      `AuthorizedKeysFile ${authKeys}`,
      `AllowUsers "${user}"`,
      `SetEnv "LEGION_HOME=${nodeHome}"`,
      'PubkeyAuthentication yes',
      'PasswordAuthentication no',
      'KbdInteractiveAuthentication no',
      'PermitRootLogin no',
      'UsePAM no',
      'StrictModes no',
      'PrintMotd no',
      'PrintLastLog no',
      penalties,
      'LogLevel INFO',
      '',
    ]
      .filter(Boolean)
      .join('\n');
    fs.writeFileSync(sshdCfg, `${sshdConfigText}\n`);

    const child = spawn(sshdBin, ['-D', '-e', '-f', sshdCfg, '-E', sshdLog], {
      detached: true,
      stdio: 'ignore',
    });
    child.unref();
    fs.writeFileSync(pidFile, `${child.pid}\n`);

    await waitForPort(port, 10000);

    const pairRaw = execFileSync(
      legionBin,
      ['pair', '--name', 'ci-media', '--role', 'operator', '--valid', '15m', '--json'],
      { env, encoding: 'utf8' },
    );
    const pairJson = JSON.parse(pairRaw);
    const invite = pairJson.invite || pairJson.token;
    if (!invite) {
      throw new Error(`legion pair did not return an invite: ${pairRaw}`);
    }

    const spec = {
      machine: 'tower',
      invite,
      image_path: imagePath,
      video_path: videoPath,
      ...(opts.noOutFile ? {} : { out_file: outFile }),
    };
    fs.writeFileSync(specPath, `${JSON.stringify(spec, null, 2)}\n`);
    console.log(
      JSON.stringify({
        nodeHome,
        specPath,
        outFile,
        imagePath,
        videoPath,
        port,
        sshdPid: child.pid,
        pidFile,
      }),
    );
    return;
  }

  throw new Error(`Unknown mode: ${mode}`);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
