import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';

const REPO_ROOT = path.resolve(import.meta.dirname, '..');
const SCRIPT = path.join(REPO_ROOT, 'scripts', 'ios-sim-check.sh');
const WORKFLOW = path.join(REPO_ROOT, '.github', 'workflows', 'legion-app.yml');

/**
 * Builds an isolated fake iOS simulator environment (fake `xcrun`, fake `swift`, fake
 * `ci-media-fixture.mjs`, and fake `ios-sim-resume.sh`) driven by a per-attempt schedule of
 * fake result files.
 *
 * Each entry in `attempts` describes what the fake launched app does on that attempt (1-based):
 * - `readyDelayMs`: ms before writing `legion-ready.txt` (null/negative = never ready)
 * - `steps`: array of `{ delayMs, report }` JSON objects written to `media-result.json` in order
 * - `seedStaleData`: if true, writes a marker file in `Library/Application Support/com.x1f4r.legion`
 *   so attempt 2 can prove `ios-sim-check.sh` wiped app data before relaunching
 * - `requireCleanData`: if true, fails pairing if the stale marker file still exists on launch
 */
function makeFakeSimEnv(t, { attempts }) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ios-sim-check-test-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));

  const binDir = path.join(dir, 'bin');
  const dataDir = path.join(dir, 'app-container');
  const shotsDir = path.join(dir, 'shots');
  const runnerTemp = path.join(dir, 'runner-temp');
  const simApp = path.join(dir, 'Legion.app');
  const legionBin = path.join(dir, 'legion');
  fs.mkdirSync(binDir, { recursive: true });
  fs.mkdirSync(dataDir, { recursive: true });
  fs.mkdirSync(shotsDir, { recursive: true });
  fs.mkdirSync(runnerTemp, { recursive: true });
  fs.mkdirSync(simApp, { recursive: true });
  fs.writeFileSync(legionBin, '#!/bin/sh\nexit 0\n', { mode: 0o755 });

  const callLog = path.join(dir, 'xcrun.log');
  const stateFile = path.join(dir, 'state.json');
  const staleMarker = path.join(
    dataDir,
    'Library',
    'Application Support',
    'com.x1f4r.legion',
    'legion-app.json',
  );
  fs.writeFileSync(stateFile, JSON.stringify({ launchCount: 0 }));

  const fixtureScript = path.join(dir, 'fake-fixture.mjs');
  fs.writeFileSync(
    fixtureScript,
    `#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
const args = process.argv.slice(2);
const work = args[args.indexOf('--work') + 1];
const out = args[args.indexOf('--out') + 1];
fs.mkdirSync(work, { recursive: true });
fs.writeFileSync(path.join(work, 'media-check.json'), JSON.stringify({
  machine: 'tower',
  invite: 'legion-invite-1.fake',
  image_path: '/tmp/a.png',
  video_path: '/tmp/b.webm',
  verify_resume: true,
  out_file: out,
}));
console.log(JSON.stringify({ work, out }));
`,
    { mode: 0o755 },
  );

  const resumeScript = path.join(dir, 'fake-resume.sh');
  fs.writeFileSync(
    resumeScript,
    `#!/usr/bin/env bash
set -euo pipefail
echo "fake resume passed"
exit 0
`,
    { mode: 0o755 },
  );

  const swiftPath = path.join(binDir, 'swift');
  fs.writeFileSync(
    swiftPath,
    `#!/usr/bin/env bash
set -euo pipefail
cat >/dev/null
echo "Verified isExcludedFromBackup == true for $2"
exit 0
`,
    { mode: 0o755 },
  );

  const fakeAppRunner = path.join(dir, 'fake-app.mjs');
  fs.writeFileSync(
    fakeAppRunner,
    `#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';

const readyFile = process.env.SIMCTL_CHILD_LEGION_APP_READY_FILE;
const specFile = process.env.SIMCTL_CHILD_LEGION_APP_MEDIA_CHECK_FILE;
const spec = JSON.parse(fs.readFileSync(specFile, 'utf8'));
const outFile = spec.out_file;
const attemptCfg = JSON.parse(process.argv[2]);
const appDataDir = process.argv[3];
const staleMarker = process.argv[4];

function writeAtomic(file, obj) {
  const tmp = file + '.tmp';
  fs.writeFileSync(tmp, JSON.stringify(obj, null, 2) + '\\n');
  fs.renameSync(tmp, file);
}

const hadStaleData = fs.existsSync(staleMarker);
fs.mkdirSync(appDataDir, { recursive: true });
if (attemptCfg.seedStaleData) {
  fs.writeFileSync(staleMarker, JSON.stringify({ stale: true }));
}

async function run() {
  if (typeof attemptCfg.readyDelayMs === 'number' && attemptCfg.readyDelayMs >= 0) {
    await new Promise((r) => setTimeout(r, attemptCfg.readyDelayMs));
    fs.writeFileSync(readyFile, 'legion-app: interface ready\\n');
  }
  if (attemptCfg.requireCleanData && hadStaleData) {
    writeAtomic(outFile, {
      stage: 'pairing',
      pair_ok: false,
      ok: false,
      image_width: 0,
      image_height: 0,
      video_width: 0,
      video_height: 0,
      error: 'Stale app data was NOT reset before attempt 2!',
    });
    await new Promise((r) => setTimeout(r, 15000));
    return;
  }
  for (const step of attemptCfg.steps || []) {
    if (step.delayMs > 0) {
      await new Promise((r) => setTimeout(r, step.delayMs));
    }
    writeAtomic(outFile, step.report);
  }
  // Stay alive until terminated by the harness.
  await new Promise((r) => setTimeout(r, 15000));
}

run().catch((err) => {
  console.error(err);
  process.exit(1);
});
`,
    { mode: 0o755 },
  );

  const xcrunPath = path.join(binDir, 'xcrun');
  fs.writeFileSync(
    xcrunPath,
    `#!/usr/bin/env node
const fs = require('node:fs');
const { spawn } = require('node:child_process');
const args = process.argv.slice(2);
const callLog = ${JSON.stringify(callLog)};
const stateFile = ${JSON.stringify(stateFile)};
const dataDir = ${JSON.stringify(dataDir)};
const appDataDir = ${JSON.stringify(path.join(dataDir, 'Library', 'Application Support', 'com.x1f4r.legion'))};
const staleMarker = ${JSON.stringify(staleMarker)};
const fakeAppRunner = ${JSON.stringify(fakeAppRunner)};
const attempts = ${JSON.stringify(attempts)};

fs.appendFileSync(callLog, args.join(' ') + '\\n');

if (args[0] !== 'simctl') process.exit(0);
const sub = args[1];

if (sub === 'get_app_container') {
  console.log(dataDir);
  process.exit(0);
}

if (sub === 'io' && args[3] === 'screenshot') {
  const out = args[4];
  fs.writeFileSync(out, Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]));
  process.exit(0);
}

if (sub === 'terminate') {
  const st = JSON.parse(fs.readFileSync(stateFile, 'utf8'));
  if (st.lastPid) {
    try { process.kill(st.lastPid, 'SIGKILL'); } catch {}
  }
  process.exit(0);
}

if (sub === 'launch') {
  const st = JSON.parse(fs.readFileSync(stateFile, 'utf8'));
  st.launchCount += 1;
  const cfg = attempts[st.launchCount - 1] || {};
  const child = spawn(process.execPath, [fakeAppRunner, JSON.stringify(cfg), appDataDir, staleMarker], {
    detached: true,
    stdio: 'ignore',
    env: process.env,
  });
  child.unref();
  st.lastPid = child.pid;
  fs.writeFileSync(stateFile, JSON.stringify(st));
  console.log('com.x1f4r.legion: ' + child.pid);
  process.exit(0);
}

process.exit(0);
`,
    { mode: 0o755 },
  );

  return { dir, binDir, dataDir, shotsDir, runnerTemp, simApp, legionBin, fixtureScript, resumeScript, callLog, staleMarker };
}

