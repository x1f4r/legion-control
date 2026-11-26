// Suite 1: Download and upload through `legion stream` (direct & 1-hop `stream.relay`).
// Covers 1 MiB, 64 MiB, 1 GiB, and 2,000 x 16 KiB files (pack/unpack + keep_open get),
// reporting MiB/s, wall ms, CPU seconds on each side, and peak RSS (VmHWM / ru_maxrss).

import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { envCommandArgv, writeThrowawayNode } from '../lib/proc.mjs';
import { summarizeSamples } from '../lib/stats.mjs';

const SUITE_DIR = path.dirname(fileURLToPath(import.meta.url));
const STREAM_PY = path.join(SUITE_DIR, '..', 'lib', 'stream_bench.py');

function runStreamJob(spec) {
  const stdout = execFileSync('python3', [STREAM_PY], {
    input: JSON.stringify(spec),
    encoding: 'utf8',
    maxBuffer: 16 * 1024 * 1024,
  });
  return JSON.parse(stdout.trim());
}

function prepareStreamNodes(scratchRoot, bin, tag) {
  const root = path.join(scratchRoot, 'nodes-stream', tag);
  const targetHome = path.join(root, 'target-legion');
  const targetUser = path.join(root, 'target-user');
  const relayHome = path.join(root, 'relay-legion');
  const relayUser = path.join(root, 'relay-user');
  const uploadDir = path.join(root, 'uploads');
  fs.mkdirSync(uploadDir, { recursive: true });

  writeThrowawayNode({ legionHome: targetHome, userHome: targetUser, name: 'target' });

  const targetCmd = envCommandArgv(bin, targetHome, targetUser);
  const fleetToml = `[machines.target]\ncommand = ${JSON.stringify(targetCmd)}\n`;
  writeThrowawayNode({
    legionHome: relayHome,
    userHome: relayUser,
    name: 'relay',
    fleetToml,
  });

  return { targetHome, targetUser, relayHome, relayUser, uploadDir };
}

