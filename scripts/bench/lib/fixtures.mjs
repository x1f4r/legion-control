// Deterministic cached fixture builder under the benchmark scratch directory.
// Never uses /tmp for large fixtures and never touches the user's real Legion setup.

import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { writeThrowawayNode } from './proc.mjs';

const LIB_DIR = path.dirname(fileURLToPath(import.meta.url));
const STREAM_PY = path.join(LIB_DIR, 'stream_bench.py');

function runPython(code, args = []) {
  return execFileSync('python3', ['-c', code, ...args], {
    encoding: 'utf8',
    maxBuffer: 32 * 1024 * 1024,
  });
}

function statHashWithLegion(bin, legionHome, userHome, filePath) {
  const spec = {
    mode: 'stat_hash',
    bin,
    entry_legion_home: legionHome,
    entry_user_home: userHome,
    remote_path: filePath,
  };
  const out = execFileSync('python3', [STREAM_PY], {
    input: JSON.stringify(spec),
    encoding: 'utf8',
  });
  const parsed = JSON.parse(out.trim());
  if (!parsed.hash) {
    throw new Error(`legion stream stat did not return hash for ${filePath}: ${out}`);
  }
  return parsed.hash;
}

export function ensureFixtures(scratchRoot, bin, neededSuites) {
  const fixturesDir = path.join(scratchRoot, 'fixtures');
  const metaPath = path.join(fixturesDir, 'meta.json');
  fs.mkdirSync(fixturesDir, { recursive: true });

  let meta = {};
  if (fs.existsSync(metaPath)) {
    try {
      meta = JSON.parse(fs.readFileSync(metaPath, 'utf8'));
    } catch {
      meta = {};
    }
  }
  const saveMeta = () => fs.writeFileSync(metaPath, `${JSON.stringify(meta, null, 2)}\n`);

  const needStream = neededSuites.has('stream');
  const needHttp = neededSuites.has('http-content');
  const needFs = neededSuites.has('fs-tools');
  const needUi = neededSuites.has('ui-browser');

  // Helper node just for computing BLAKE3 hashes through the real legion binary.
  const hashNodeHome = path.join(scratchRoot, 'hash-node', 'legion');
  const hashUserHome = path.join(scratchRoot, 'hash-node', 'user');
  if (needStream || needHttp) {
    writeThrowawayNode({ legionHome: hashNodeHome, userHome: hashUserHome, name: 'hasher' });
  }

  const out = {
    dir: fixturesDir,
    blobs: {},
    small2000: null,
    list10k: null,
    list100k: null,
    tree100k: null,
    pics300: null,
    songs300: null,
    ui5k: null,
  };

  // 1. Binary blobs: 1 MiB, 64 MiB, 1 GiB (non-zero deterministic pattern).
  if (needStream || needHttp) {
    const sizes = needStream
      ? [
          ['1m', 1 * 1024 * 1024],
          ['64m', 64 * 1024 * 1024],
          ['1g', 1024 * 1024 * 1024],
        ]
      : [['1g', 1024 * 1024 * 1024]];

    meta.blobs = meta.blobs || {};
    for (const [key, bytes] of sizes) {
      const filePath = path.join(fixturesDir, `blob-${key}.bin`);
      if (!fs.existsSync(filePath) || fs.statSync(filePath).size !== bytes) {
        console.error(`[bench] creating ${(bytes / 1048576).toFixed(0)} MiB fixture ${filePath}...`);
        runPython(
          `
import os, sys, struct
path, total = sys.argv[1], int(sys.argv[2])
# Build a deterministic 1 MiB non-repeating base chunk via simple LCG bytes.
base = bytearray(1024 * 1024)
x = 0x9E3779B9
for i in range(0, len(base), 4):
    x = (x * 1664525 + 1013904223) & 0xFFFFFFFF
    struct.pack_into("<I", base, i, x)
with open(path, "wb") as f:
    rem = total
    idx = 0
    while rem > 0:
        n = min(len(base), rem)
        struct.pack_into("<I", base, 0, idx)
        f.write(memoryview(base)[:n])
        rem -= n
        idx += 1
`,
          [filePath, String(bytes)],
        );
        delete meta.blobs[key];
      }
      if (!meta.blobs[key]?.hash) {
        console.error(`[bench] hashing ${key} fixture via legion stream stat...`);
        const hash = statHashWithLegion(bin, hashNodeHome, hashUserHome, filePath);
        meta.blobs[key] = { path: filePath, bytes, hash };
        saveMeta();
      }
      out.blobs[key] = { path: filePath, bytes, hash: meta.blobs[key].hash };
    }
  }

  // 2. 2,000 files of 16 KiB + tar archive + Planned entries list.
  if (needStream) {
    const smallDir = path.join(fixturesDir, 'small-2000');
    const tarFile = path.join(fixturesDir, 'small-2000.tar');
    const count = 2000;
    const fileSize = 16 * 1024;
    const names = Array.from({ length: count }, (_, i) => `item-${String(i).padStart(4, '0')}.bin`);
    const marker = path.join(smallDir, names[count - 1]);
    if (!fs.existsSync(marker) || fs.statSync(marker).size !== fileSize) {
      console.error(`[bench] creating 2,000 x 16 KiB files in ${smallDir}...`);
      fs.mkdirSync(smallDir, { recursive: true });
      runPython(
        `
import os, sys, struct
d, n, sz = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
buf = bytearray(sz)
for i in range(sz):
    buf[i] = (i * 31 + 17) & 0xFF
for k in range(n):
    struct.pack_into("<I", buf, 0, k)
    with open(os.path.join(d, f"item-{k:04d}.bin"), "wb") as f:
        f.write(buf)
`,
        [smallDir, String(count), String(fileSize)],
      );
      if (fs.existsSync(tarFile)) fs.unlinkSync(tarFile);
    }
    if (!fs.existsSync(tarFile)) {
      console.error(`[bench] building tar archive ${tarFile}...`);
      execFileSync('python3', [STREAM_PY], {
        input: JSON.stringify({
          mode: 'build_tar',
          source_dir: smallDir,
          tar_file: tarFile,
          names,
        }),
        encoding: 'utf8',
      });
    }
    const entries = names.map((name) => ({ path: name, kind: 'file', size: fileSize }));
    out.small2000 = {
      dir: smallDir,
      tarFile,
      names,
      entries,
      fileCount: count,
      fileSize,
      payloadBytes: count * fileSize,
    };
  }

  // 3. Filesystem fixtures: 10,000-entry dir, 100,000-entry dir, 100,000-file tree, 300 pics, 300 songs.
  if (needFs) {
    const list10k = path.join(fixturesDir, 'list-10k');
    if (!fs.existsSync(path.join(list10k, 'entry_09999.md'))) {
      console.error(`[bench] creating 10,000-entry directory ${list10k}...`);
      fs.mkdirSync(list10k, { recursive: true });
      runPython(
        `
import os, sys
d = sys.argv[1]
exts = [".txt", ".jpg", ".pdf", ".mp4", ".mp3", ".rs", ".json", ".zip", ".png", ".md"]
for i in range(10000):
    ext = exts[i % len(exts)]
    p = os.path.join(d, f"entry_{i:05d}{ext}")
    fd = os.open(p, os.O_CREAT | os.O_WRONLY, 0o644)
    os.close(fd)
`,
        [list10k],
      );
    }
    out.list10k = list10k;

    const list100k = path.join(fixturesDir, 'list-100k');
    if (!fs.existsSync(path.join(list100k, 'entry_099999.md'))) {
      console.error(`[bench] creating 100,000-entry directory ${list100k}...`);
      fs.mkdirSync(list100k, { recursive: true });
      runPython(
        `
import os, sys
d = sys.argv[1]
exts = [".txt", ".jpg", ".pdf", ".mp4", ".mp3", ".rs", ".json", ".zip", ".png", ".md"]
for i in range(100000):
    ext = exts[i % len(exts)]
    p = os.path.join(d, f"entry_{i:06d}{ext}")
    fd = os.open(p, os.O_CREAT | os.O_WRONLY, 0o644)
    os.close(fd)
`,
        [list100k],
      );
    }
    out.list100k = list100k;

    const tree100k = path.join(fixturesDir, 'tree-100k');
    if (!fs.existsSync(path.join(tree100k, 'dir_099', 'needle_report_099.txt'))) {
      console.error(`[bench] creating 100,000-file directory tree ${tree100k}...`);
      fs.mkdirSync(tree100k, { recursive: true });
      runPython(
        `
import os, sys
root = sys.argv[1]
exts = [".txt", ".jpg", ".pdf", ".rs", ".json", ".log", ".png", ".csv", ".md", ".bin"]
for d_idx in range(100):
    sub = os.path.join(root, f"dir_{d_idx:03d}")
    os.makedirs(sub, exist_ok=True)
    for f_idx in range(1000):
        if f_idx == 999 and (d_idx % 10 == 9):
            name = f"needle_report_{d_idx:03d}.txt"
        else:
            name = f"file_{d_idx:03d}_{f_idx:04d}{exts[f_idx % len(exts)]}"
        fd = os.open(os.path.join(sub, name), os.O_CREAT | os.O_WRONLY, 0o644)
        os.close(fd)
`,
        [tree100k],
      );
    }
    out.tree100k = tree100k;

    // 300 distinct real 1280x720 JPEG pictures generated once with ffmpeg.
    const pics300 = path.join(fixturesDir, 'pics-300');
    const lastPic = path.join(pics300, 'photo_0300.jpg');
    if (!fs.existsSync(lastPic)) {
      console.error(`[bench] generating 300 real 1280x720 JPEGs in ${pics300}...`);
      fs.mkdirSync(pics300, { recursive: true });
      execFileSync(
        'ffmpeg',
        [
          '-hide_banner',
          '-loglevel',
          'error',
          '-y',
          '-f',
          'lavfi',
          '-i',
          'testsrc2=size=1280x720:rate=30:duration=10',
          '-frames:v',
          '300',
          '-q:v',
          '4',
          path.join(pics300, 'photo_%04d.jpg'),
        ],
        { stdio: 'inherit' },
      );
    }
    out.pics300 = {
      dir: pics300,
      files: Array.from({ length: 300 }, (_, i) =>
        path.join(pics300, `photo_${String(i + 1).padStart(4, '0')}.jpg`),
      ),
    };

    // 300 distinct real ID3v2-tagged MP3 songs in one album folder.
    const songs300 = path.join(fixturesDir, 'songs-300');
    const lastSong = path.join(songs300, 'track_0300.mp3');
    if (!fs.existsSync(lastSong)) {
      console.error(`[bench] generating 300 real tagged MP3 files in ${songs300}...`);
      fs.mkdirSync(songs300, { recursive: true });
      const baseMp3 = path.join(fixturesDir, '_base_tone.mp3');
      execFileSync(
        'ffmpeg',
        [
          '-hide_banner',
          '-loglevel',
          'error',
          '-y',
          '-f',
          'lavfi',
          '-i',
          'sine=frequency=440:duration=2',
          '-c:a',
          'libmp3lame',
          '-b:a',
          '128k',
          baseMp3,
        ],
        { stdio: 'inherit' },
      );
      runPython(
        `
import concurrent.futures, os, subprocess, sys
base_mp3, out_dir = sys.argv[1], sys.argv[2]

def make_one(i):
    dst = os.path.join(out_dir, f"track_{i:04d}.mp3")
    subprocess.run([
        "ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
        "-i", base_mp3, "-c:a", "copy",
        "-metadata", f"title=Benchmark Track {i:03d}",
        "-metadata", f"artist=Signal Quartet {(i % 12) + 1}",
        "-metadata", f"album=Session Anthology {(i % 6) + 1}",
        "-metadata", f"track={i}/300",
        "-metadata", "date=2025",
        dst,
    ], check=True)

with concurrent.futures.ThreadPoolExecutor(max_workers=min(16, os.cpu_count() or 4)) as ex:
    list(ex.map(make_one, range(1, 301)))
`,
        [baseMp3, songs300],
      );
      fs.unlinkSync(baseMp3);
    }
    out.songs300 = {
      dir: songs300,
      files: Array.from({ length: 300 }, (_, i) =>
        path.join(songs300, `track_${String(i + 1).padStart(4, '0')}.mp3`),
      ),
    };
  }

  // 4. UI 5,000-entry directory (text/docs/code files so list rendering & scroll are measured cleanly).
  if (needUi) {
    const ui5k = path.join(fixturesDir, 'ui-5k');
    if (!fs.existsSync(path.join(ui5k, 'file_04999.toml'))) {
      console.error(`[bench] creating 5,000-entry UI fixture directory ${ui5k}...`);
      fs.mkdirSync(ui5k, { recursive: true });
      runPython(
        `
import os, sys
d = sys.argv[1]
exts = [".txt", ".md", ".json", ".rs", ".js", ".pdf", ".zip", ".log", ".csv", ".toml"]
for i in range(5000):
    p = os.path.join(d, f"file_{i:05d}{exts[i % len(exts)]}")
    with open(p, "wb") as f:
        f.write(b"sample content\\n")
`,
        [ui5k],
      );
    }
    out.ui5k = ui5k;
  }

  return out;
}
