import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';

const REPO_ROOT = path.resolve(import.meta.dirname, '..');
const SCRIPT = path.join(REPO_ROOT, 'scripts', 'ios-sim-resume.sh');
const WORKFLOW = path.join(REPO_ROOT, '.github', 'workflows', 'legion-app.yml');

function makeFakeEnv(t, { frontmostOnPoll = 1, passOnLegionCycle = 1, failOnLegionCycle = 0 } = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ios-sim-resume-test-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));

  const binDir = path.join(dir, 'bin');
  fs.mkdirSync(binDir);
  const mediaOut = path.join(dir, 'media-result.json');
  const callLog = path.join(dir, 'xcrun.log');
  const stateFile = path.join(dir, 'state.json');

  fs.writeFileSync(
    mediaOut,
    JSON.stringify({
      ok: true,
      image_width: 16,
      image_height: 16,
      video_width: 64,
      video_height: 64,
      ipc_isolation_ok: true,
      resume_ok: false,
    }),
  );
  fs.writeFileSync(
    stateFile,
    JSON.stringify({
      prefsLaunches: 0,
      legionLaunches: 0,
      logPollsInCycle: 0,
    }),
  );

  // Fake `xcrun` executable simulating `simctl launch` and `simctl spawn ... log show`.
  const xcrunPath = path.join(binDir, 'xcrun');
  fs.writeFileSync(
    xcrunPath,
    `#!/usr/bin/env node
const fs = require("node:fs");
const args = process.argv.slice(2);
const callLog = ${JSON.stringify(callLog)};
const stateFile = ${JSON.stringify(stateFile)};
const mediaOut = ${JSON.stringify(mediaOut)};
const frontmostOnPoll = ${JSON.stringify(frontmostOnPoll)};
const passOnLegionCycle = ${JSON.stringify(passOnLegionCycle)};
const failOnLegionCycle = ${JSON.stringify(failOnLegionCycle)};

fs.appendFileSync(callLog, args.join(" ") + "\\n");
const st = JSON.parse(fs.readFileSync(stateFile, "utf8"));

if (args[0] === "simctl" && args[1] === "launch") {
  const bundle = args[3];
  if (bundle === "com.apple.Preferences") {
    st.prefsLaunches += 1;
    st.logPollsInCycle = 0;
    fs.writeFileSync(stateFile, JSON.stringify(st));
    console.log("com.apple.Preferences: " + (2000 + st.prefsLaunches));
    process.exit(0);
  }
  if (bundle === "com.x1f4r.legion") {
    st.legionLaunches += 1;
    fs.writeFileSync(stateFile, JSON.stringify(st));
    if (failOnLegionCycle > 0 && st.legionLaunches === failOnLegionCycle) {
      fs.writeFileSync(
        mediaOut,
        JSON.stringify({
          ok: false,
          image_width: 16,
          video_width: 64,
          resume_ok: false,
          error: "Simulated post-resume image error",
        }),
      );
    } else if (passOnLegionCycle > 0 && st.legionLaunches === passOnLegionCycle) {
      fs.writeFileSync(
        mediaOut,
        JSON.stringify({
          ok: true,
          image_width: 16,
          image_height: 16,
          video_width: 64,
          video_height: 64,
          ipc_isolation_ok: true,
          resume_ok: true,
          resume_trigger: "tauri://resumed",
        }),
      );
    }
    console.log("com.x1f4r.legion: 13595");
    process.exit(0);
  }
}

if (args[0] === "simctl" && args[1] === "spawn" && args[3] === "log" && args[4] === "show") {
  st.logPollsInCycle += 1;
  fs.writeFileSync(stateFile, JSON.stringify(st));
  if (frontmostOnPoll > 0 && st.logPollsInCycle >= frontmostOnPoll) {
    console.log("2026-11-25 16:00:01.000 Df SpringBoard[1:1] [com.apple.runningboard:monitor] Received state update for 2001 (app<com.apple.Preferences((null))>, running-active-Visible");
  }
  process.exit(0);
}

process.exit(0);
`,
    { mode: 0o755 },
  );

  return { dir, binDir, mediaOut, callLog };
}