export async function runStreamSuite({ scratchRoot, fixtures, variants, runsOverride }) {
  const nodesByVariant = new Map();
  for (const v of variants) {
    nodesByVariant.set(v.id, prepareStreamNodes(scratchRoot, v.bin, v.id));
  }

  const cases = [
    {
      id: 'get-1m-direct',
      name: 'Download 1 MiB (direct)',
      runs: runsOverride ?? 5,
      run: (v, nodes) =>
        runStreamJob({
          mode: 'get',
          bin: v.bin,
          entry_legion_home: nodes.targetHome,
          entry_user_home: nodes.targetUser,
          remote_path: fixtures.blobs['1m'].path,
        }),
    },
    {
      id: 'get-1m-relay',
      name: 'Download 1 MiB (relayed)',
      runs: runsOverride ?? 5,
      run: (v, nodes) =>
        runStreamJob({
          mode: 'get',
          bin: v.bin,
          entry_legion_home: nodes.relayHome,
          entry_user_home: nodes.relayUser,
          relay_on: 'target',
          remote_path: fixtures.blobs['1m'].path,
        }),
    },
    {
      id: 'put-1m-direct',
      name: 'Upload 1 MiB (direct)',
      runs: runsOverride ?? 5,
      run: (v, nodes, iter) => {
        const dst = path.join(nodes.uploadDir, `put-1m-d-${iter}.bin`);
        const res = runStreamJob({
          mode: 'put',
          bin: v.bin,
          entry_legion_home: nodes.targetHome,
          entry_user_home: nodes.targetUser,
          source_file: fixtures.blobs['1m'].path,
          dest_path: dst,
          blake3_hash: fixtures.blobs['1m'].hash,
        });
        fs.rmSync(dst, { force: true });
        return res;
      },
    },
    {
      id: 'put-1m-relay',
      name: 'Upload 1 MiB (relayed)',
      runs: runsOverride ?? 5,
      run: (v, nodes, iter) => {
        const dst = path.join(nodes.uploadDir, `put-1m-r-${iter}.bin`);
        const res = runStreamJob({
          mode: 'put',
          bin: v.bin,
          entry_legion_home: nodes.relayHome,
          entry_user_home: nodes.relayUser,
          relay_on: 'target',
          source_file: fixtures.blobs['1m'].path,
          dest_path: dst,
          blake3_hash: fixtures.blobs['1m'].hash,
        });
        fs.rmSync(dst, { force: true });
        return res;
      },
    },
    {
      id: 'get-64m-direct',
      name: 'Download 64 MiB (direct)',
      runs: runsOverride ?? 5,
      run: (v, nodes) =>
        runStreamJob({
          mode: 'get',
          bin: v.bin,
          entry_legion_home: nodes.targetHome,
          entry_user_home: nodes.targetUser,
          remote_path: fixtures.blobs['64m'].path,
        }),
    },
    {
      id: 'get-64m-relay',
      name: 'Download 64 MiB (relayed)',
      runs: runsOverride ?? 5,
      run: (v, nodes) =>
        runStreamJob({
          mode: 'get',
          bin: v.bin,
          entry_legion_home: nodes.relayHome,
          entry_user_home: nodes.relayUser,
          relay_on: 'target',
          remote_path: fixtures.blobs['64m'].path,
        }),
    },
    {
      id: 'put-64m-direct',
      name: 'Upload 64 MiB (direct)',
      runs: runsOverride ?? 5,
      run: (v, nodes, iter) => {
        const dst = path.join(nodes.uploadDir, `put-64m-d-${iter}.bin`);
        const res = runStreamJob({
          mode: 'put',
          bin: v.bin,
          entry_legion_home: nodes.targetHome,
          entry_user_home: nodes.targetUser,
          source_file: fixtures.blobs['64m'].path,
          dest_path: dst,
          blake3_hash: fixtures.blobs['64m'].hash,
        });
        fs.rmSync(dst, { force: true });
        return res;
      },
    },
    {
      id: 'put-64m-relay',
      name: 'Upload 64 MiB (relayed)',
      runs: runsOverride ?? 5,
      run: (v, nodes, iter) => {
        const dst = path.join(nodes.uploadDir, `put-64m-r-${iter}.bin`);
        const res = runStreamJob({
          mode: 'put',
          bin: v.bin,
          entry_legion_home: nodes.relayHome,
          entry_user_home: nodes.relayUser,
          relay_on: 'target',
          source_file: fixtures.blobs['64m'].path,
          dest_path: dst,
          blake3_hash: fixtures.blobs['64m'].hash,
        });
        fs.rmSync(dst, { force: true });
        return res;
      },
    },
    {
      id: 'get-1g-direct',
      name: 'Download 1 GiB (direct)',
      highlight: true,
      runs: runsOverride ?? 3,
      run: (v, nodes) =>
        runStreamJob({
          mode: 'get',
          bin: v.bin,
          entry_legion_home: nodes.targetHome,
          entry_user_home: nodes.targetUser,
          remote_path: fixtures.blobs['1g'].path,
        }),
    },
    {
      id: 'get-1g-relay',
      name: 'Download 1 GiB (relayed)',
      highlight: true,
      runs: runsOverride ?? 3,
      run: (v, nodes) =>
        runStreamJob({
          mode: 'get',
          bin: v.bin,
          entry_legion_home: nodes.relayHome,
          entry_user_home: nodes.relayUser,
          relay_on: 'target',
          remote_path: fixtures.blobs['1g'].path,
        }),
    },
    {
      id: 'put-1g-direct',
      name: 'Upload 1 GiB (direct)',
      highlight: true,
      runs: runsOverride ?? 3,
      run: (v, nodes, iter) => {
        const dst = path.join(nodes.uploadDir, `put-1g-d-${iter}.bin`);
        const res = runStreamJob({
          mode: 'put',
          bin: v.bin,
          entry_legion_home: nodes.targetHome,
          entry_user_home: nodes.targetUser,
          source_file: fixtures.blobs['1g'].path,
          dest_path: dst,
          blake3_hash: fixtures.blobs['1g'].hash,
        });
        fs.rmSync(dst, { force: true });
        return res;
      },
    },
    {
      id: 'put-1g-relay',
      name: 'Upload 1 GiB (relayed)',
      runs: runsOverride ?? 3,
      run: (v, nodes, iter) => {
        const dst = path.join(nodes.uploadDir, `put-1g-r-${iter}.bin`);
        const res = runStreamJob({
          mode: 'put',
          bin: v.bin,
          entry_legion_home: nodes.relayHome,
          entry_user_home: nodes.relayUser,
          relay_on: 'target',
          source_file: fixtures.blobs['1g'].path,
          dest_path: dst,
          blake3_hash: fixtures.blobs['1g'].hash,
        });
        fs.rmSync(dst, { force: true });
        return res;
      },
    },
    {
      id: 'pack-2000x16k-direct',
      name: 'Download 2,000 × 16 KiB via pack (direct)',
      note: '31.25 MiB payload across 2,000 files in one tar stream',
      runs: runsOverride ?? 3,
      run: (v, nodes) =>
        runStreamJob({
          mode: 'pack',
          bin: v.bin,
          entry_legion_home: nodes.targetHome,
          entry_user_home: nodes.targetUser,
          remote_root: fixtures.small2000.dir,
          names: fixtures.small2000.names,
          payload_bytes: fixtures.small2000.payloadBytes,
        }),
    },
    {
      id: 'pack-2000x16k-relay',
      name: 'Download 2,000 × 16 KiB via pack (relayed)',
      runs: runsOverride ?? 3,
      run: (v, nodes) =>
        runStreamJob({
          mode: 'pack',
          bin: v.bin,
          entry_legion_home: nodes.relayHome,
          entry_user_home: nodes.relayUser,
          relay_on: 'target',
          remote_root: fixtures.small2000.dir,
          names: fixtures.small2000.names,
          payload_bytes: fixtures.small2000.payloadBytes,
        }),
    },
    {
      id: 'unpack-2000x16k-direct',
      name: 'Upload 2,000 × 16 KiB via unpack (direct)',
      highlight: true,
      note: '31.25 MiB payload across 2,000 files unpacked into a fresh directory',
      runs: runsOverride ?? 3,
      run: (v, nodes, iter) => {
        const dstRoot = path.join(nodes.uploadDir, `unpack-d-${iter}`);
        fs.rmSync(dstRoot, { recursive: true, force: true });
        const res = runStreamJob({
          mode: 'unpack',
          bin: v.bin,
          entry_legion_home: nodes.targetHome,
          entry_user_home: nodes.targetUser,
          tar_file: fixtures.small2000.tarFile,
          entries: fixtures.small2000.entries,
          dest_root: dstRoot,
          payload_bytes: fixtures.small2000.payloadBytes,
        });
        fs.rmSync(dstRoot, { recursive: true, force: true });
        return res;
      },
    },
    {
      id: 'unpack-2000x16k-relay',
      name: 'Upload 2,000 × 16 KiB via unpack (relayed)',
      runs: runsOverride ?? 3,
      run: (v, nodes, iter) => {
        const dstRoot = path.join(nodes.uploadDir, `unpack-r-${iter}`);
        fs.rmSync(dstRoot, { recursive: true, force: true });
        const res = runStreamJob({
          mode: 'unpack',
          bin: v.bin,
          entry_legion_home: nodes.relayHome,
          entry_user_home: nodes.relayUser,
          relay_on: 'target',
          tar_file: fixtures.small2000.tarFile,
          entries: fixtures.small2000.entries,
          dest_root: dstRoot,
          payload_bytes: fixtures.small2000.payloadBytes,
        });
        fs.rmSync(dstRoot, { recursive: true, force: true });
        return res;
      },
    },
    {
      id: 'get-2000x16k-keepopen-direct',
      name: 'Download 2,000 × 16 KiB via keep_open get (direct)',
      note: '2,000 sequential get requests over one warm stream',
      runs: runsOverride ?? 3,
      run: (v, nodes) =>
        runStreamJob({
          mode: 'get_keep_open_batch',
          bin: v.bin,
          entry_legion_home: nodes.targetHome,
          entry_user_home: nodes.targetUser,
          requests: fixtures.small2000.names.map((n) => ({
            path: path.join(fixtures.small2000.dir, n),
          })),
        }),
    },
    {
      id: 'get-2000x16k-keepopen-relay',
      name: 'Download 2,000 × 16 KiB via keep_open get (relayed)',
      runs: runsOverride ?? 3,
      run: (v, nodes) =>
        runStreamJob({
          mode: 'get_keep_open_batch',
          bin: v.bin,
          entry_legion_home: nodes.relayHome,
          entry_user_home: nodes.relayUser,
          relay_on: 'target',
          requests: fixtures.small2000.names.map((n) => ({
            path: path.join(fixtures.small2000.dir, n),
          })),
        }),
    },
  ];

  const rows = [];
  for (const c of cases) {
    console.error(`[bench:stream] ${c.name} (${c.runs} runs x ${variants.length} variant(s))...`);
    const samplesByVariant = new Map(variants.map((v) => [v.id, []]));
    // Interleave variants per iteration so host conditions match tightly.
    for (let iter = 0; iter < c.runs; iter += 1) {
      const order = iter % 2 === 0 ? variants : [...variants].reverse();
      for (const v of order) {
        const nodes = nodesByVariant.get(v.id);
        const sample = c.run(v, nodes, iter);
        samplesByVariant.get(v.id).push(sample);
      }
    }
    const row = { id: c.id, name: c.name, highlight: Boolean(c.highlight), note: c.note };
    for (const v of variants) {
      row[v.slot] = summarizeSamples(samplesByVariant.get(v.id));
    }
    rows.push(row);
  }

  return {
    id: 'stream',
    title: '1. Bulk Streams (`legion stream` direct & relayed)',
    description:
      'Loopback pipe throughput, wall time, CPU seconds on client and node/relay sides, and peak resident memory (ru_maxrss / VmHWM). Relayed runs use `op: "relay"` through a second `legion stream` node.',
    columns: [
      { key: 'mib_s', label: 'Throughput', unit: 'MiB/s', decimals: 1, primary: true, higherIsBetter: true },
      { key: 'wall_ms', label: 'Wall', unit: 'ms', decimals: 1 },
      { key: 'server_cpu_s', label: 'Node+Relay CPU', unit: 's', decimals: 3 },
      { key: 'client_cpu_s', label: 'Client CPU', unit: 's', decimals: 3 },
      { key: 'server_rss_mib', label: 'Node Peak RSS', unit: 'MiB', decimals: 1 },
      { key: 'client_rss_mib', label: 'Client Peak RSS', unit: 'MiB', decimals: 1 },
    ],
    rows,
    notes: [
      'For relayed runs, Node+Relay CPU is exact wait4 rusage across the relay process tree; target/relay splits are recorded in results.json when sampled.',
      '2,000 x 16 KiB upload via `unpack` exercises the exact tree-transfer protocol used by `fs.transfer`.',
    ],
  };
}