function runCheckScript(envSetup, { readyTimeout = 3, pairTimeout = 3, mediaTimeout = 2, pollInterval = 0.1 } = {}) {
  return spawnSync(
    'bash',
    [
      SCRIPT,
      '--kind',
      'iphone',
      '--udid',
      'FAKE-IPHONE-UDID',
      '--dev-name',
      'iPhone_16',
      '--sim-app',
      envSetup.simApp,
      '--legion-bin',
      envSetup.legionBin,
      '--runner-temp',
      envSetup.runnerTemp,
      '--shots-dir',
      envSetup.shotsDir,
      '--ready-timeout',
      String(readyTimeout),
      '--pair-timeout',
      String(pairTimeout),
      '--media-timeout',
      String(mediaTimeout),
      '--poll-interval',
      String(pollInterval),
      '--retry-sleep',
      '0',
      '--screenshot-settle',
      '0',
      '--fixture-script',
      envSetup.fixtureScript,
      '--resume-script',
      envSetup.resumeScript,
    ],
    {
      encoding: 'utf8',
      env: {
        ...process.env,
        PATH: `${envSetup.binDir}${path.delimiter}${process.env.PATH || ''}`,
      },
    },
  );
}

test('ios-sim-check.sh has valid bash syntax and is wired into legion-app.yml', () => {
  execFileSync('bash', ['-n', SCRIPT], { encoding: 'utf8' });
  const workflow = fs.readFileSync(WORKFLOW, 'utf8');
  assert.match(workflow, /bash scripts\/ios-sim-check\.sh/);
  assert.match(workflow, /'scripts\/ios-sim-check\.sh'/);
  assert.match(fs.readFileSync(SCRIPT, 'utf8'), /ios-sim-resume\.sh/);
});