function runResumeScript(envSetup, { frontmostTimeout = 5, waitSecs = 1, extraArgs = [] } = {}) {
  return spawnSync(
    'bash',
    [
      SCRIPT,
      '--kind',
      'ipad',
      '--udid',
      'FAKE-UDID-1234',
      '--media-out',
      envSetup.mediaOut,
      '--cycles',
      '3',
      '--wait-secs',
      String(waitSecs),
      '--frontmost-timeout',
      String(frontmostTimeout),
      '--poll-interval',
      '0.1',
      '--frontmost-poll-interval',
      '0.1',
      '--settle-secs',
      '0',
      ...extraArgs,
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

test('ios-sim-resume.sh has valid bash syntax and is wired into legion-app.yml', () => {
  execFileSync('bash', ['-n', SCRIPT], { encoding: 'utf8' });
  const workflow = fs.readFileSync(WORKFLOW, 'utf8');
  assert.match(workflow, /bash scripts\/ios-sim-resume\.sh/);
  assert.doesNotMatch(workflow, /com\.apple\.Preferences[^\n]*\n\s*sleep 3\n\s*xcrun simctl launch/);
});

test('ios-sim-resume.sh waits for Preferences frontmost confirmation before returning to Legion', (t) => {
  if (process.platform === 'win32') return;
  const envSetup = makeFakeEnv(t, { frontmostOnPoll: 2, passOnLegionCycle: 1 });
  const res = runResumeScript(envSetup);

  assert.equal(res.status, 0, res.stdout + res.stderr);
  assert.match(res.stdout, /com\.apple\.Preferences confirmed frontmost/);
  assert.match(res.stdout, /passed on cycle 1\/3.*trigger=tauri:\/\/resumed/);

  const calls = fs.readFileSync(envSetup.callLog, 'utf8').trim().split('\n');
  assert.equal(calls[0], 'simctl launch FAKE-UDID-1234 com.apple.Preferences');
  assert.match(calls[1], /^simctl spawn FAKE-UDID-1234 log show --start /);
  assert.match(calls[2], /^simctl spawn FAKE-UDID-1234 log show --start /);
  assert.equal(calls[3], 'simctl launch FAKE-UDID-1234 com.x1f4r.legion');
});

test('ios-sim-resume.sh retries bounded cycles and passes when cycle 2 reports resume_ok', (t) => {
  if (process.platform === 'win32') return;
  const envSetup = makeFakeEnv(t, { frontmostOnPoll: 1, passOnLegionCycle: 2 });
  const res = runResumeScript(envSetup);

  assert.equal(res.status, 0, res.stdout + res.stderr);
  assert.match(res.stdout, /cycle 1\/3\)\.\.\./);
  assert.match(res.stdout, /cycle 1\/3: resume_ok still pending after/);
  assert.match(res.stdout, /cycle 2\/3\)\.\.\./);
  assert.match(res.stdout, /passed on cycle 2\/3/);
});

test('ios-sim-resume.sh aborts immediately when media-result reports ok=false', (t) => {
  if (process.platform === 'win32') return;
  const envSetup = makeFakeEnv(t, { frontmostOnPoll: 1, passOnLegionCycle: 0, failOnLegionCycle: 1 });
  const res = runResumeScript(envSetup);

  assert.equal(res.status, 1, res.stdout + res.stderr);
  assert.match(res.stdout, /ipad failed at stage: post-resume media check \(cycle 1\/3.*Simulated post-resume image error\)/);
  assert.doesNotMatch(res.stdout, /cycle 2\/3/);
});

test('ios-sim-resume.sh fails cleanly after 3 cycles and names the missing stage', (t) => {
  if (process.platform === 'win32') return;

  // Case A: frontmost was confirmed, but resume_ok never became true.
  const envPending = makeFakeEnv(t, { frontmostOnPoll: 1, passOnLegionCycle: 0 });
  const resPending = runResumeScript(envPending);
  assert.equal(resPending.status, 1, resPending.stdout + resPending.stderr);
  assert.match(resPending.stdout, /cycle 1\/3: resume_ok still pending/);
  assert.match(resPending.stdout, /cycle 2\/3: resume_ok still pending/);
  assert.match(resPending.stdout, /cycle 3\/3: resume_ok still pending/);
  assert.match(resPending.stdout, /ipad failed at stage: post-resume result \(resume_ok never turned true across 3 cycles of 1s\)/);

  // Case B: Preferences never confirmed frontmost across all 3 cycles.
  const envNoFrontmost = makeFakeEnv(t, { frontmostOnPoll: 0, passOnLegionCycle: 0 });
  const resNoFrontmost = runResumeScript(envNoFrontmost, { frontmostTimeout: 1 });
  assert.equal(resNoFrontmost.status, 1, resNoFrontmost.stdout + resNoFrontmost.stderr);
  assert.match(
    resNoFrontmost.stdout,
    /ipad failed at stage: background switch \(com\.apple\.Preferences was never confirmed frontmost across 3 cycles; resume_ok stayed false\)/,
  );
});
