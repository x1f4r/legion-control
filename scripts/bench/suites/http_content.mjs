// Suite 2: HTTP door `/v1/content` (TTFB, full sequential 1 GiB read, and 200 random 1 MiB ranges).
// Covers local direct (`legion serve` local file), remote direct (`legion serve` -> remote `legion stream`),
// and relayed `stream.relay` keep_open ranged/sequential reads (while probing whether HTTP door `via` is supported).

import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  envCommandArgv,
  readProcMetrics,
  runLegionSync,
  startLegionServe,
  writeThrowawayNode,
} from '../lib/proc.mjs';
import { summarizeSamples } from '../lib/stats.mjs';

const SUITE_DIR = path.dirname(fileURLToPath(import.meta.url));
const STREAM_PY = path.join(SUITE_DIR, '..', 'lib', 'stream_bench.py');

function deterministicOffsets(fileSize, rangeSize, count, seed = 0x5eed1234) {
  let s = seed >>> 0;
  const maxOffset = Math.max(0, fileSize - rangeSize);
  const offsets = [];
  for (let i = 0; i < count; i += 1) {
    s = (Math.imul(s, 1664525) + 1013904223) >>> 0;
    const frac = s / 0x100000000;
    // Align to 4 KiB boundaries across the full 1 GiB range.
    const off = Math.floor((frac * maxOffset) / 4096) * 4096;
    offsets.push(off);
  }
  return offsets;
}

function httpJsonPost(agent, baseUrl, urlPath, token, payload) {
  const body = JSON.stringify(payload);
  return new Promise((resolve, reject) => {
    const u = new URL(urlPath, baseUrl);
    const req = http.request(
      {
        hostname: u.hostname,
        port: u.port,
        path: u.pathname + u.search,
        method: 'POST',
        agent,
        headers: {
          Authorization: `Bearer ${token}`,
          'Content-Type': 'application/json',
          'Content-Length': Buffer.byteLength(body),
        },
      },
      (res) => {
        const chunks = [];
        res.on('data', (c) => chunks.push(c));
        res.on('end', () => {
          const text = Buffer.concat(chunks).toString('utf8');
          let json = null;
          try {
            json = JSON.parse(text);
          } catch {
            // Non-JSON error.
          }
          resolve({ status: res.statusCode, json, text });
        });
      },
    );
    req.on('error', reject);
    req.end(body);
  });
}

function httpGetDrain(agent, baseUrl, urlPath, headers = {}) {
  return new Promise((resolve, reject) => {
    const u = new URL(urlPath, baseUrl);
    const t0 = process.hrtime.bigint();
    let ttfbMs = null;
    let bytes = 0;
    const req = http.request(
      {
        hostname: u.hostname,
        port: u.port,
        path: u.pathname + u.search,
        method: 'GET',
        agent,
        headers,
      },
      (res) => {
        res.on('data', (chunk) => {
          if (ttfbMs === null) {
            ttfbMs = Number(process.hrtime.bigint() - t0) / 1e6;
          }
          bytes += chunk.length;
        });
        res.on('end', () => {
          const wallMs = Number(process.hrtime.bigint() - t0) / 1e6;
          if (res.statusCode !== 200 && res.statusCode !== 206) {
            reject(new Error(`GET ${urlPath} returned HTTP ${res.statusCode}`));
            return;
          }
          resolve({ status: res.statusCode, bytes, ttfbMs: ttfbMs ?? wallMs, wallMs });
        });
      },
    );
    req.on('error', reject);
    req.end();
  });
}

function runPythonStream(spec) {
  const out = execFileSync('python3', [STREAM_PY], {
    input: JSON.stringify(spec),
    encoding: 'utf8',
    maxBuffer: 16 * 1024 * 1024,
  });
  return JSON.parse(out.trim());
}

