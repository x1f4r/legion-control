import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import zlib from 'node:zlib';

import {
  androidVersionCode,
  createPreviewKey,
  currentSequence,
  desktopBundleVersion,
  macosBundleVersion,
  makeUpdateSet,
  parseArtifactSpec,
  tauriBundleConfig,
  validateCreateSequence,
  verifyArtifactSequence,
  verifyUpdateSet,
} from '../scripts/make-update-set.mjs';

// ── Minimal binary fixture builders (no external binaries required) ─────────────────────────

/** Build a valid minimal Android binary XML (AXML) buffer containing `<manifest android:versionCode>`. */
function buildMinimalAxml(versionCode) {
  const strings = ['manifest', 'versionCode'];
  const stringBlobs = strings.map((s) => {
    const utf8 = Buffer.from(s, 'utf8');
    return Buffer.concat([Buffer.from([s.length, utf8.length]), utf8, Buffer.from([0])]);
  });
  let offset = 0;
  const offsets = Buffer.alloc(strings.length * 4);
  for (let i = 0; i < stringBlobs.length; i += 1) {
    offsets.writeUInt32LE(offset, i * 4);
    offset += stringBlobs[i].length;
  }
  const stringData = Buffer.concat(stringBlobs);
  const padLen = (4 - (stringData.length % 4)) % 4;
  const spHeaderSize = 28;
  const spStringsStart = spHeaderSize + offsets.length;
  const spChunkSize = spStringsStart + stringData.length + padLen;
  const sp = Buffer.alloc(spChunkSize);
  sp.writeUInt16LE(0x0001, 0); // RES_STRING_POOL_TYPE
  sp.writeUInt16LE(spHeaderSize, 2);
  sp.writeUInt32LE(spChunkSize, 4);
  sp.writeUInt32LE(strings.length, 8);
  sp.writeUInt32LE(0, 12); // styleCount
  sp.writeUInt32LE(1 << 8, 16); // UTF8_FLAG
  sp.writeUInt32LE(spStringsStart, 20);
  sp.writeUInt32LE(0, 24);
  offsets.copy(sp, spHeaderSize);
  stringData.copy(sp, spStringsStart);

  // Resource ID map chunk (0x0180): index 0 -> 0, index 1 -> 0x0101021b (android:versionCode)
  const resMap = Buffer.alloc(16);
  resMap.writeUInt16LE(0x0180, 0);
  resMap.writeUInt16LE(8, 2);
  resMap.writeUInt32LE(16, 4);
  resMap.writeUInt32LE(0, 8);
  resMap.writeUInt32LE(0x0101021b, 12);

  // Start element chunk (0x0102): `<manifest android:versionCode="versionCode">`
  const startElem = Buffer.alloc(16 + 20 + 20);
  startElem.writeUInt16LE(0x0102, 0);
  startElem.writeUInt16LE(16, 2);
  startElem.writeUInt32LE(startElem.length, 4);
  startElem.writeUInt32LE(1, 8); // lineNumber
  startElem.writeInt32LE(-1, 12); // comment
  startElem.writeInt32LE(-1, 16); // ns
  startElem.writeInt32LE(0, 20); // name = "manifest"
  startElem.writeUInt16LE(20, 24); // attrStart
  startElem.writeUInt16LE(20, 26); // attrSize
  startElem.writeUInt16LE(1, 28); // attrCount
  // Attribute 0 at offset 36
  startElem.writeInt32LE(-1, 36); // ns
  startElem.writeInt32LE(1, 40); // name = "versionCode"
  startElem.writeInt32LE(-1, 44); // rawValue
  startElem.writeUInt16LE(8, 48); // typedValue.size
  startElem.writeUInt8(0, 50); // res0
  startElem.writeUInt8(0x10, 51); // TYPE_INT_DEC
  startElem.writeInt32LE(versionCode, 52);

  const body = Buffer.concat([sp, resMap, startElem]);
  const root = Buffer.alloc(8);
  root.writeUInt16LE(0x0003, 0); // RES_XML_TYPE
  root.writeUInt16LE(8, 2);
  root.writeUInt32LE(8 + body.length, 4);
  return Buffer.concat([root, body]);
}

