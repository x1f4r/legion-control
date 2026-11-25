import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import zlib from 'node:zlib';

import {
  androidVersionCode,
  checkShippedArtifact,
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

/**
 * Build a tiny valid SquashFS 4.0 image (gzip) behind an ELF64 prefix containing:
 * - optional root `legion-app.desktop`
 * - optional `usr/bin/legion-app` compiled binary bytes
 */
function buildMiniAppImage({ desktopText = null, binBytes = null, elfSize = 128 } = {}) {
  const elfStub = Buffer.alloc(Math.max(128, elfSize), 0);
  Buffer.from('\x7fELF', 'ascii').copy(elfStub, 0);
  elfStub[4] = 2; // ELF64
  elfStub[5] = 1; // little-endian
  const shentsize = 64;
  const shnum = 1;
  const shoff = elfStub.length - shentsize * shnum;
  elfStub.writeBigUInt64LE(BigInt(shoff), 40);
  elfStub.writeUInt16LE(shentsize, 58);
  elfStub.writeUInt16LE(shnum, 60);

  const dataBlocks = [];
  let dataCursor = 96; // immediately after 96-byte superblock
  const addFileData = (raw) => {
    const start = dataCursor;
    const comp = zlib.deflateSync(raw);
    dataBlocks.push(comp);
    dataCursor += comp.length;
    return { start, rawLen: raw.length, compLen: comp.length };
  };

  const makeFileInode = (info) => {
    const buf = Buffer.alloc(36);
    buf.writeUInt16LE(2, 0); // basic regular file
    buf.writeUInt32LE(info.start, 16); // blocks_start
    buf.writeUInt32LE(0xffffffff, 20); // no fragment
    buf.writeUInt32LE(0, 24);
    buf.writeUInt32LE(info.rawLen, 28);
    buf.writeUInt32LE(info.compLen, 32);
    return buf;
  };

  const dirStreamParts = [];
  let dirStreamLen = 0;
  const addDirTable = (entries) => {
    const startOff = dirStreamLen;
    const parts = [];
    const hdr = Buffer.alloc(12);
    hdr.writeUInt32LE(entries.length - 1, 0); // count - 1
    hdr.writeUInt32LE(0, 4); // start_block = 0
    hdr.writeUInt32LE(1, 8); // inode_number
    parts.push(hdr);
    for (const e of entries) {
      const nameBuf = Buffer.from(e.name, 'utf8');
      const ent = Buffer.alloc(8 + nameBuf.length);
      ent.writeUInt16LE(e.inodeOff, 0);
      ent.writeInt16LE(0, 2);
      ent.writeUInt16LE(e.type, 4);
      ent.writeUInt16LE(nameBuf.length - 1, 6);
      nameBuf.copy(ent, 8);
      parts.push(ent);
    }
    const raw = Buffer.concat(parts);
    dirStreamParts.push(raw);
    dirStreamLen += raw.length;
    return { offset: startOff, size: raw.length };
  };

  const makeDirInode = (dirMeta) => {
    const buf = Buffer.alloc(32);
    buf.writeUInt16LE(1, 0); // basic dir
    buf.writeUInt32LE(0, 16); // block_index = 0
    buf.writeUInt32LE(2, 20); // link_count
    buf.writeUInt16LE(dirMeta.size + 3, 24); // file_size (+3 for . and ..)
    buf.writeUInt16LE(dirMeta.offset, 26); // block_offset
    return buf;
  };

  const inodes = [];
  let inodeOff = 0;
  const pushInode = (buf) => {
    const off = inodeOff;
    inodes.push(buf);
    inodeOff += buf.length;
    return off;
  };

  const rootEntries = [];
  if (desktopText !== null) {
    const deskOff = pushInode(makeFileInode(addFileData(Buffer.from(desktopText, 'utf8'))));
    rootEntries.push({ name: 'legion-app.desktop', inodeOff: deskOff, type: 2 });
  }
  if (binBytes !== null) {
    const binOff = pushInode(makeFileInode(addFileData(Buffer.isBuffer(binBytes) ? binBytes : Buffer.from(binBytes))));
    const binDirMeta = addDirTable([{ name: 'legion-app', inodeOff: binOff, type: 2 }]);
    const binDirOff = pushInode(makeDirInode(binDirMeta));
    const usrDirMeta = addDirTable([{ name: 'bin', inodeOff: binDirOff, type: 1 }]);
    const usrDirOff = pushInode(makeDirInode(usrDirMeta));
    rootEntries.push({ name: 'usr', inodeOff: usrDirOff, type: 1 });
  }
  const rootDirMeta = addDirTable(rootEntries);
  const rootInodeOff = pushInode(makeDirInode(rootDirMeta));

  const inodeTableStart = dataCursor;
  const inodeBlockComp = zlib.deflateSync(Buffer.concat(inodes));
  const inodeMetaHdr = Buffer.alloc(2);
  inodeMetaHdr.writeUInt16LE(inodeBlockComp.length, 0);

  const dirTableStart = inodeTableStart + 2 + inodeBlockComp.length;
  const dirBlockComp = zlib.deflateSync(Buffer.concat(dirStreamParts));
  const dirMetaHdr = Buffer.alloc(2);
  dirMetaHdr.writeUInt16LE(dirBlockComp.length, 0);

  const totalSqLen = dirTableStart + 2 + dirBlockComp.length;
  const sb = Buffer.alloc(96, 0);
  Buffer.from('hsqs', 'ascii').copy(sb, 0);
  sb.writeUInt32LE(inodes.length, 4); // inodes
  sb.writeUInt32LE(131072, 12); // block_size
  sb.writeUInt32LE(0, 16); // fragments
  sb.writeUInt16LE(1, 20); // gzip
  sb.writeUInt16LE(17, 22); // block_log
  sb.writeUInt16LE(4, 28); // s_major
  sb.writeUInt16LE(0, 30); // s_minor
  sb.writeBigUInt64LE(BigInt(rootInodeOff), 32); // root_inode_ref
  sb.writeBigUInt64LE(BigInt(totalSqLen), 40); // bytes_used
  sb.writeBigUInt64LE(0xffffffffffffffffn, 56);
  sb.writeBigUInt64LE(BigInt(inodeTableStart), 64);
  sb.writeBigUInt64LE(BigInt(dirTableStart), 72);
  sb.writeBigUInt64LE(0xffffffffffffffffn, 80);

  return Buffer.concat([elfStub, sb, ...dataBlocks, inodeMetaHdr, inodeBlockComp, dirMetaHdr, dirBlockComp]);
}

/** Build a synthetic Windows NSIS installer with PE UTF-16LE version string and zlib NSIS datablock. */
function buildMiniNsis({ peSeq, payloadBytes }) {
  const peHeader = Buffer.concat([
    Buffer.from('MZ..'),
    peSeq !== null ? Buffer.from(`0.2.0+seq${peSeq}\0`, 'utf16le') : Buffer.alloc(0),
  ]);
  // Align NSIS FirstHeader to 512 bytes.
  const padLen = (512 - (peHeader.length % 512)) % 512;
  const stub = Buffer.concat([peHeader, Buffer.alloc(padLen, 0)]);
  const deflated = zlib.deflateRawSync(payloadBytes);
  const blockHdr = Buffer.alloc(4);
  blockHdr.writeUInt32LE((0x80000000 | deflated.length) >>> 0, 0);
  const archiveLen = 28 + blockHdr.length + deflated.length;
  const firstHeader = Buffer.alloc(28, 0);
  Buffer.from([
    0xef, 0xbe, 0xad, 0xde, 0x4e, 0x75, 0x6c, 0x6c, 0x73, 0x6f, 0x66, 0x74, 0x49, 0x6e, 0x73, 0x74,
  ]).copy(firstHeader, 4);
  firstHeader.writeUInt32LE(payloadBytes.length, 20);
  firstHeader.writeUInt32LE(archiveLen, 24);
  return Buffer.concat([stub, firstHeader, blockHdr, deflated]);
}

function compiledPayload(seq, previewValue = 'none') {
  return Buffer.concat([
    Buffer.from('\x7fELF...'),
    Buffer.from(`LEGION_UPDATE_SEQUENCE=${seq}\0`),
    Buffer.from(`LEGION_UPDATE_PREVIEW_KEY=${previewValue}\0`),
  ]);
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

test('verifyArtifactSequence inspects compiled binaries in every format and refuses mismatches or conflicting markers', () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-update-seq-verify-'));
  try {
    const seq = 202611250137;
    const wrongSeq = 202611250136;

    // 1. Android APK (ZIP + binary AXML + compiled lib/*/liblegion_app_lib.so)
    const apk = path.join(tmp, 'Legion-0.2.0-universal.apk');
    fs.writeFileSync(
      apk,
      buildZip([
        { name: 'AndroidManifest.xml', data: buildMinimalAxml(androidVersionCode(seq)) },
        { name: 'lib/arm64-v8a/liblegion_app_lib.so', data: compiledPayload(seq) },
        { name: 'lib/x86_64/liblegion_app_lib.so', data: compiledPayload(seq) },
      ]),
    );
    verifyArtifactSequence(apk, { name: path.basename(apk), kind: 'apk' }, seq);
    assert.throws(() => verifyArtifactSequence(apk, { name: path.basename(apk), kind: 'apk' }, wrongSeq), /versionCode/);

    // APK with matching versionCode but seq-0 compiled native library MUST be refused!
    const apkSeq0 = path.join(tmp, 'Legion-seq0.apk');
    fs.writeFileSync(
      apkSeq0,
      buildZip([
        { name: 'AndroidManifest.xml', data: buildMinimalAxml(androidVersionCode(seq)) },
        { name: 'lib/arm64-v8a/liblegion_app_lib.so', data: compiledPayload(0) },
      ]),
    );
    assert.throws(
      () => verifyArtifactSequence(apkSeq0, { name: path.basename(apkSeq0), kind: 'apk' }, seq),
      /compiled LEGION_UPDATE_SEQUENCE=0/,
    );

    // APK with one matching ABI and one mismatched ABI MUST be refused!
    const apkMixedAbi = path.join(tmp, 'Legion-mixed.apk');
    fs.writeFileSync(
      apkMixedAbi,
      buildZip([
        { name: 'AndroidManifest.xml', data: buildMinimalAxml(androidVersionCode(seq)) },
        { name: 'lib/arm64-v8a/liblegion_app_lib.so', data: compiledPayload(seq) },
        { name: 'lib/x86_64/liblegion_app_lib.so', data: compiledPayload(wrongSeq) },
      ]),
    );
    assert.throws(
      () => verifyArtifactSequence(apkMixedAbi, { name: path.basename(apkMixedAbi), kind: 'apk' }, seq),
      /compiled LEGION_UPDATE_SEQUENCE=/,
    );

    // 2. Node standalone binary (compiled sequence marker)
    const bin = path.join(tmp, 'legion-0.2.0-x86_64-unknown-linux-musl');
    fs.writeFileSync(bin, compiledPayload(seq));
    verifyArtifactSequence(bin, { name: path.basename(bin), kind: 'binary' }, seq);
    assert.throws(() => verifyArtifactSequence(bin, { name: path.basename(bin), kind: 'binary' }, wrongSeq), /compiled LEGION_UPDATE_SEQUENCE=/);

    // Binary holding TWO conflicting markers (one matching, one different) MUST be refused!
    const dualBin = path.join(tmp, 'legion-dual');
    fs.writeFileSync(
      dualBin,
      Buffer.concat([
        Buffer.from(`LEGION_UPDATE_SEQUENCE=${seq}\0`),
        Buffer.from(`LEGION_UPDATE_SEQUENCE=${wrongSeq}\0`),
      ]),
    );
    assert.throws(
      () => verifyArtifactSequence(dualBin, { name: path.basename(dualBin), kind: 'binary' }, seq),
      /conflicting compiled LEGION_UPDATE_SEQUENCE markers/,
    );

    // Duplicate identical markers fail `checkShippedArtifact(..., { requireExactlyOne: true })`.
    const dupBin = path.join(tmp, 'legion-dup');
    fs.writeFileSync(
      dupBin,
      Buffer.concat([
        Buffer.from(`LEGION_UPDATE_SEQUENCE=${seq}\0`),
        Buffer.from(`LEGION_UPDATE_SEQUENCE=${seq}\0`),
        Buffer.from('LEGION_UPDATE_PREVIEW_KEY=none\0'),
      ]),
    );
    assert.throws(
      () => checkShippedArtifact({ filePath: dupBin, kind: 'binary', sequence: seq, requireExactlyOne: true }),
      /expected exactly 1/,
    );

    // 3. Debian .deb (ar + control.tar.gz + data.tar.gz with compiled usr/bin/legion-app)
    const deb = path.join(tmp, 'legion-app_0.2.0_amd64.deb');
    const controlTarGz = zlib.gzipSync(buildTar([{
      name: './control',
      data: Buffer.from(`Package: legion-app\nVersion: 0.2.0+${seq}\nArchitecture: amd64\n`),
    }]));
    const dataTarGz = zlib.gzipSync(buildTar([{
      name: './usr/bin/legion-app',
      data: compiledPayload(seq),
    }]));
    fs.writeFileSync(deb, buildDeb([
      { name: 'control.tar.gz', data: controlTarGz },
      { name: 'data.tar.gz', data: dataTarGz },
    ]));
    verifyArtifactSequence(deb, { name: path.basename(deb), kind: 'deb' }, seq);
    assert.throws(() => verifyArtifactSequence(deb, { name: path.basename(deb), kind: 'deb' }, wrongSeq), /Debian Version/);

    // .deb with matching control Version but seq-0 compiled binary MUST be refused!
    const debBadBin = path.join(tmp, 'legion-app-badbin.deb');
    const badDataTarGz = zlib.gzipSync(buildTar([{
      name: './usr/bin/legion-app',
      data: compiledPayload(0),
    }]));
    fs.writeFileSync(debBadBin, buildDeb([
      { name: 'control.tar.gz', data: controlTarGz },
      { name: 'data.tar.gz', data: badDataTarGz },
    ]));
    assert.throws(
      () => verifyArtifactSequence(debBadBin, { name: path.basename(debBadBin), kind: 'deb' }, seq),
      /compiled LEGION_UPDATE_SEQUENCE=0/,
    );

    // 4. macOS app-tar (.tar.gz + Info.plist + Contents/MacOS/legion-app)
    const appTar = path.join(tmp, 'Legion-macos-aarch64.app.tar.gz');
    const plist = `<?xml version="1.0"?><plist><dict><key>CFBundleExecutable</key><string>legion-app</string><key>CFBundleVersion</key><string>${seq}</string></dict></plist>`;
    fs.writeFileSync(
      appTar,
      zlib.gzipSync(buildTar([
        { name: 'Legion.app/Contents/Info.plist', data: Buffer.from(plist) },
        { name: 'Legion.app/Contents/MacOS/legion-app', data: compiledPayload(seq) },
      ])),
    );
    verifyArtifactSequence(appTar, { name: path.basename(appTar), kind: 'app-tar' }, seq);
    assert.throws(() => verifyArtifactSequence(appTar, { name: path.basename(appTar), kind: 'app-tar' }, wrongSeq), /CFBundleVersion/);

    // macOS app-tar with matching Info.plist but seq-0 binary MUST be refused!
    const appTarBadBin = path.join(tmp, 'Legion-badbin.app.tar.gz');
    fs.writeFileSync(
      appTarBadBin,
      zlib.gzipSync(buildTar([
        { name: 'Legion.app/Contents/Info.plist', data: Buffer.from(plist) },
        { name: 'Legion.app/Contents/MacOS/legion-app', data: compiledPayload(0) },
      ])),
    );
    assert.throws(
      () => verifyArtifactSequence(appTarBadBin, { name: path.basename(appTarBadBin), kind: 'app-tar' }, seq),
      /compiled LEGION_UPDATE_SEQUENCE=0/,
    );

    // 5. Windows NSIS (.exe with PE UTF-16LE +seq<N> AND compressed NSIS payload containing compiled marker)
    const nsis = path.join(tmp, 'Legion-windows-x86_64-setup.exe');
    fs.writeFileSync(nsis, buildMiniNsis({ peSeq: seq, payloadBytes: compiledPayload(seq) }));
    verifyArtifactSequence(nsis, { name: path.basename(nsis), kind: 'nsis' }, seq);
    assert.throws(() => verifyArtifactSequence(nsis, { name: path.basename(nsis), kind: 'nsis' }, wrongSeq), /PE version string/);

    // NSIS with matching PE +seq<N> but seq-0 compiled payload MUST be refused!
    const nsisBadPayload = path.join(tmp, 'Legion-badpayload-setup.exe');
    fs.writeFileSync(nsisBadPayload, buildMiniNsis({ peSeq: seq, payloadBytes: compiledPayload(0) }));
    assert.throws(
      () => verifyArtifactSequence(nsisBadPayload, { name: path.basename(nsisBadPayload), kind: 'nsis' }, seq),
      /compiled LEGION_UPDATE_SEQUENCE=0/,
    );

    // NSIS with PE metadata only and no NSIS payload MUST be refused!
    const nsisNoPayload = path.join(tmp, 'Legion-nopayload-setup.exe');
    fs.writeFileSync(nsisNoPayload, Buffer.concat([Buffer.from('MZ..'), Buffer.from(`0.2.0+seq${seq}\0`, 'utf16le')]));
    assert.throws(
      () => verifyArtifactSequence(nsisNoPayload, { name: path.basename(nsisNoPayload), kind: 'nsis' }, seq),
      /NSIS payload/,
    );

    // 6. Linux AppImage (SquashFS 4.0 containing BOTH .desktop AND compiled usr/bin/legion-app behind >900 KiB ELF64)
    const appImage = path.join(tmp, 'Legion-0.2.0-x86_64.AppImage');
    fs.writeFileSync(
      appImage,
      buildMiniAppImage({
        desktopText: `[Desktop Entry]\nName=Legion\nX-AppImage-Version=0.2.0+${seq}\n`,
        binBytes: compiledPayload(seq),
        elfSize: 1024 * 1024,
      }),
    );
    verifyArtifactSequence(appImage, { name: path.basename(appImage), kind: 'appimage' }, seq);
    assert.throws(() => verifyArtifactSequence(appImage, { name: path.basename(appImage), kind: 'appimage' }, wrongSeq), /X-AppImage-Version/);

    // AppImage with matching .desktop metadata ALONE (no usr/bin/legion-app) MUST be refused!
    const appImageDesktopOnly = path.join(tmp, 'Legion-desktop-only.AppImage');
    fs.writeFileSync(
      appImageDesktopOnly,
      buildMiniAppImage({
        desktopText: `[Desktop Entry]\nName=Legion\nX-AppImage-Version=0.2.0+${seq}\n`,
        binBytes: null,
      }),
    );
    assert.throws(
      () => verifyArtifactSequence(appImageDesktopOnly, { name: path.basename(appImageDesktopOnly), kind: 'appimage' }, seq),
      /desktop metadata alone is not accepted/,
    );

    // AppImage with matching .desktop but seq-0 compiled usr/bin/legion-app MUST be refused!
    const appImageBadBin = path.join(tmp, 'Legion-badbin.AppImage');
    fs.writeFileSync(
      appImageBadBin,
      buildMiniAppImage({
        desktopText: `[Desktop Entry]\nName=Legion\nX-AppImage-Version=0.2.0+${seq}\n`,
        binBytes: compiledPayload(0),
      }),
    );
    assert.throws(
      () => verifyArtifactSequence(appImageBadBin, { name: path.basename(appImageBadBin), kind: 'appimage' }, seq),
      /compiled LEGION_UPDATE_SEQUENCE=0/,
    );
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});

test('checkShippedArtifact enforces no compiled-in preview key across formats and build logs', () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-update-preview-check-'));
  try {
    const seq = 202611250137;
    const cleanLog = path.join(tmp, 'clean.log');
    const previewLog = path.join(tmp, 'preview.log');
    fs.writeFileSync(cleanLog, 'warning: legion-update@0.1.0: legion-update: release update key only (no preview key)\n');
    fs.writeFileSync(previewLog, 'warning: legion-update@0.1.0: legion-update: preview update key compiled in (SHA256:abc123)\n');

    // Clean NSIS passes `--forbid-preview-key`.
    const cleanNsis = path.join(tmp, 'Legion-clean-setup.exe');
    fs.writeFileSync(cleanNsis, buildMiniNsis({ peSeq: seq, payloadBytes: compiledPayload(seq, 'none') }));
    checkShippedArtifact({
      filePath: cleanNsis,
      kind: 'nsis',
      sequence: seq,
      requireExactlyOne: true,
      forbidPreviewKey: true,
      buildLogPath: cleanLog,
    });

    // Stale preview-keyed NSIS fails `--forbid-preview-key` even with a clean log!
    const previewNsis = path.join(tmp, 'Legion-preview-setup.exe');
    fs.writeFileSync(previewNsis, buildMiniNsis({ peSeq: seq, payloadBytes: compiledPayload(seq, 'SHA256:throwawayPreviewFp') }));
    assert.throws(
      () => checkShippedArtifact({
        filePath: previewNsis,
        kind: 'nsis',
        sequence: seq,
        requireExactlyOne: true,
        forbidPreviewKey: true,
        buildLogPath: cleanLog,
      }),
      /trusts a compiled-in preview update key/,
    );

    // Build log reporting preview key compiled in fails even if binary was replaced!
    assert.throws(
      () => checkShippedArtifact({
        filePath: cleanNsis,
        kind: 'nsis',
        sequence: seq,
        requireExactlyOne: true,
        forbidPreviewKey: true,
        buildLogPath: previewLog,
      }),
      /build log .* preview update key/,
    );

    // Clean macOS app-bundle directory passes; preview-keyed app-bundle fails.
    const appDir = path.join(tmp, 'Legion.app');
    fs.mkdirSync(path.join(appDir, 'Contents', 'MacOS'), { recursive: true });
    fs.writeFileSync(
      path.join(appDir, 'Contents', 'Info.plist'),
      `<plist><dict><key>CFBundleExecutable</key><string>legion-app</string><key>CFBundleVersion</key><string>${seq}</string></dict></plist>`,
    );
    fs.writeFileSync(path.join(appDir, 'Contents', 'MacOS', 'legion-app'), compiledPayload(seq, 'none'));
    checkShippedArtifact({
      filePath: appDir,
      kind: 'app-bundle',
      sequence: seq,
      requireExactlyOne: true,
      forbidPreviewKey: true,
    });
    fs.writeFileSync(
      path.join(appDir, 'Contents', 'MacOS', 'legion-app'),
      compiledPayload(seq, 'SHA256:badPreviewKey'),
    );
    assert.throws(
      () => checkShippedArtifact({
        filePath: appDir,
        kind: 'app-bundle',
        sequence: seq,
        requireExactlyOne: true,
        forbidPreviewKey: true,
      }),
      /trusts a compiled-in preview update key/,
    );
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
    fs.writeFileSync(
      apk,
      buildZip([
        { name: 'AndroidManifest.xml', data: buildMinimalAxml(androidVersionCode(seq)) },
        { name: 'lib/arm64-v8a/liblegion_app_lib.so', data: compiledPayload(seq) },
      ]),
    );
    fs.writeFileSync(
      appimage,
      buildMiniAppImage({
        desktopText: `[Desktop Entry]\nName=Legion\nX-AppImage-Version=0.2.0+${seq}\n`,
        binBytes: compiledPayload(seq),
      }),
    );

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