async function setupHttpVariant(scratchRoot, v) {
  const root = path.join(scratchRoot, 'nodes-http', v.id);
  const hubHome = path.join(root, 'hub-legion');
  const hubUser = path.join(root, 'hub-user');
  const targetHome = path.join(root, 'target-legion');
  const targetUser = path.join(root, 'target-user');
  const relayHome = path.join(root, 'relay-legion');
  const relayUser = path.join(root, 'relay-user');

  writeThrowawayNode({ legionHome: targetHome, userHome: targetUser, name: 'target' });

  const targetCmd = envCommandArgv(v.bin, targetHome, targetUser);
  writeThrowawayNode({
    legionHome: relayHome,
    userHome: relayUser,
    name: 'relay',
    fleetToml: `[machines.target]\ncommand = ${JSON.stringify(targetCmd)}\n`,
  });

  const relayCmd = envCommandArgv(v.bin, relayHome, relayUser);
  writeThrowawayNode({
    legionHome: hubHome,
    userHome: hubUser,
    name: 'hub',
    fleetToml: [
      '[machines.target]',
      `command = ${JSON.stringify(targetCmd)}`,
      '',
      '[machines.relay]',
      `command = ${JSON.stringify(relayCmd)}`,
      '',
      '[machines.far]',
      'via = "relay"',
      '',
    ].join('\n'),
  });

  // Remove old token if present so re-runs stay idempotent.
  try {
    fs.rmSync(path.join(hubHome, 'config', 'tokens.toml'), { force: true });
  } catch {
    // Ignore.
  }
  const tokOut = JSON.parse(
    runLegionSync(v.bin, ['token.add', 'name=bench', 'principal=operator', '--json'], hubHome, hubUser),
  );
  const server = await startLegionServe(v.bin, hubHome, hubUser);
  return {
    hubHome,
    hubUser,
    targetHome,
    targetUser,
    relayHome,
    relayUser,
    token: tokOut.token,
    server,
  };
}

