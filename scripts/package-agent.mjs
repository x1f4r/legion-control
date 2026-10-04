#!/usr/bin/env node
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import zlib from 'node:zlib';
import { fileURLToPath } from 'node:url';
import { artifactDigest, signManifest, verifyManifest, verifyArtifact } from './release-integrity.mjs';
import { agentFloorProblem } from './check-versions.mjs';

const repository = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

// The agent tarball is written here rather than by `tar -czf`, so that one staged tree always
// gives the same bytes. tar kept each file's mtime from the staging copy, the builder's uid, gid
// and user name, and gzip stamped the time it ran, so two builds of one commit never matched and
// nobody could check a published bundle by building it again. Here every entry is a plain ustar
// header, in byte order of its path, owned by 0 with no user or group name, stamped with one fixed
// time and given 0755 or 0644 and nothing else. The agent reads it with its own parser
// (agent/src/archive.mjs) and every client unpacks it with the system tar; all of them read ustar.

/** 1980-01-01T00:00:00Z, the time desktop/build.sh stamps on every file of the desktop archives. */
export const ARCHIVE_MTIME = 315532800;
const BLOCK = 512;

/** A ustar number: octal digits, zero padded to fill the field but its closing NUL. */
const octal = (value, width) => value.toString(8).padStart(width - 1, '0') + '\0';

/**
 * Splits a path into the ustar prefix and name fields. A name holds 100 bytes; a longer path is
 * stored as a prefix of up to 155 bytes, cut at a slash, and the name after it.
 */
function ustarName(name) {
  if (Buffer.byteLength(name) <= 100) return ['', name];
  for (let slash = name.indexOf('/'); slash !== -1; slash = name.indexOf('/', slash + 1)) {
    const [prefix, rest] = [name.slice(0, slash), name.slice(slash + 1)];
    if (rest && Buffer.byteLength(prefix) <= 155 && Buffer.byteLength(rest) <= 100) return [prefix, rest];
  }
  throw new Error(`Agent package path is too long for a ustar header: ${name}`);
}

function ustarHeader({ name, mode, size, directory }) {
  const header = Buffer.alloc(BLOCK);
  const [prefix, base] = ustarName(name);
  header.write(base, 0, 100, 'utf8');
  header.write(octal(mode, 8), 100, 'ascii');
  header.write(octal(0, 8), 108, 'ascii'); // uid
  header.write(octal(0, 8), 116, 'ascii'); // gid
  header.write(octal(size, 12), 124, 'ascii');
  header.write(octal(ARCHIVE_MTIME, 12), 136, 'ascii');
  header.fill(0x20, 148, 156); // the checksum counts its own field as spaces
  header.write(directory ? '5' : '0', 156, 'ascii');
  header.write('ustar\0' + '00', 257, 'ascii');
  // The user and group names stay empty, so an unpacking root falls back on uid and gid 0.
  header.write(octal(0, 8), 329, 'ascii'); // devmajor
  header.write(octal(0, 8), 337, 'ascii'); // devminor
  header.write(prefix, 345, 155, 'utf8');
  let sum = 0;
  for (const byte of header) sum += byte;
  header.write(sum.toString(8).padStart(6, '0') + '\0 ', 148, 'ascii');
  return header;
}

/**
 * The gzipped ustar archive of `directory`, its entries named under `top`. Only regular files and
 * directories are expected; the staging copy has nothing else in it.
 */
export function reproducibleTarball(directory, top) {
  const entries = [];
  (function walk(relative) {
    const input = path.join(directory, relative);
    const stat = fs.lstatSync(input);
    const name = path.posix.join(top, relative.split(path.sep).join('/'));
    if (stat.isDirectory()) {
      entries.push({ name: name + '/', mode: 0o755, size: 0, directory: true });
      for (const child of fs.readdirSync(input)) walk(path.join(relative, child));
    } else if (stat.isFile()) {
      entries.push({ name, mode: stat.mode & 0o111 ? 0o755 : 0o644, size: stat.size, input });
    } else throw new Error(`Unsupported agent package entry: ${relative}`);
  })('');
  // Byte order of the full path, which no file system or locale has a say in. A directory still
  // comes before everything in it, because its name with the slash is a prefix of theirs.
  entries.sort((a, b) => Buffer.compare(Buffer.from(a.name), Buffer.from(b.name)));
  const blocks = [];
  for (const entry of entries) {
    blocks.push(ustarHeader(entry));
    if (entry.directory) continue;
    const body = fs.readFileSync(entry.input);
    if (body.length !== entry.size) throw new Error(`Agent package file changed while it was archived: ${entry.name}`);
    blocks.push(body, Buffer.alloc((BLOCK - (body.length % BLOCK)) % BLOCK));
  }
  blocks.push(Buffer.alloc(2 * BLOCK));
  const gzip = zlib.gzipSync(Buffer.concat(blocks), { level: 9 });
  // zlib leaves the time and the file name out of the header already, but writes the operating
  // system it was compiled for into byte 9. 3, Unix, is what `gzip -n` writes on a Mac or Linux,
  // and it keeps a build on another system from differing in that byte alone.
  gzip[9] = 3;
  return gzip;
}

