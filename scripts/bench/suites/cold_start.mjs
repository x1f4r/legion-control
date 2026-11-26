// Suite 4: Cold start — process start to first answered `meta.whoami` call.
// Covers:
//   1. `legion rpc` over a local pipe
//   2. `legion gate --as operator --home <dir>` forced-command direct (SSH_ORIGINAL_COMMAND="legion rpc")
//   3. `legion gate` behind a real throwaway loopback `sshd` forced-command key

import { spawn } from 'node:child_process';
import path from 'node:path';
import readline from 'node:readline';
import {
  cleanLegionEnv,
  readProcMetrics,
  startLoopbackSshd,
  writeThrowawayNode,
} from '../lib/proc.mjs';
import { summarizeSamples } from '../lib/stats.mjs';

const HELLO_LINE = `${JSON.stringify({
  jsonrpc: '2.0',
  id: 1,
  method: 'hello',
  params: { client: 'bench', version: '0.1.0', protocol: 1 },
})}\n`;

const WHOAMI_LINE = `${JSON.stringify({
  jsonrpc: '2.0',
  id: 2,
  method: 'call',
  params: { tool: 'meta.whoami', args: {} },
})}\n`;

async function measureOneSpawn(cmd, args, env) {
  const t0 = process.hrtime.bigint();
  const child = spawn(cmd, args, { env, stdio: ['pipe', 'pipe', 'pipe'] });
  let stderr = '';
  child.stderr.on('data', (d) => {
    stderr += d.toString('utf8');
  });

  // Pipeline hello + first tool call immediately so cold start measures pure startup + first answer.
  child.stdin.write(HELLO_LINE);
  child.stdin.write(WHOAMI_LINE);

  let helloMs = null;
  let firstAnswerMs = null;
  let rssMiB = null;
  let threads = null;

  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      try {
        child.kill('SIGKILL');
      } catch {
        // Ignore.
      }
      reject(new Error(`Cold start timed out for ${cmd}. stderr:\n${stderr}`));
    }, 15_000);

    const rl = readline.createInterface({ input: child.stdout });
    rl.on('line', (line) => {
      if (!line.trim()) return;
      let msg;
      try {
        msg = JSON.parse(line);
      } catch {
        return;
      }
      if (msg.id === 1 && helloMs === null) {
        if (msg.error) {
          clearTimeout(timer);
          rl.close();
          reject(new Error(`hello error: ${JSON.stringify(msg.error)}`));
          return;
        }
        helloMs = Number(process.hrtime.bigint() - t0) / 1e6;
      }
      if (msg.id === 2) {
        clearTimeout(timer);
        if (msg.error) {
          rl.close();
          reject(new Error(`meta.whoami error: ${JSON.stringify(msg.error)}`));
          return;
        }
        firstAnswerMs = Number(process.hrtime.bigint() - t0) / 1e6;
        const m = readProcMetrics(child.pid);
        rssMiB = m.vmHwmMiB;
        threads = m.threads;
        rl.close();
        resolve();
      }
    });
    child.once('exit', (code) => {
      if (firstAnswerMs === null) {
        clearTimeout(timer);
        rl.close();
        reject(new Error(`${cmd} exited early (${code}). stderr:\n${stderr}`));
      }
    });
  });

  try {
    child.stdin.end();
  } catch {
    // Ignore.
  }
  await new Promise((resolve) => {
    const t = setTimeout(() => {
      try {
        child.kill('SIGKILL');
      } catch {
        // Ignore.
      }
      resolve();
    }, 3000);
    child.once('exit', () => {
      clearTimeout(t);
      resolve();
    });
  });

  return {
    first_answer_ms: firstAnswerMs,
    hello_ms: helloMs,
    peak_rss_mib: rssMiB,
    threads,
  };
}