test('ios-sim-check.sh gives pairing its own clock before starting the media clock (pairing slow then fine)', (t) => {
  if (process.platform === 'win32') return;
  // Pairing takes 2.3s (> mediaTimeout=2s), then media takes 0.25s (well inside a 2s integer
  // second window even across a second boundary). Because media starts its own clock only after
  // pair_ok=true, the run must pass cleanly on attempt 1/2.
  const envSetup = makeFakeSimEnv(t, {
    attempts: [
      {
        readyDelayMs: 50,
        steps: [
          {
            delayMs: 50,
            report: { stage: 'pairing', pair_ok: false, ok: false, image_width: 0, video_width: 0, error: null },
          },
          {
            delayMs: 2250,
            report: { stage: 'media', pair_ok: true, ok: false, image_width: 0, video_width: 0, error: null },
          },
          {
            delayMs: 250,
            report: {
              stage: 'resume',
              pair_ok: true,
              ok: true,
              image_width: 16,
              image_height: 16,
              video_width: 64,
              video_height: 64,
              ipc_isolation_ok: true,
              resume_ok: false,
            },
          },
        ],
      },
    ],
  });

  const res = runCheckScript(envSetup, { readyTimeout: 3, pairTimeout: 6, mediaTimeout: 2 });
  assert.equal(res.status, 0, res.stdout + res.stderr);
  assert.match(res.stdout, /\[attempt 1\/2\]/);
  assert.doesNotMatch(res.stdout, /\[attempt 2\/2\]/);
  assert.match(res.stdout, /iphone pairing completed after/);
  assert.match(res.stdout, /iphone initial media check passed after/);
  assert.ok(fs.existsSync(path.join(envSetup.shotsDir, 'iphone-dark.png')));
  assert.ok(fs.existsSync(path.join(envSetup.shotsDir, 'iphone-light.png')));
});