export async function packageAgent({ source = path.join(repository, 'agent'), destination = path.join(repository, 'dist'), privateKey = path.join(os.homedir(), '.legion-control/release-signing.key'), publicKey = path.join(repository, 'contract/release-public-key.pem'), clientVersion } = {}) {
  if (!clientVersion) {
    const project = fs.readFileSync(path.join(repository, 'desktop/LegionControl.Desktop/LegionControl.Desktop.csproj'), 'utf8');
    clientVersion = project.match(/<Version>([^<]+)<\/Version>/)?.[1];
  }
  if (!/^\d+\.\d+\.\d+$/.test(clientVersion ?? '')) throw new Error('Client version must be a stable semantic version.');
  const agentPackage = JSON.parse(fs.readFileSync(path.join(source, 'package.json'), 'utf8'));
  const { version } = agentPackage;
  if (!/^\d+\.\d+\.\d+$/.test(version)) throw new Error('Agent version must be a stable semantic version.');
  // The floor travels inside the signed package.json, so a bundle that gets it wrong cannot be
  // corrected once it is out. Refuse it before anything is staged or signed.
  const floorProblem = agentFloorProblem(agentPackage);
  if (floorProblem) throw new Error(`${floorProblem} Nothing was packaged.`);
  if (!fs.existsSync(privateKey)) throw new Error('Release signing key missing. See docs/security.md.');
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-agent-package-'));
  const stage = path.join(scratch, 'agent');
  fs.mkdirSync(stage);
  const files = [];
  try {
    async function copy(relative) {
      const input = path.join(source, relative);
      const stat = fs.lstatSync(input);
      if (stat.isSymbolicLink()) throw new Error(`Agent package cannot contain symlinks: ${relative}`);
      if (stat.isDirectory()) {
        fs.mkdirSync(path.join(stage, relative), { recursive: true });
        for (const name of fs.readdirSync(input).sort()) await copy(path.posix.join(relative, name));
      } else if (stat.isFile()) {
        fs.copyFileSync(input, path.join(stage, relative));
        fs.chmodSync(path.join(stage, relative), stat.mode & 0o111 ? 0o755 : 0o644);
        files.push({ path: relative, ...await artifactDigest(path.join(stage, relative)) });
      } else throw new Error(`Unsupported agent package entry: ${relative}`);
    }
    for (const relative of ['src', 'install', 'package.json']) await copy(relative);
    files.sort((a, b) => a.path.localeCompare(b.path, 'en'));
    const manifest = path.join(stage, 'MANIFEST.json');
    fs.writeFileSync(manifest, JSON.stringify({ schema: 1, version, contract: 3, files }, null, 2) + '\n');
    signManifest(manifest, privateKey, manifest + '.sig');
    verifyManifest(manifest, manifest + '.sig', publicKey);
    for (const file of files) await verifyArtifact(path.join(stage, file.path), file);
    fs.mkdirSync(destination, { recursive: true });
    const name = `legionctl-agent-${version}.tgz`;
    const temporaryArchive = path.join(scratch, name);
    fs.writeFileSync(temporaryArchive, reproducibleTarball(stage, 'agent'));
    const artifact = { name, ...await artifactDigest(temporaryArchive) };
    const releaseManifest = path.join(scratch, 'Legion-Control-agent-manifest.json');
    fs.writeFileSync(releaseManifest, JSON.stringify({ schema: 1, version: clientVersion, agentVersion: version, artifacts: [artifact] }, null, 2) + '\n');
    signManifest(releaseManifest, privateKey, releaseManifest + '.sig');
    verifyManifest(releaseManifest, releaseManifest + '.sig', publicKey);
    for (const file of [temporaryArchive, releaseManifest, releaseManifest + '.sig']) fs.copyFileSync(file, path.join(destination, path.basename(file)));
    return { archive: path.join(destination, name), manifest: path.join(destination, path.basename(releaseManifest)), artifact };
  } finally { fs.rmSync(scratch, { recursive: true, force: true }); }
}
// Compared through realpath: Node resolves the symlinks in import.meta.url but not in argv[1], so
// run through a symlinked checkout a plain comparison would package nothing and still exit 0.
const real = (file) => { try { return fs.realpathSync(file); } catch { return path.resolve(file); } };
if (process.argv[1] && real(process.argv[1]) === real(fileURLToPath(import.meta.url))) {
  try { console.log(JSON.stringify(await packageAgent(), null, 2)); }
  catch (error) { console.error(error.message); process.exitCode = 1; }
}
