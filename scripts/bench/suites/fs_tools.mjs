// Suite 3: Filesystem and media tools over a persistent `legion rpc` node process.
// Measures latency and node peak RSS (VmHWM) for:
//   - fs.list on 10,000 and 100,000 entries
//   - fs.find over a 100,000-file directory tree
//   - fs.thumb for 300 real 1280x720 JPEGs in batches of 32 (cold cache)
//   - fs.tags for 300 real ID3v2-tagged MP3s in batches of 32 (cold cache)

import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import readline from 'node:readline';
import { cleanLegionEnv, readProcMetrics, writeThrowawayNode } from '../lib/proc.mjs';
import { summarizeSamples } from '../lib/stats.mjs';

class RpcSession {
  constructor(bin, legionHome, userHome) {
    this.child = spawn(bin, ['rpc'], {
      env: cleanLegionEnv(legionHome, userHome),
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    this.nextId = 1;
    this.pending = new Map();
    this.stderr = '';
    this.child.stderr.on('data', (d) => {
      this.stderr += d.toString('utf8');
    });
    this.rl = readline.createInterface({ input: this.child.stdout });
    this.rl.on('line', (line) => {
      if (!line.trim()) return;
      let msg;
      try {
        msg = JSON.parse(line);
      } catch {
        return;
      }
      if (msg.id !== undefined && this.pending.has(msg.id)) {
        const { resolve, reject } = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        if (msg.error) {
          reject(new Error(`RPC error: ${JSON.stringify(msg.error)}`));
        } else {
          resolve(msg.result);
        }
      }
    });
    this.child.once('exit', (code) => {
      for (const { reject } of this.pending.values()) {
        reject(new Error(`legion rpc exited (${code}): ${this.stderr}`));
      }
      this.pending.clear();
    });
  }

  get pid() {
    return this.child.pid;
  }

  request(method, params) {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', id, method, params })}\n`);
    });
  }

  async hello() {
    return this.request('hello', { client: 'bench', version: '0.1.0', protocol: 1 });
  }

  async call(tool, args) {
    return this.request('call', { tool, args });
  }

  async close() {
    this.rl.close();
    try {
      this.child.stdin.end();
    } catch {
      // Ignore.
    }
    await new Promise((resolve) => {
      const t = setTimeout(() => {
        try {
          this.child.kill('SIGKILL');
        } catch {
          // Ignore.
        }
        resolve();
      }, 5000);
      this.child.once('exit', () => {
        clearTimeout(t);
        resolve();
      });
    });
  }
}

function chunkArray(arr, size) {
  const out = [];
  for (let i = 0; i < arr.length; i += size) {
    out.push(arr.slice(i, i + size));
  }
  return out;
}

