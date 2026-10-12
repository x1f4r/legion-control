// Signed trees and bundles for the self-update and version-floor tests.
//
// The release key's private half is not available to the tests, so each test
// that needs a signature makes a key pair of its own and runs an isolated copy
// of the agent whose trust module pins that key instead. Production code keeps
// the real key; only the copy in a temporary directory trusts the test one.

import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import zlib from 'node:zlib';
import { fileURLToPath, pathToFileURL } from 'node:url';

export const AGENT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

export function signingPair() { return crypto.generateKeyPairSync('ed25519'); }
export const sha = (bytes) => crypto.createHash('sha256').update(bytes).digest('hex');

/** Write MANIFEST.json and its signature for every file under src/, install/ and package.json. */
export function signTree(dir, privateKey) {
  const files = [];
  const walk = (relative) => {
    const absolute = path.join(dir, relative);
    for (const name of fs.readdirSync(absolute).sort()) {
      const child = path.posix.join(relative, name);
      const stat = fs.lstatSync(path.join(dir, child));
      if (stat.isDirectory()) walk(child);
      else if (child !== 'MANIFEST.json' && child !== 'MANIFEST.json.sig') {
        const bytes = fs.readFileSync(path.join(dir, child));
        files.push({ path: child, sha256: sha(bytes), size: bytes.length });
      }
    }
  };
  for (const relative of ['src', 'install']) if (fs.existsSync(path.join(dir, relative))) walk(relative);
  for (const relative of ['package.json']) {
    if (!fs.existsSync(path.join(dir, relative))) continue;
    const bytes = fs.readFileSync(path.join(dir, relative));
    files.push({ path: relative, sha256: sha(bytes), size: bytes.length });
  }
  files.sort((a, b) => a.path.localeCompare(b.path));
  const version = JSON.parse(fs.readFileSync(path.join(dir, 'package.json'), 'utf8')).version;
  const manifest = Buffer.from(`${JSON.stringify({ schema: 1, version, contract: 3, files }, null, 2)}\n`);
  fs.writeFileSync(path.join(dir, 'MANIFEST.json'), manifest);
  fs.writeFileSync(path.join(dir, 'MANIFEST.json.sig'), `${crypto.sign(null, manifest, privateKey).toString('base64')}\n`);
}

/** Make a copied trust.mjs pin `publicKey` instead of the release key. */
export function patchTrust(src, publicKey) {
  const file = path.join(src, 'trust.mjs');
  const pemBody = publicKey.export({ type: 'spki', format: 'pem' }).toString().split('\n')[1];
  const original = fs.readFileSync(file, 'utf8');
  fs.writeFileSync(file, original.replace(/  'MCowBQYDK2VwAyEA[^']+',/, `  '${pemBody}',`));
}

/** Make a copied launcher.mjs pin `publicKey` instead of the release key. */
export function patchLauncherTrust(file, publicKey) {
  const pem = publicKey.export({ type: 'spki', format: 'pem' }).toString();
  const original = fs.readFileSync(file, 'utf8');
  fs.writeFileSync(file, original.replace(/const PUBLIC_KEY = \[[\s\S]*?\]\.join\('\\n'\);/, () => `const PUBLIC_KEY = ${JSON.stringify(pem)};`));
}

/** A gzipped tar of `directory` under agent/, the shape package-agent.mjs produces. */
export function bundleTree(directory) {
  const parts = [];
  const walk = (relative = '') => {
    for (const name of fs.readdirSync(path.join(directory, relative))) {
      const child = relative ? `${relative}/${name}` : name;
      if (fs.statSync(path.join(directory, child)).isDirectory()) { walk(child); continue; }
      const bytes = fs.readFileSync(path.join(directory, child));
      const header = Buffer.alloc(512);
      header.write(`agent/${child}`, 0, 100);
      header.write('0000644\0', 100);
      header.write(`${bytes.length.toString(8).padStart(11, '0')}\0`, 124);
      header.write('        ', 148);
      header.write('0', 156);
      header.write('ustar\0', 257);
      header.write('00', 263);
      const checksum = header.reduce((sum, byte) => sum + byte, 0);
      header.write(`${checksum.toString(8).padStart(6, '0')}\0 `, 148);
      parts.push(header, bytes, Buffer.alloc((512 - bytes.length % 512) % 512));
    }
  };
  walk();
  return zlib.gzipSync(Buffer.concat([...parts, Buffer.alloc(1024)]));
}

/**
 * A copy of the agent in a temporary directory that trusts a key of its own,
 * with its package.json beside src/ as in every real tree, and its
 * selfupdate.mjs imported fresh. `patch(src)` may edit the copy first.
 */
export async function isolatedModule({ patch = null } = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-selfupdate-module-'));
  const src = path.join(root, 'src');
  fs.cpSync(path.join(AGENT, 'src'), src, { recursive: true });
  fs.copyFileSync(path.join(AGENT, 'package.json'), path.join(root, 'package.json'));
  const pair = signingPair();
  patchTrust(src, pair.publicKey);
  if (patch) patch(src);
  const module = await import(`${pathToFileURL(path.join(src, 'selfupdate.mjs')).href}?${Date.now()}-${Math.random()}`);
  return { root, src, pair, module };
}
