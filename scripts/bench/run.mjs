#!/usr/bin/env node
// Repeatable loopback performance benchmark suite for Legion.
//
// Usage:
//   node scripts/bench/run.mjs [options]
//
// Options:
//   --bin <path>           Candidate legion binary (default: target/release/legion)
//   --compare <path>       Optional baseline binary for interleaved A/B comparison
//   --label <name>         Display label for candidate binary
//   --compare-label <name> Display label for baseline binary (default: baseline)
//   --suite <list>         Comma-separated subset of: stream,http-content,fs-tools,cold-start,ui-browser
//   --case <list>          Optional comma-separated subset of case IDs within selected suites
//   --runs <n>             Override run count per benchmark case
//   --scratch <dir>        Scratch/fixture directory (default: ~/.cache/legion-perf/work)
//   --out <dir>            Report output dir (default: ~/Documents/legion/perf/<date>-<rev>/)
//   --max-load <n>         Max 1-min load average to wait for before each series (default: cpu count)

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { ensureFixtures } from './lib/fixtures.mjs';
import { collectMachineInfo, waitForQuietLoad } from './lib/proc.mjs';
import { renderHtmlReport } from './lib/report.mjs';
import { compareStats, formatNum } from './lib/stats.mjs';
import { runColdStartSuite } from './suites/cold_start.mjs';
import { runFsToolsSuite } from './suites/fs_tools.mjs';
import { runHttpContentSuite } from './suites/http_content.mjs';
import { runStreamSuite } from './suites/stream.mjs';
import { runUiBrowserSuite } from './suites/ui_browser.mjs';

const BENCH_DIR = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(BENCH_DIR, '..', '..');

const ALL_SUITES = ['stream', 'http-content', 'fs-tools', 'cold-start', 'ui-browser'];

function parseArgs(argv) {
  const opts = {
    bin: path.join(REPO_ROOT, 'target', 'release', process.platform === 'win32' ? 'legion.exe' : 'legion'),
    compare: null,
    label: null,
    compareLabel: 'baseline',
    suites: [...ALL_SUITES],
    cases: null,
    runs: null,
    scratch: path.join(os.homedir(), '.cache', 'legion-perf', 'work'),
    out: null,
    maxLoad: os.cpus().length,
  };

  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    const next = () => {
      if (i + 1 >= argv.length) throw new Error(`${arg} requires a value`);
      return argv[++i];
    };
    if (arg === '--bin') opts.bin = path.resolve(next());
    else if (arg === '--compare') opts.compare = path.resolve(next());
    else if (arg === '--label') opts.label = next();
    else if (arg === '--compare-label') opts.compareLabel = next();
    else if (arg === '--suite') {
      const list = next()
        .split(',')
        .map((s) => s.trim())
        .filter(Boolean);
      for (const s of list) {
        if (!ALL_SUITES.includes(s)) {
          throw new Error(`Unknown suite "${s}". Valid: ${ALL_SUITES.join(', ')}`);
        }
      }
      opts.suites = list;
    } else if (arg === '--case') {
      const list = next()
        .split(',')
        .map((s) => s.trim())
        .filter(Boolean);
      if (list.length === 0) throw new Error('--case requires at least one case id');
      opts.cases = new Set(list);
    } else if (arg === '--runs') {
      const n = Number(next());
      if (!Number.isInteger(n) || n < 1) throw new Error('--runs must be a positive integer');
      opts.runs = n;
    } else if (arg === '--scratch') opts.scratch = path.resolve(next());
    else if (arg === '--out') opts.out = path.resolve(next());
    else if (arg === '--max-load') {
      const n = Number(next());
      if (!Number.isFinite(n) || n <= 0) throw new Error('--max-load must be > 0');
      opts.maxLoad = n;
    } else if (arg === '--help' || arg === '-h') {
      console.log(
        [
          'Usage: node scripts/bench/run.mjs [options]',
          '  --bin <path>           Candidate legion binary (default: target/release/legion)',
          '  --compare <path>       Baseline binary for interleaved A/B comparison',
          '  --label <name>         Label for candidate binary',
          '  --compare-label <name> Label for baseline binary',
          `  --suite <list>         Subset of ${ALL_SUITES.join(',')}`,
          '  --case <list>          Optional comma-separated case IDs to run',
          '  --runs <n>             Override iterations per case',
          '  --scratch <dir>        Scratch/fixture directory',
          '  --out <dir>            Output directory for results.json and index.html',
          '  --max-load <n>         Wait if 1-min load average exceeds n',
        ].join('\n'),
      );
      process.exit(0);
    } else {
      throw new Error(`Unknown argument: ${arg}`);
    }
  }
  return opts;
}