/** Build a valid minimal ZIP archive (deflated) containing `entries` (`[{ name, data }]`). */
function buildZip(entries) {
  const localParts = [];
  const cdParts = [];
  let offset = 0;
  for (const { name, data } of entries) {
    const nameBuf = Buffer.from(name, 'utf8');
    const comp = zlib.deflateRawSync(data);
    const lfh = Buffer.alloc(30);
    lfh.writeUInt32LE(0x04034b50, 0);
    lfh.writeUInt16LE(20, 4);
    lfh.writeUInt16LE(8, 8); // deflate
    lfh.writeUInt32LE(comp.length, 18);
    lfh.writeUInt32LE(data.length, 22);
    lfh.writeUInt16LE(nameBuf.length, 26);
    localParts.push(lfh, nameBuf, comp);

    const cdh = Buffer.alloc(46);
    cdh.writeUInt32LE(0x02014b50, 0);
    cdh.writeUInt16LE(20, 4);
    cdh.writeUInt16LE(20, 6);
    cdh.writeUInt16LE(8, 10);
    cdh.writeUInt32LE(comp.length, 20);
    cdh.writeUInt32LE(data.length, 24);
    cdh.writeUInt16LE(nameBuf.length, 28);
    cdh.writeUInt32LE(offset, 42);
    cdParts.push(cdh, nameBuf);
    offset += lfh.length + nameBuf.length + comp.length;
  }
  const cd = Buffer.concat(cdParts);
  const eocd = Buffer.alloc(22);
  eocd.writeUInt32LE(0x06054b50, 0);
  eocd.writeUInt16LE(entries.length, 8);
  eocd.writeUInt16LE(entries.length, 10);
  eocd.writeUInt32LE(cd.length, 12);
  eocd.writeUInt32LE(offset, 16);
  return Buffer.concat([...localParts, cd, eocd]);
}

/** Build a minimal POSIX USTAR tar buffer from `[{ name, data }]`. */
function buildTar(entries) {
  const parts = [];
  for (const { name, data } of entries) {
    const hdr = Buffer.alloc(512, 0);
    Buffer.from(name, 'utf8').copy(hdr, 0, 0, Math.min(100, name.length));
    Buffer.from('0000644\0', 'ascii').copy(hdr, 100);
    Buffer.from('0000000\0', 'ascii').copy(hdr, 108);
    Buffer.from('0000000\0', 'ascii').copy(hdr, 116);
    Buffer.from(`${data.length.toString(8).padStart(11, '0')}\0`, 'ascii').copy(hdr, 124);
    Buffer.from('00000000000\0', 'ascii').copy(hdr, 136);
    hdr.fill(0x20, 148, 156);
    hdr[156] = 0x30; // '0' regular file
    Buffer.from('ustar\0', 'ascii').copy(hdr, 257);
    Buffer.from('00', 'ascii').copy(hdr, 263);
    let cksum = 0;
    for (let i = 0; i < 512; i += 1) cksum += hdr[i];
    Buffer.from(`${cksum.toString(8).padStart(6, '0')}\0 `, 'ascii').copy(hdr, 148);
    parts.push(hdr, data);
    const pad = (512 - (data.length % 512)) % 512;
    if (pad) parts.push(Buffer.alloc(pad, 0));
  }
  parts.push(Buffer.alloc(1024, 0));
  return Buffer.concat(parts);
}

/** Build a Debian `.deb` `ar` archive from `[{ name, data }]`. */
function buildDeb(members) {
  const parts = [Buffer.from('!<arch>\n', 'ascii')];
  for (const { name, data } of members) {
    const hdr = Buffer.alloc(60, 0x20);
    Buffer.from(name, 'ascii').copy(hdr, 0);
    Buffer.from('0', 'ascii').copy(hdr, 16);
    Buffer.from('0', 'ascii').copy(hdr, 28);
    Buffer.from('0', 'ascii').copy(hdr, 34);
    Buffer.from('100644', 'ascii').copy(hdr, 40);
    Buffer.from(String(data.length), 'ascii').copy(hdr, 48);
    hdr[58] = 0x60;
    hdr[59] = 0x0a;
    parts.push(hdr, data);
    if (data.length % 2 === 1) parts.push(Buffer.from('\n', 'ascii'));
  }
  return Buffer.concat(parts);
}