export async function runHttpContentSuite({ scratchRoot, fixtures, variants, runsOverride }) {
  const blob1g = fixtures.blobs['1g'];
  const rangeSize = 1024 * 1024;
  const randomOffsets = deterministicOffsets(blob1g.bytes, rangeSize, 200);
  const notes = [];

  const envByVariant = new Map();
  for (const v of variants) {
    envByVariant.set(v.id, await setupHttpVariant(scratchRoot, v));
  }

  const agent = new http.Agent({ keepAlive: true, maxSockets: 4 });

  try {
    // Check whether HTTP door supports `via` relay on this binary (honestly document if unsupported).
    const firstEnv = envByVariant.get(variants[0].id);
    const viaProbe = await httpJsonPost(agent, firstEnv.server.url, '/v1/content', firstEnv.token, {
      on: 'far',
      path: blob1g.path,
    });
    const httpSupportsVia = viaProbe.status === 200;
    if (!httpSupportsVia) {
      notes.push(
        `HTTP door POST /v1/content for a \`via\`-relayed machine currently returns HTTP ${viaProbe.status} (${viaProbe.json?.code || 'unsupported'}); relayed content below measures the underlying \`stream.relay\` keep_open ranged-read path directly.`,
      );
    }

    const cases = [
      {
        id: 'http-local-ttfb',
        name: 'HTTP /v1/content local — TTFB (1st byte)',
        primaryKey: 'ttfb_ms',
        runs: runsOverride ?? 5,
        run: async (_v, env) => {
          const ticket = await httpJsonPost(agent, env.server.url, '/v1/content', env.token, {
            path: blob1g.path,
          });
          if (ticket.status !== 200) throw new Error(`ticket failed: ${ticket.text}`);
          const m0 = readProcMetrics(env.server.pid);
          const r = await httpGetDrain(agent, env.server.url, ticket.json.url, {
            Range: 'bytes=0-65535',
          });
          const m1 = readProcMetrics(env.server.pid);
          return {
            wall_ms: r.wallMs,
            ttfb_ms: r.ttfbMs,
            mib_s: (r.bytes / 1048576) / Math.max(r.wallMs / 1000, 1e-9),
            server_cpu_s: m1.cpuSecs != null && m0.cpuSecs != null ? Math.max(0, m1.cpuSecs - m0.cpuSecs) : null,
            server_rss_mib: m1.vmHwmMiB,
          };
        },
      },
      {
        id: 'http-local-seq-1g',
        name: 'HTTP /v1/content local — Full sequential 1 GiB',
        highlight: true,
        runs: runsOverride ?? 3,
        run: async (_v, env) => {
          const ticket = await httpJsonPost(agent, env.server.url, '/v1/content', env.token, {
            path: blob1g.path,
          });
          if (ticket.status !== 200) throw new Error(`ticket failed: ${ticket.text}`);
          const m0 = readProcMetrics(env.server.pid);
          const r = await httpGetDrain(agent, env.server.url, ticket.json.url);
          const m1 = readProcMetrics(env.server.pid);
          return {
            wall_ms: r.wallMs,
            ttfb_ms: r.ttfbMs,
            mib_s: (r.bytes / 1048576) / Math.max(r.wallMs / 1000, 1e-9),
            server_cpu_s: m1.cpuSecs != null && m0.cpuSecs != null ? Math.max(0, m1.cpuSecs - m0.cpuSecs) : null,
            server_rss_mib: m1.vmHwmMiB,
          };
        },
      },
      {
        id: 'http-local-seek-200x1m',
        name: 'HTTP /v1/content local — 200 random 1 MiB ranges in 1 GiB',
        runs: runsOverride ?? 3,
        run: async (_v, env) => {
          const ticket = await httpJsonPost(agent, env.server.url, '/v1/content', env.token, {
            path: blob1g.path,
          });
          if (ticket.status !== 200) throw new Error(`ticket failed: ${ticket.text}`);
          const m0 = readProcMetrics(env.server.pid);
          const t0 = process.hrtime.bigint();
          let totalBytes = 0;
          const ttfbs = [];
          for (const off of randomOffsets) {
            const r = await httpGetDrain(agent, env.server.url, ticket.json.url, {
              Range: `bytes=${off}-${off + rangeSize - 1}`,
            });
            totalBytes += r.bytes;
            ttfbs.push(r.ttfbMs);
          }
          const wallMs = Number(process.hrtime.bigint() - t0) / 1e6;
          const m1 = readProcMetrics(env.server.pid);
          ttfbs.sort((a, b) => a - b);
          return {
            wall_ms: wallMs,
            ttfb_ms: ttfbs[Math.floor(ttfbs.length / 2)],
            mib_s: (totalBytes / 1048576) / Math.max(wallMs / 1000, 1e-9),
            server_cpu_s: m1.cpuSecs != null && m0.cpuSecs != null ? Math.max(0, m1.cpuSecs - m0.cpuSecs) : null,
            server_rss_mib: m1.vmHwmMiB,
          };
        },
      },
      {
        id: 'http-remote-ttfb',
        name: 'HTTP /v1/content remote (1-hop) — TTFB (1st byte)',
        note: 'hub legion serve -> target legion stream (keep_open)',
        runs: runsOverride ?? 5,
        run: async (_v, env) => {
          const ticket = await httpJsonPost(agent, env.server.url, '/v1/content', env.token, {
            on: 'target',
            path: blob1g.path,
          });
          if (ticket.status !== 200) throw new Error(`remote ticket failed: ${ticket.text}`);
          const m0 = readProcMetrics(env.server.pid);
          const r = await httpGetDrain(agent, env.server.url, ticket.json.url, {
            Range: 'bytes=0-65535',
          });
          const m1 = readProcMetrics(env.server.pid);
          return {
            wall_ms: r.wallMs,
            ttfb_ms: r.ttfbMs,
            mib_s: (r.bytes / 1048576) / Math.max(r.wallMs / 1000, 1e-9),
            server_cpu_s: m1.cpuSecs != null && m0.cpuSecs != null ? Math.max(0, m1.cpuSecs - m0.cpuSecs) : null,
            server_rss_mib: m1.vmHwmMiB,
          };
        },
      },
      {
        id: 'http-remote-seq-1g',
        name: 'HTTP /v1/content remote (1-hop) — Full sequential 1 GiB',
        highlight: true,
        note: 'hub legion serve -> target via RemoteWindowedReader',
        runs: runsOverride ?? 3,
        run: async (_v, env) => {
          const ticket = await httpJsonPost(agent, env.server.url, '/v1/content', env.token, {
            on: 'target',
            path: blob1g.path,
          });
          if (ticket.status !== 200) throw new Error(`remote ticket failed: ${ticket.text}`);
          const m0 = readProcMetrics(env.server.pid);
          const r = await httpGetDrain(agent, env.server.url, ticket.json.url);
          const m1 = readProcMetrics(env.server.pid);
          return {
            wall_ms: r.wallMs,
            ttfb_ms: r.ttfbMs,
            mib_s: (r.bytes / 1048576) / Math.max(r.wallMs / 1000, 1e-9),
            server_cpu_s: m1.cpuSecs != null && m0.cpuSecs != null ? Math.max(0, m1.cpuSecs - m0.cpuSecs) : null,
            server_rss_mib: m1.vmHwmMiB,
          };
        },
      },
      {
        id: 'http-remote-seek-200x1m',
        name: 'HTTP /v1/content remote (1-hop) — 200 random 1 MiB ranges in 1 GiB',
        highlight: true,
        runs: runsOverride ?? 3,
        run: async (_v, env) => {
          const ticket = await httpJsonPost(agent, env.server.url, '/v1/content', env.token, {
            on: 'target',
            path: blob1g.path,
          });
          if (ticket.status !== 200) throw new Error(`remote ticket failed: ${ticket.text}`);
          const m0 = readProcMetrics(env.server.pid);
          const t0 = process.hrtime.bigint();
          let totalBytes = 0;
          const ttfbs = [];
          for (const off of randomOffsets) {
            const r = await httpGetDrain(agent, env.server.url, ticket.json.url, {
              Range: `bytes=${off}-${off + rangeSize - 1}`,
            });
            totalBytes += r.bytes;
            ttfbs.push(r.ttfbMs);
          }
          const wallMs = Number(process.hrtime.bigint() - t0) / 1e6;
          const m1 = readProcMetrics(env.server.pid);
          ttfbs.sort((a, b) => a - b);
          return {
            wall_ms: wallMs,
            ttfb_ms: ttfbs[Math.floor(ttfbs.length / 2)],
            mib_s: (totalBytes / 1048576) / Math.max(wallMs / 1000, 1e-9),
            server_cpu_s: m1.cpuSecs != null && m0.cpuSecs != null ? Math.max(0, m1.cpuSecs - m0.cpuSecs) : null,
            server_rss_mib: m1.vmHwmMiB,
          };
        },
      },
      {
        id: 'relay-stream-seek-200x1m',
        name: 'Relayed content (`stream.relay` keep_open) — 200 random 1 MiB ranges in 1 GiB',
        note: 'Underlying stream.relay keep_open path for 200 x 1 MiB video seeks',
        runs: runsOverride ?? 3,
        run: async (v, env) => {
          const res = runPythonStream({
            mode: 'get_keep_open_batch',
            bin: v.bin,
            entry_legion_home: env.relayHome,
            entry_user_home: env.relayUser,
            relay_on: 'target',
            requests: randomOffsets.map((off) => ({
              path: blob1g.path,
              offset: off,
              length: rangeSize,
            })),
          });
          return {
            wall_ms: res.wall_ms,
            ttfb_ms: res.req_p50_ms,
            mib_s: res.mib_s,
            server_cpu_s: res.server_cpu_s,
            server_rss_mib: res.server_rss_mib,
          };
        },
      },
    ];

    const rows = [];
    for (const c of cases) {
      console.error(`[bench:http-content] ${c.name} (${c.runs} runs x ${variants.length} variant(s))...`);
      const samplesByVariant = new Map(variants.map((v) => [v.id, []]));
      for (let iter = 0; iter < c.runs; iter += 1) {
        const order = iter % 2 === 0 ? variants : [...variants].reverse();
        for (const v of order) {
          const sample = await c.run(v, envByVariant.get(v.id), iter);
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
      id: 'http-content',
      title: '2. HTTP Door Content (`/v1/content` & Relayed Seeks)',
      description:
        'TTFB, full sequential 1 GiB streaming, and 200 deterministic random 1 MiB Range requests in a 1 GiB file (video seek pattern).',
      columns: [
        { key: 'mib_s', label: 'Throughput', unit: 'MiB/s', decimals: 1, primary: true, higherIsBetter: true },
        { key: 'wall_ms', label: 'Total Wall', unit: 'ms', decimals: 1 },
        { key: 'ttfb_ms', label: 'TTFB / Req p50', unit: 'ms', decimals: 2 },
        { key: 'server_cpu_s', label: 'Server CPU', unit: 's', decimals: 3 },
        { key: 'server_rss_mib', label: 'Server Peak RSS', unit: 'MiB', decimals: 1 },
      ],
      rows,
      notes,
    };
  } finally {
    agent.destroy();
    for (const env of envByVariant.values()) {
      await env.server.stop();
    }
  }
}