export async function runColdStartSuite({ scratchRoot, variants, runsOverride }) {
  const runs = runsOverride ?? 7;
  const setupByVariant = new Map();
  const sshds = [];

  try {
    for (const v of variants) {
      const root = path.join(scratchRoot, 'nodes-cold', v.id);
      const legionHome = path.join(root, 'legion');
      const userHome = path.join(root, 'user');
      writeThrowawayNode({ legionHome, userHome, name: 'tower' });

      let sshd = null;
      try {
        sshd = await startLoopbackSshd({
          workDir: path.join(root, 'sshd'),
          bin: v.bin,
          legionHome,
        });
        sshds.push(sshd);
      } catch (err) {
        console.error(`[bench:cold-start] loopback sshd unavailable (${err.message}); skipping sshd case.`);
      }
      setupByVariant.set(v.id, { legionHome, userHome, sshd });
    }

    const cases = [
      {
        id: 'cold-rpc-local',
        name: 'Cold start — `legion rpc` (local pipe)',
        highlight: true,
        runs,
        run: (v, s) =>
          measureOneSpawn(v.bin, ['rpc'], cleanLegionEnv(s.legionHome, s.userHome)),
      },
      {
        id: 'cold-gate-direct',
        name: 'Cold start — `legion gate` forced-command (direct pipe)',
        highlight: true,
        note: 'Spawns `legion gate --as operator --home <dir>` with SSH_ORIGINAL_COMMAND="legion rpc"',
        runs,
        run: (v, s) =>
          measureOneSpawn(
            v.bin,
            ['gate', '--as', 'operator', '--home', s.legionHome],
            cleanLegionEnv(s.legionHome, s.userHome, { SSH_ORIGINAL_COMMAND: 'legion rpc' }),
          ),
      },
    ];

    if (variants.every((v) => setupByVariant.get(v.id)?.sshd)) {
      cases.push({
        id: 'cold-gate-sshd',
        name: 'Cold start — `legion gate` over real loopback `sshd`',
        note: 'Includes OpenSSH TCP handshake + ed25519 pubkey auth + forced-command `legion gate`',
        runs,
        run: (_v, s) =>
          measureOneSpawn(
            'ssh',
            [
              '-T',
              '-o',
              'BatchMode=yes',
              '-o',
              'StrictHostKeyChecking=no',
              '-o',
              'UserKnownHostsFile=/dev/null',
              '-o',
              'LogLevel=ERROR',
              '-i',
              s.sshd.clientKey,
              '-p',
              String(s.sshd.port),
              `${s.sshd.user}@127.0.0.1`,
              'legion rpc',
            ],
            cleanLegionEnv(s.legionHome, s.userHome),
          ),
      });
    }

    const rows = [];
    for (const c of cases) {
      console.error(`[bench:cold-start] ${c.name} (${c.runs} runs x ${variants.length} variant(s))...`);
      const samplesByVariant = new Map(variants.map((v) => [v.id, []]));
      // Warm 1 throwaway run to populate OS binary pages in page cache before timing.
      for (const v of variants) {
        await c.run(v, setupByVariant.get(v.id));
      }
      for (let iter = 0; iter < c.runs; iter += 1) {
        const order = iter % 2 === 0 ? variants : [...variants].reverse();
        for (const v of order) {
          const sample = await c.run(v, setupByVariant.get(v.id));
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
      id: 'cold-start',
      title: '4. Cold Start (Spawn to First Answered Call)',
      description:
        'Wall-clock time from process spawn to receiving the `hello` and first `meta.whoami` JSON-RPC replies.',
      columns: [
        { key: 'first_answer_ms', label: 'First Answer', unit: 'ms', decimals: 2, primary: true, higherIsBetter: false },
        { key: 'hello_ms', label: 'Hello Reply', unit: 'ms', decimals: 2 },
        { key: 'peak_rss_mib', label: 'Peak RSS', unit: 'MiB', decimals: 1 },
        { key: 'threads', label: 'Threads', unit: '', decimals: 0 },
      ],
      rows,
      notes: [
        'One unmeasured warm-up run executes before timing so executable pages are resident in the OS page cache.',
      ],
    };
  } finally {
    for (const s of sshds) {
      await s.stop();
    }
  }
}