/** Build a tiny valid SquashFS 4.0 image (gzip) with a root `.desktop` file behind an ELF prefix. */
function buildMiniAppImage(desktopText) {
  const elfStub = Buffer.alloc(128, 0);
  Buffer.from('\x7fELF', 'ascii').copy(elfStub, 0);

  const fileBytes = Buffer.from(desktopText, 'utf8');
  const compFileBlock = zlib.deflateSync(fileBytes);

  // Data block starts right after the 96-byte superblock.
  const dataStart = 96;
  const inodeTableStart = dataStart + compFileBlock.length;

  // Inode 0 (offset 0): basic regular file (type 2), 32 + 4 = 36 bytes.
  const fileInode = Buffer.alloc(36);
  fileInode.writeUInt16LE(2, 0); // basic regular file
  fileInode.writeUInt32LE(dataStart, 16); // blocks_start
  fileInode.writeUInt32LE(0xffffffff, 20); // no fragment
  fileInode.writeUInt32LE(0, 24);
  fileInode.writeUInt32LE(fileBytes.length, 28); // file_size
  fileInode.writeUInt32LE(compFileBlock.length, 32); // block_size[0] (compressed)

  // Directory entry stream for root: 12-byte header + 8-byte entry + name "legion-app.desktop"
  const nameBuf = Buffer.from('legion-app.desktop', 'utf8');
  const dirData = Buffer.alloc(12 + 8 + nameBuf.length);
  dirData.writeUInt32LE(0, 0); // count - 1 = 0 (1 entry)
  dirData.writeUInt32LE(0, 4); // start_block = 0
  dirData.writeUInt32LE(1, 8); // inode_number
  dirData.writeUInt16LE(0, 12); // offset inside inode metadata block = 0
  dirData.writeInt16LE(0, 14); // inode_offset
  dirData.writeUInt16LE(2, 16); // type = regular file
  dirData.writeUInt16LE(nameBuf.length - 1, 18); // name_size - 1
  nameBuf.copy(dirData, 20);

  // Inode 1 (offset 36): basic directory (type 1), 32 bytes.
  const rootInode = Buffer.alloc(32);
  rootInode.writeUInt16LE(1, 0); // basic dir
  rootInode.writeUInt32LE(0, 16); // block_index
  rootInode.writeUInt32LE(2, 20); // link_count
  rootInode.writeUInt16LE(dirData.length + 3, 24); // file_size (+3 for . and ..)
  rootInode.writeUInt16LE(0, 26); // block_offset

  const inodeBlockRaw = Buffer.concat([fileInode, rootInode]);
  const inodeBlockComp = zlib.deflateSync(inodeBlockRaw);
  const inodeMetaHdr = Buffer.alloc(2);
  inodeMetaHdr.writeUInt16LE(inodeBlockComp.length, 0);

  const dirTableStart = inodeTableStart + 2 + inodeBlockComp.length;
  const dirBlockComp = zlib.deflateSync(dirData);
  const dirMetaHdr = Buffer.alloc(2);
  dirMetaHdr.writeUInt16LE(dirBlockComp.length, 0);

  const totalSqLen = dirTableStart + 2 + dirBlockComp.length;
  const sb = Buffer.alloc(96, 0);
  Buffer.from('hsqs', 'ascii').copy(sb, 0);
  sb.writeUInt32LE(2, 4); // inodes
  sb.writeUInt32LE(131072, 12); // block_size
  sb.writeUInt32LE(0, 16); // fragments
  sb.writeUInt16LE(1, 20); // gzip
  sb.writeUInt16LE(17, 22); // block_log
  sb.writeUInt16LE(4, 28); // s_major
  sb.writeUInt16LE(0, 30); // s_minor
  sb.writeBigUInt64LE(BigInt(fileInode.length), 32); // root_inode_ref (block 0, off 36)
  sb.writeBigUInt64LE(BigInt(totalSqLen), 40); // bytes_used
  sb.writeBigUInt64LE(0xffffffffffffffffn, 56);
  sb.writeBigUInt64LE(BigInt(inodeTableStart), 64);
  sb.writeBigUInt64LE(BigInt(dirTableStart), 72);
  sb.writeBigUInt64LE(0xffffffffffffffffn, 80);

  return Buffer.concat([elfStub, sb, compFileBlock, inodeMetaHdr, inodeBlockComp, dirMetaHdr, dirBlockComp]);
}