function printConsoleSummary(report) {
  const hasCompare = Boolean(report.compare);
  console.log('\n=== Legion Performance Summary ===');
  console.log(
    `Machine: ${report.machine.cpuModel} (${report.machine.physicalCores ?? '?'}C/${report.machine.logicalCpus}T), ${report.machine.totalRamGiB} GiB RAM, rev ${report.machine.gitRev}`,
  );
  for (const suite of report.suites) {
    console.log(`\n## ${suite.title}`);
    if (suite.loadBefore) {
      console.log(`   uptime: ${suite.loadBefore.raw}`);
    }
    const primary = suite.columns.find((c) => c.primary);
    for (const row of suite.rows) {
      const cStat = primary ? row.candidate?.[primary.key] : null;
      const cText =
        cStat?.median != null
          ? `${formatNum(cStat.median, primary.decimals ?? 1)} ${primary.unit} (IQR ${formatNum(cStat.iqr, primary.decimals ?? 1)})`
          : '—';
      if (hasCompare && primary) {
        const bStat = row.baseline?.[primary.key];
        const bText =
          bStat?.median != null ? `${formatNum(bStat.median, primary.decimals ?? 1)} ${primary.unit}` : '—';
        const cmp = compareStats(bStat, cStat, Boolean(primary.higherIsBetter));
        const dText =
          cmp?.pctDelta != null
            ? `${cmp.pctDelta >= 0 ? '+' : ''}${cmp.pctDelta.toFixed(1)}% (${cmp.speedup.toFixed(2)}x)`
            : '—';
        console.log(`  - ${row.name}: ${bText} -> ${cText}  [${dText}]`);
      } else {
        console.log(`  - ${row.name}: ${cText}`);
      }
    }
  }
  console.log('');
}

async function main() {
  const opts = parseArgs(process.argv.slice(2));
  if (!fs.existsSync(opts.bin)) {
    throw new Error(`Candidate binary not found: ${opts.bin}`);
  }
  if (opts.compare && !fs.existsSync(opts.compare)) {
    throw new Error(`Compare binary not found: ${opts.compare}`);
  }

  const machine = collectMachineInfo(REPO_ROOT);
  const dateStamp = new Date().toISOString().slice(0, 10);
  const outDir =
    opts.out || path.join(os.homedir(), 'Documents', 'legion', 'perf', `${dateStamp}-${machine.gitRev}`);

  fs.mkdirSync(opts.scratch, { recursive: true });
  fs.mkdirSync(outDir, { recursive: true });

  const variants = [];
  if (opts.compare) {
    variants.push({
      id: 'baseline',
      slot: 'baseline',
      label: opts.compareLabel,
      bin: opts.compare,
    });
  }
  variants.push({
    id: 'candidate',
    slot: 'candidate',
    label: opts.label || machine.gitRev,
    bin: opts.bin,
  });

  const neededSuites = new Set(opts.suites);
  console.error(`[bench] preparing fixtures in ${opts.scratch}...`);
  const fixtures = ensureFixtures(opts.scratch, variants[0].bin, neededSuites, opts.cases);

  const runners = {
    stream: runStreamSuite,
    'http-content': runHttpContentSuite,
    'fs-tools': runFsToolsSuite,
    'cold-start': runColdStartSuite,
    'ui-browser': runUiBrowserSuite,
  };

  const jsonPath = path.join(outDir, 'results.json');
  const htmlPath = path.join(outDir, 'index.html');
  const writeReport = () => {
    const report = {
      schemaVersion: 1,
      machine,
      candidate: {
        label: opts.label || machine.gitRev,
        bin: opts.bin,
      },
      compare: opts.compare
        ? {
            label: opts.compareLabel,
            bin: opts.compare,
          }
        : null,
      suites: suitesOut,
    };
    fs.writeFileSync(jsonPath, `${JSON.stringify(report, null, 2)}\n`);
    fs.writeFileSync(htmlPath, renderHtmlReport(report));
    return report;
  };

  const suitesOut = [];
  for (const suiteId of opts.suites) {
    const loadBefore = await waitForQuietLoad(opts.maxLoad);
    console.error(`[bench] === starting suite "${suiteId}" | ${loadBefore.raw} ===`);
    const result = await runners[suiteId]({
      repoRoot: REPO_ROOT,
      scratchRoot: opts.scratch,
      fixtures,
      variants,
      runsOverride: opts.runs,
      casesFilter: opts.cases,
    });
    result.loadBefore = loadBefore;
    suitesOut.push(result);
    writeReport();
  }

  const report = writeReport();

  printConsoleSummary(report);
  console.error(`[bench] wrote ${jsonPath}`);
  console.error(`[bench] wrote ${htmlPath}`);
}

main().catch((err) => {
  console.error(`[bench] fatal: ${err.stack || err.message}`);
  process.exit(1);
});