export async function runFsToolsSuite({ scratchRoot, fixtures, variants, runsOverride, casesFilter = null }) {
  const cases = [
    {
      id: 'fs-list-10k-500',
      name: 'fs.list — 10,000 entries (limit 500)',
      runs: runsOverride ?? 5,
      run: async (rpc) => {
        const m0 = readProcMetrics(rpc.pid);
        const t0 = process.hrtime.bigint();
        const res = await rpc.call('fs.list', { path: fixtures.list10k, limit: 500, sort: 'name' });
        const wallMs = Number(process.hrtime.bigint() - t0) / 1e6;
        const m1 = readProcMetrics(rpc.pid);
        if (!res?.entries?.length) throw new Error('fs.list 10k returned empty');
        return {
          wall_ms: wallMs,
          peak_rss_mib: m1.vmHwmMiB,
          rss_after_mib: m1.vmRssMiB,
          cpu_s: m1.cpuSecs != null && m0.cpuSecs != null ? Math.max(0, m1.cpuSecs - m0.cpuSecs) : null,
          threads: m1.threads,
        };
      },
    },
    {
      id: 'fs-list-100k-500',
      name: 'fs.list — 100,000 entries (limit 500)',
      highlight: true,
      runs: runsOverride ?? 5,
      run: async (rpc) => {
        const m0 = readProcMetrics(rpc.pid);
        const t0 = process.hrtime.bigint();
        const res = await rpc.call('fs.list', { path: fixtures.list100k, limit: 500, sort: 'name' });
        const wallMs = Number(process.hrtime.bigint() - t0) / 1e6;
        const m1 = readProcMetrics(rpc.pid);
        if (!res?.entries?.length) throw new Error('fs.list 100k returned empty');
        return {
          wall_ms: wallMs,
          peak_rss_mib: m1.vmHwmMiB,
          rss_after_mib: m1.vmRssMiB,
          cpu_s: m1.cpuSecs != null && m0.cpuSecs != null ? Math.max(0, m1.cpuSecs - m0.cpuSecs) : null,
          threads: m1.threads,
        };
      },
    },
    {
      id: 'fs-list-100k-5000',
      name: 'fs.list — 100,000 entries (limit 5,000)',
      runs: runsOverride ?? 5,
      run: async (rpc) => {
        const m0 = readProcMetrics(rpc.pid);
        const t0 = process.hrtime.bigint();
        const res = await rpc.call('fs.list', { path: fixtures.list100k, limit: 5000, sort: 'name' });
        const wallMs = Number(process.hrtime.bigint() - t0) / 1e6;
        const m1 = readProcMetrics(rpc.pid);
        if (!res?.entries?.length) throw new Error('fs.list 100k/5000 returned empty');
        return {
          wall_ms: wallMs,
          peak_rss_mib: m1.vmHwmMiB,
          rss_after_mib: m1.vmRssMiB,
          cpu_s: m1.cpuSecs != null && m0.cpuSecs != null ? Math.max(0, m1.cpuSecs - m0.cpuSecs) : null,
          threads: m1.threads,
        };
      },
    },
    {
      id: 'fs-find-100k',
      name: 'fs.find — tree of 100,000 files (name query)',
      highlight: true,
      note: '100 dirs x 1,000 files; matches 10 needle files across the whole tree',
      runs: runsOverride ?? 5,
      run: async (rpc) => {
        const m0 = readProcMetrics(rpc.pid);
        const t0 = process.hrtime.bigint();
        const res = await rpc.call('fs.find', { path: fixtures.tree100k, name: 'needle report' });
        const wallMs = Number(process.hrtime.bigint() - t0) / 1e6;
        const m1 = readProcMetrics(rpc.pid);
        if (!res?.matches || res.matches.length < 10) {
          throw new Error(`fs.find expected 10 matches, got ${res?.matches?.length}`);
        }
        return {
          wall_ms: wallMs,
          peak_rss_mib: m1.vmHwmMiB,
          rss_after_mib: m1.vmRssMiB,
          cpu_s: m1.cpuSecs != null && m0.cpuSecs != null ? Math.max(0, m1.cpuSecs - m0.cpuSecs) : null,
          threads: m1.threads,
        };
      },
    },
    {
      id: 'fs-thumb-300-cold',
      name: 'fs.thumb — 300 pictures (1280×720 JPEG, 10 batches of 32, cold cache)',
      highlight: true,
      note: 'Fresh thumb-cache per run; measures node VmHWM across all 10 batches',
      runs: runsOverride ?? 3,
      freshProcess: true,
      run: async (rpc, nodeHome) => {
        fs.rmSync(path.join(nodeHome, 'state', 'thumb-cache'), { recursive: true, force: true });
        const batches = chunkArray(fixtures.pics300.files, 32);
        const m0 = readProcMetrics(rpc.pid);
        const t0 = process.hrtime.bigint();
        let okCount = 0;
        for (const batch of batches) {
          const res = await rpc.call('fs.thumb', { paths: batch, size: 256 });
          for (const item of res.results || []) {
            if (item.ok) okCount += 1;
          }
        }
        const wallMs = Number(process.hrtime.bigint() - t0) / 1e6;
        const m1 = readProcMetrics(rpc.pid);
        if (okCount !== 300) {
          throw new Error(`fs.thumb succeeded on only ${okCount}/300 pictures`);
        }
        return {
          wall_ms: wallMs,
          peak_rss_mib: m1.vmHwmMiB,
          rss_after_mib: m1.vmRssMiB,
          cpu_s: m1.cpuSecs != null && m0.cpuSecs != null ? Math.max(0, m1.cpuSecs - m0.cpuSecs) : null,
          threads: m1.threads,
        };
      },
    },
    {
      id: 'fs-tags-300-cold',
      name: 'fs.tags — 300 songs (ID3v2 MP3 in one folder, 10 batches of 32, cold cache)',
      highlight: true,
      note: 'Fresh thumb-cache per run; measures latency and node VmHWM',
      runs: runsOverride ?? 3,
      freshProcess: true,
      run: async (rpc, nodeHome) => {
        fs.rmSync(path.join(nodeHome, 'state', 'thumb-cache'), { recursive: true, force: true });
        const batches = chunkArray(fixtures.songs300.files, 32);
        const m0 = readProcMetrics(rpc.pid);
        const t0 = process.hrtime.bigint();
        let okCount = 0;
        for (const batch of batches) {
          const res = await rpc.call('fs.tags', { paths: batch });
          for (const item of res.results || []) {
            if (item.ok) okCount += 1;
          }
        }
        const wallMs = Number(process.hrtime.bigint() - t0) / 1e6;
        const m1 = readProcMetrics(rpc.pid);
        if (okCount !== 300) {
          throw new Error(`fs.tags succeeded on only ${okCount}/300 songs`);
        }
        return {
          wall_ms: wallMs,
          peak_rss_mib: m1.vmHwmMiB,
          rss_after_mib: m1.vmRssMiB,
          cpu_s: m1.cpuSecs != null && m0.cpuSecs != null ? Math.max(0, m1.cpuSecs - m0.cpuSecs) : null,
          threads: m1.threads,
        };
      },
    },
  ];

  const selectedCases = casesFilter ? cases.filter((c) => casesFilter.has(c.id)) : cases;
  const rows = [];
  for (const c of selectedCases) {
    console.error(`[bench:fs-tools] ${c.name} (${c.runs} runs x ${variants.length} variant(s))...`);
    const samplesByVariant = new Map(variants.map((v) => [v.id, []]));
    for (let iter = 0; iter < c.runs; iter += 1) {
      const order = iter % 2 === 0 ? variants : [...variants].reverse();
      for (const v of order) {
        const nodeRoot = path.join(scratchRoot, 'nodes-fs', `${v.id}-${c.id}`);
        const nodeHome = path.join(nodeRoot, 'legion');
        const userHome = path.join(nodeRoot, 'user');
        writeThrowawayNode({ legionHome: nodeHome, userHome, name: 'tower' });
        const rpc = new RpcSession(v.bin, nodeHome, userHome);
        try {
          await rpc.hello();
          const sample = await c.run(rpc, nodeHome, iter);
          samplesByVariant.get(v.id).push(sample);
        } finally {
          await rpc.close();
        }
      }
    }
    const row = { id: c.id, name: c.name, highlight: Boolean(c.highlight), note: c.note };
    for (const v of variants) {
      row[v.slot] = summarizeSamples(samplesByVariant.get(v.id));
    }
    rows.push(row);
  }

  return {
    id: 'fs-tools',
    title: '3. Filesystem & Media Tools (`legion rpc`)',
    description:
      'Latency and node process peak RSS (VmHWM) over a fresh `legion rpc` process per run so peak memory reflects each operation cleanly.',
    columns: [
      { key: 'wall_ms', label: 'Latency', unit: 'ms', decimals: 1, primary: true, higherIsBetter: false },
      { key: 'peak_rss_mib', label: 'Node Peak RSS', unit: 'MiB', decimals: 1 },
      { key: 'cpu_s', label: 'Node CPU', unit: 's', decimals: 3 },
      { key: 'threads', label: 'Node Threads', unit: '', decimals: 0 },
    ],
    rows,
    notes: [
      'Each run launches a fresh `legion rpc` node so `VmHWM` measures peak memory for that specific workload rather than an earlier call.',
      'For `fs.thumb` and `fs.tags`, `state/thumb-cache` is cleared before every run to measure cold generation.',
    ],
  };
}