// ── Tests ───────────────────────────────────────────────────────────────────────────────────

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
    version: '0.1.0+seq202611250137',
  });
});

test('validateCreateSequence refuses calendar sequences more than 24 hours ahead of UTC now', () => {
  const now = new Date('2026-11-25T02:00:00Z');
  assert.equal(validateCreateSequence(202611250200, now), 202611250200);
  assert.equal(validateCreateSequence(202611260200, now), 202611260200); // exactly +24h ok
  assert.throws(() => validateCreateSequence(202611260201, now), /more than 24 hours ahead/);
  assert.throws(() => validateCreateSequence(299912312359, now), /more than 24 hours ahead/);
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

    assert.throws(() => createPreviewKey({ privateKeyPath: priv, publicKeyPath: pub }), /already exists/);

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

test('verifyArtifactSequence inspects every artifact format without executing it and refuses mismatches', () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-update-seq-verify-'));
  try {
    const seq = 202611250137;
    const wrongSeq = 202611250136;

    // 1. Android APK (ZIP + binary AXML)
    const apk = path.join(tmp, 'Legion-0.2.0-universal.apk');
    fs.writeFileSync(apk, buildZip([{ name: 'AndroidManifest.xml', data: buildMinimalAxml(androidVersionCode(seq)) }]));
    verifyArtifactSequence(apk, { name: path.basename(apk), kind: 'apk' }, seq);
    assert.throws(() => verifyArtifactSequence(apk, { name: path.basename(apk), kind: 'apk' }, wrongSeq), /versionCode/);

    // 2. Node standalone binary (compiled sequence marker)
    const bin = path.join(tmp, 'legion-0.2.0-x86_64-unknown-linux-musl');
    fs.writeFileSync(bin, Buffer.concat([Buffer.from('\x7fELF...'), Buffer.from(`LEGION_UPDATE_SEQUENCE=${seq}\0`)]));
    verifyArtifactSequence(bin, { name: path.basename(bin), kind: 'binary' }, seq);
    assert.throws(() => verifyArtifactSequence(bin, { name: path.basename(bin), kind: 'binary' }, wrongSeq), /does not match sequence/);

    // 3. Debian .deb (ar + control.tar.gz)
    const deb = path.join(tmp, 'legion-app_0.2.0_amd64.deb');
    const controlTarGz = zlib.gzipSync(buildTar([{
      name: './control',
      data: Buffer.from(`Package: legion-app\nVersion: 0.2.0+${seq}\nArchitecture: amd64\n`),
    }]));
    fs.writeFileSync(deb, buildDeb([{ name: 'control.tar.gz', data: controlTarGz }]));
    verifyArtifactSequence(deb, { name: path.basename(deb), kind: 'deb' }, seq);
    assert.throws(() => verifyArtifactSequence(deb, { name: path.basename(deb), kind: 'deb' }, wrongSeq), /Debian Version/);

    // 4. macOS app-tar (.tar.gz + Info.plist)
    const appTar = path.join(tmp, 'Legion-macos-aarch64.app.tar.gz');
    const plist = `<?xml version="1.0"?><plist><dict><key>CFBundleVersion</key><string>${seq}</string></dict></plist>`;
    fs.writeFileSync(appTar, zlib.gzipSync(buildTar([{ name: 'Legion.app/Contents/Info.plist', data: Buffer.from(plist) }])));
    verifyArtifactSequence(appTar, { name: path.basename(appTar), kind: 'app-tar' }, seq);
    assert.throws(() => verifyArtifactSequence(appTar, { name: path.basename(appTar), kind: 'app-tar' }, wrongSeq), /CFBundleVersion/);

    // 5. Windows NSIS (.exe with UTF-16LE VS_VERSION_INFO FileVersion)
    const nsis = path.join(tmp, 'Legion-windows-x86_64-setup.exe');
    fs.writeFileSync(nsis, Buffer.concat([Buffer.from('MZ..'), Buffer.from(`0.2.0+seq${seq}\0`, 'utf16le')]));
    verifyArtifactSequence(nsis, { name: path.basename(nsis), kind: 'nsis' }, seq);
    assert.throws(() => verifyArtifactSequence(nsis, { name: path.basename(nsis), kind: 'nsis' }, wrongSeq), /does not match sequence/);

    // 6. Linux AppImage (SquashFS 4.0 containing .desktop with X-AppImage-Version)
    const appImage = path.join(tmp, 'Legion-0.2.0-x86_64.AppImage');
    fs.writeFileSync(appImage, buildMiniAppImage(`[Desktop Entry]\nName=Legion\nX-AppImage-Version=0.2.0+${seq}\n`));
    verifyArtifactSequence(appImage, { name: path.basename(appImage), kind: 'appimage' }, seq);
    assert.throws(() => verifyArtifactSequence(appImage, { name: path.basename(appImage), kind: 'appimage' }, wrongSeq), /does not match sequence/);
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});

test('makeUpdateSet verifies embedded sequences, records artifact sizes, signs, and detects tampering', () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-update-set-'));
  try {
    const priv = path.join(tmp, 'preview.key');
    const pub = path.join(tmp, 'preview.pub.pem');
    const otherPriv = path.join(tmp, 'other.key');
    const otherPub = path.join(tmp, 'other.pub.pem');
    createPreviewKey({ privateKeyPath: priv, publicKeyPath: pub });
    createPreviewKey({ privateKeyPath: otherPriv, publicKeyPath: otherPub });

    const seq = 202611250137;
    const now = new Date('2026-11-25T01:37:00Z');
    const apk = path.join(tmp, 'Legion-0.2.0-universal.apk');
    const appimage = path.join(tmp, 'Legion-0.2.0-x86_64.AppImage');
    fs.writeFileSync(apk, buildZip([{ name: 'AndroidManifest.xml', data: buildMinimalAxml(androidVersionCode(seq)) }]));
    fs.writeFileSync(appimage, buildMiniAppImage(`[Desktop Entry]\nName=Legion\nX-AppImage-Version=0.2.0+${seq}\n`));

    // Mismatched sequence is refused before writing an update set.
    assert.throws(
      () =>
        makeUpdateSet({
          outDir: path.join(tmp, 'set-bad-seq'),
          channel: 'preview',
          version: '0.2.0',
          build: 'abc123def456',
          sequence: 202611250138,
          notes: 'Wrong sequence.',
          privateKeyPath: priv,
          artifacts: [apk],
          now,
        }),
      /versionCode/,
    );

    const outDir = path.join(tmp, 'set');
    const created = makeUpdateSet({
      outDir,
      channel: 'preview',
      version: '0.2.0',
      build: 'abc123def456',
      sequence: seq,
      notes: 'Automatic fleet updates.',
      privateKeyPath: priv,
      publicKeyPath: pub,
      artifacts: [apk, appimage],
      now,
    });
    assert.equal(created.manifest.artifacts.length, 2);
    assert.equal(created.manifest.artifacts[0].size, fs.statSync(apk).size);
    assert.equal(created.manifest.artifacts[1].size, fs.statSync(appimage).size);
    assert.equal(created.entries.length, 3);

    const verified = verifyUpdateSet({ dir: outDir, publicKeyPath: pub });
    assert.equal(verified.manifest.sequence, seq);
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