test('ios-sim-check.sh retries once with app-data reset and fails cleanly naming pairing when pairing fails twice', (t) => {
  if (process.platform === 'win32') return;
  const pairFailStep = {
    delayMs: 100,
    report: {
      stage: 'pairing',
      pair_ok: false,
      ok: false,
      image_width: 0,
      image_height: 0,
      video_width: 0,
      video_height: 0,
      error: 'Timed out waiting for tower to confirm enrolment after 45s.',
    },
  };

  const envSetup = makeFakeSimEnv(t, {
    attempts: [
      { readyDelayMs: 50, seedStaleData: true, steps: [pairFailStep] },
      { readyDelayMs: 50, steps: [pairFailStep] },
    ],
  });

  const res = runCheckScript(envSetup);
  assert.equal(res.status, 1, res.stdout + res.stderr);
  assert.match(res.stdout, /\[attempt 1\/2\]/);
  assert.match(res.stdout, /Failed iphone at stage: pairing \(ready=1, pairing=0, media_check=0, resume=0\)/);
  assert.match(res.stderr, /Retrying iphone simulator once \(attempt 2\/2\)/);
  assert.match(res.stdout, /Resetting app data for com\.x1f4r\.legion on iphone simulator before attempt 2\/2/);
  assert.match(res.stdout, /\[attempt 2\/2\]/);
  assert.doesNotMatch(res.stdout, /\[attempt 3\/2\]/);

  const calls = fs.readFileSync(envSetup.callLog, 'utf8');
  assert.match(calls, /simctl uninstall FAKE-IPHONE-UDID com\.x1f4r\.legion/);
});

test('ios-sim-check.sh resets app data on simulator and passes on attempt 2/2 after a pre-media pairing failure', (t) => {
  if (process.platform === 'win32') return;
  const envSetup = makeFakeSimEnv(t, {
    attempts: [
      {
        readyDelayMs: 50,
        seedStaleData: true,
        steps: [
          {
            delayMs: 100,
            report: {
              stage: 'pairing',
              pair_ok: false,
              ok: false,
              image_width: 0,
              image_height: 0,
              video_width: 0,
              video_height: 0,
              error: 'Timed out waiting for tower to confirm enrolment after 45s.',
            },
          },
        ],
      },
      {
        readyDelayMs: 50,
        requireCleanData: true,
        steps: [
          {
            delayMs: 80,
            report: { stage: 'media', pair_ok: true, ok: false, image_width: 0, video_width: 0, error: null },
          },
          {
            delayMs: 80,
            report: {
              stage: 'resume',
              pair_ok: true,
              ok: true,
              image_width: 16,
              image_height: 16,
              video_width: 64,
              video_height: 64,
              ipc_isolation_ok: true,
              resume_ok: false,
            },
          },
        ],
      },
    ],
  });

  const res = runCheckScript(envSetup);
  assert.equal(res.status, 0, res.stdout + res.stderr);
  assert.match(res.stdout, /\[attempt 1\/2\]/);
  assert.match(res.stdout, /Failed iphone at stage: pairing \(ready=1, pairing=0, media_check=0, resume=0\)/);
  assert.match(res.stderr, /Retrying iphone simulator once \(attempt 2\/2\)/);
  assert.match(res.stdout, /Resetting app data for com\.x1f4r\.legion on iphone simulator before attempt 2\/2/);
  assert.match(res.stdout, /\[attempt 2\/2\]/);
  assert.match(res.stdout, /iphone pairing completed after/);
  assert.match(res.stdout, /iphone initial media check passed after/);
  assert.equal(fs.existsSync(envSetup.staleMarker), false);
});

test('ios-sim-check.sh does not retry when failure occurs after media stage was reached', (t) => {
  if (process.platform === 'win32') return;
  const envSetup = makeFakeSimEnv(t, {
    attempts: [
      {
        readyDelayMs: 50,
        steps: [
          {
            delayMs: 50,
            report: { stage: 'media', pair_ok: true, ok: false, image_width: 0, video_width: 0, error: null },
          },
          {
            delayMs: 80,
            report: {
              stage: 'media',
              pair_ok: true,
              ok: false,
              image_width: 0,
              image_height: 0,
              video_width: 0,
              video_height: 0,
              error: 'Image decode failed',
            },
          },
        ],
      },
    ],
  });

  const res = runCheckScript(envSetup);
  assert.equal(res.status, 1, res.stdout + res.stderr);
  assert.match(res.stdout, /Failed iphone at stage: initial media check \(ready=1, pairing=1, media_check=0, resume=0\)/);
  assert.doesNotMatch(res.stdout, /\[attempt 2\/2\]/);
});
